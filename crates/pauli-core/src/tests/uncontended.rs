//! Uncontended dedicated-hart slice extension and early boot-console breaks.
//! Ownership/Lock Ordering: sequential mock owners drive real dispatcher logic.
//! Invariants: extension applies only to sole Ready lanes and reverts immediately on contention.
use super::model::{Allocation, Arch, Fixture, IDLE_DEADLINE, IDLE_ENTRIES, NOW, OWNER, TestBoard};
use crate::{
    Result,
    arch::ExitKind,
    board::{Board, BoardInfo, BootImages, VmResources},
    manager::VmStore,
    memory::pool::DomainPool,
    runtime::{Dispatcher, VmRuntime},
    vm::State,
};
use pauli_config::{BootConfig, GuestBoardLayout, VmConfig};
use pauli_hal::device::{HostBlockDevice, HostNetDevice, HostRtcDevice, MmioAccess};
use std::boxed::Box;

struct UncontendedBoard(TestBoard);

impl Board for UncontendedBoard {
    const IRQ_DRIVEN_CONSOLE: bool = true;
    const UNCONTENDED_QUANTUM_US: Option<u64> = Some(1_000);
    const IDLE_USE_GUEST_DEADLINE: bool = true;

    fn info(&self) -> BoardInfo {
        self.0.info()
    }
    fn load_boot_config(&self) -> Result<&'static BootConfig> {
        self.0.load_boot_config()
    }
    #[cfg(feature = "guest-control")]
    fn boot_service_for(&self, slot: usize) -> Option<usize> {
        self.0.boot_service_for(slot)
    }
    fn take_resources(&self, slot: usize, config: &VmConfig) -> Result<VmResources> {
        self.0.take_resources(slot, config)
    }
    fn domain_pool(&self) -> Option<&DomainPool> {
        self.0.domain_pool()
    }
    fn resolve_block(&self, slot: u8) -> Result<&'static dyn HostBlockDevice> {
        self.0.resolve_block(slot)
    }
    fn resolve_uplink_net(&self) -> Result<&'static dyn HostNetDevice> {
        self.0.resolve_uplink_net()
    }
    fn rtc(&self) -> Option<&'static dyn HostRtcDevice> {
        self.0.rtc()
    }
    fn pop_console_rx(&self) -> Option<u8> {
        self.0.pop_console_rx()
    }
    fn direct_linux_console(&self) -> bool {
        self.0.direct_linux_console()
    }
    fn console_rx_queued(&self) -> bool {
        self.0.console_rx_queued()
    }
    fn service_devices(&self, now: u64) {
        self.0.service_devices(now)
    }
    fn shutdown(&self) -> ! {
        self.0.shutdown()
    }
}

struct ExtendedFixture {
    runtime: VmRuntime<Arch, UncontendedBoard>,
    _backing: [Allocation; 2],
}

impl ExtendedFixture {
    fn new(host_cpus: usize) -> Self {
        Self::with_config(host_cpus, |_, _| {})
    }

    fn with_config(host_cpus: usize, configure: impl Fn(usize, &mut VmConfig)) -> Self {
        let (board, backing, _) = Fixture::test_board(
            host_cpus,
            [1, 1],
            0,
            [None; pauli_config::MAX_DISKS],
            |slot, vm| {
                configure(slot, vm);
                BootImages {
                    kernel: &[0x31; 64],
                    initrd: &[],
                    dtb: &[0xd1; 64],
                }
            },
        );
        let board = Box::leak(Box::new(UncontendedBoard(board)));
        let runtime = VmRuntime::create(board, Box::leak(Box::new(VmStore::new()))).unwrap();
        Self {
            runtime,
            _backing: backing,
        }
    }

    fn steps(&self, slot: usize) -> u64 {
        self.runtime
            .manager
            .vm(slot)
            .unwrap()
            .cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
    }

    fn park_primary(&self, local: &mut Dispatcher<super::model::Timer>) {
        let rt = &self.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        let vm = rt.manager.vm(0).unwrap();
        let slot = vm.cpu(0).unwrap();
        slot.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Wait);
        assert!(rt.dispatch(local).unwrap());
        assert_eq!(slot.data.lock().state, crate::vm::slot::CpuState::Waiting);
        NOW.with(|n| n.set(1_000));
        slot.data.lock().context.as_mut().unwrap().deadline = Some(1_800);
    }

    fn start_compute_guest(&self, mmio: Option<MmioAccess>) {
        let rt = &self.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        let vm = rt.manager.vm(0).unwrap();
        let mut slot = vm.cpu(0).unwrap().data.lock();
        let cpu = slot.context.as_mut().unwrap();
        cpu.run_until_host_timer = true;
        cpu.mmio = mmio;
    }

    fn program(&self, address: u64, width: usize, value: usize) {
        let manager = &self.runtime.manager;
        let access = MmioAccess {
            gpa: address as usize,
            width,
            write: Some(value),
        };
        assert_eq!(
            manager
                .vm(0)
                .unwrap()
                .devices
                .access(access, || NOW.with(|n| n.get()), manager.link()),
            Some(0)
        );
    }

    fn idle_deadline(&self, local: &mut Dispatcher<super::model::Timer>) -> Option<u64> {
        IDLE_ENTRIES.with(|n| n.set(0));
        self.runtime.idle(local).unwrap();
        match IDLE_ENTRIES.with(|n| n.get()) {
            0 => None,
            1 => IDLE_DEADLINE.with(|d| d.get()),
            _ => panic!("multiple idle waits"),
        }
    }
}

#[test]
fn linux_guest_deadline_idle_opt_in_sleeps_past_fast_poll() {
    let f = ExtendedFixture::new(1);
    let mut local = Dispatcher::new();
    f.park_primary(&mut local);
    let vm = f.runtime.manager.vm(0).unwrap();
    assert_eq!(f.idle_deadline(&mut local), Some(1_800));
    vm.devices
        .io
        .lock()
        .console
        .access(0, Some(b'x'.into()))
        .unwrap();
    assert_eq!(f.idle_deadline(&mut local), Some(1_100));
    assert_eq!(vm.devices.io.lock().console.drain(&mut [0]), 1);
    assert_eq!(f.idle_deadline(&mut local), Some(1_800));
}

#[test]
fn rtc_alarm_bounds_compute_even_when_programmed_during_the_slice() {
    let rtc = GuestBoardLayout::DEFAULT.rtc.base;
    for during in [false, true] {
        let f = ExtendedFixture::new(1);
        let alarm = MmioAccess {
            gpa: (rtc + 8) as usize,
            width: 4,
            write: Some(250_000),
        };
        f.start_compute_guest(during.then_some(alarm));
        f.program(rtc + 0x10, 4, 1);
        if !during {
            f.program(rtc + 8, 4, 250_000);
        }
        assert!(f.runtime.dispatch(&mut Dispatcher::new()).unwrap());
        assert_eq!(NOW.with(|n| n.get()), 250);
        assert_eq!(f.steps(0), if during { 2 } else { 1 });
        f.runtime.manager.service(f.runtime.board);
        assert!(
            f.runtime
                .manager
                .vm(0)
                .unwrap()
                .devices
                .io
                .lock()
                .rtc
                .device
                .irq_pending()
        );
    }
}

#[test]
fn queued_console_tx_and_rx_timeout_limit_the_programmed_host_timer() {
    let uart = GuestBoardLayout::DEFAULT.uart.base;
    let f = ExtendedFixture::new(1);
    f.start_compute_guest(Some(MmioAccess {
        gpa: uart as usize,
        width: 1,
        write: Some(b'x'.into()),
    }));
    let mut local = Dispatcher::new();
    assert!(f.runtime.dispatch(&mut local).unwrap());
    assert_eq!(NOW.with(|n| n.get()), 300);
    assert_eq!(f.steps(0), 2); // One TX exit followed by uninterrupted compute.
    assert!(f.runtime.dispatch(&mut local).unwrap()); // Existing undrained TX.
    assert_eq!(NOW.with(|n| n.get()), 600);
    let vm = f.runtime.manager.vm(0).unwrap();
    assert_eq!(vm.devices.io.lock().console.drain(&mut [0]), 1);
    f.program(uart + 2, 1, 0x41); // FIFO trigger 4.
    f.program(uart + 1, 1, 1); // Enable RX IRQ.
    assert!(vm.devices.receive_console(b'a', || 600));
    NOW.with(|n| n.set(900));
    assert!(f.runtime.dispatch(&mut local).unwrap());
    assert_eq!(NOW.with(|n| n.get()), 1600);
    f.runtime.manager.service(f.runtime.board);
    assert!(vm.devices.io.lock().console.uart.rx_irq_pending());
}

#[test]
fn already_due_rtc_returns_to_monitor_without_entering_guest_or_wfi() {
    let rtc = GuestBoardLayout::DEFAULT.rtc.base;
    let f = ExtendedFixture::new(1);
    f.start_compute_guest(None);
    f.program(rtc + 0x10, 4, 1);
    f.program(rtc + 8, 4, 150_000);
    NOW.with(|n| n.set(150));
    let mut local = Dispatcher::new();
    assert!(!f.runtime.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0), 0);
    assert_eq!(f.idle_deadline(&mut local), None);
    f.runtime.manager.service(f.runtime.board);
    assert!(f.runtime.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0), 1);
}

#[test]
fn mixed_payload_keeps_maintenance_even_on_an_opted_in_board() {
    let f = ExtendedFixture::with_config(1, |slot, vm| {
        if slot == 1 {
            vm.memory_size = 32 * 1024;
            vm.firmware = Some(pauli_config::FirmwareLayout {
                rx_pages: 1,
                ro_pages: 0,
            });
        }
    });
    let mut local = Dispatcher::new();
    f.park_primary(&mut local);
    assert_eq!(f.idle_deadline(&mut local), Some(1_100));
}

#[test]
fn direct_input_backpressure_preserves_native_rx_and_allows_idle() {
    let f = ExtendedFixture::new(1);
    let mut local = Dispatcher::new();
    f.park_primary(&mut local);
    let rt = &f.runtime;
    rt.board
        .0
        .direct_linux
        .store(true, core::sync::atomic::Ordering::Relaxed);
    let mut monitor = crate::monitor::Monitor::new();
    monitor.welcome(rt);
    rt.board.0.input.lock().extend(b"ab");
    monitor.poll(rt);
    assert_eq!(rt.board.0.input.lock().front(), Some(&b'b'));
    assert_eq!(f.idle_deadline(&mut local), Some(1_800));
    let vm = rt.manager.vm(0).unwrap();
    assert_eq!(
        vm.devices.io.lock().console.access(0, None),
        Some(b'a'.into())
    );
    assert_eq!(f.idle_deadline(&mut local), None);
    monitor.poll(rt);
    assert_eq!(
        vm.devices.io.lock().console.access(0, None),
        Some(b'b'.into())
    );
    assert!(rt.board.0.input.lock().is_empty());
}

#[test]
fn a_full_primary_fifo_does_not_interrupt_the_extended_slice() {
    let f = ExtendedFixture::new(1);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    rt.manager.start(rt.board, 0).unwrap();
    rt.board
        .0
        .direct_linux
        .store(true, core::sync::atomic::Ordering::Relaxed);
    let vm = rt.manager.vm(0).unwrap();
    assert!(vm.devices.io.lock().console.uart.receive(b'a'));
    rt.board.0.input.lock().push_back(b'b');
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0), 10);
    assert_eq!(
        vm.devices.io.lock().console.access(0, None),
        Some(b'a'.into())
    );
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0), 11);
}

#[test]
fn sole_lane_extends_slice_reverts_under_contention_and_restores_on_stop() {
    let f = ExtendedFixture::new(1);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();

    // 1. Sole Ready VM receives the extended 1000 us slice (10 x 100 us steps).
    rt.manager.start(rt.board, 0).unwrap();
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0), 10);

    // 2. Starting a second Ready VM on the same hart immediately reverts both to 300 us (3 steps).
    rt.manager.start(rt.board, 1).unwrap();
    let before0 = f.steps(0);
    let before1 = f.steps(1);
    assert!(rt.dispatch(&mut local).unwrap());
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0) - before0, 3);
    assert_eq!(f.steps(1) - before1, 3);

    // Even if the peer parks in WFI (still Ready), the hart stays contended (3 steps).
    rt.manager
        .vm(1)
        .unwrap()
        .cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    let before1 = f.steps(1);
    assert!(rt.dispatch(&mut local).unwrap()); // Beta parks after 1 step.
    assert_eq!(f.steps(1) - before1, 1);
    let before0 = f.steps(0);
    assert!(rt.dispatch(&mut local).unwrap()); // Alpha sees peer Ready/Waiting -> 3 steps.
    assert_eq!(f.steps(0) - before0, 3);

    // 3. Stopping and joining Beta restores the extended 1000 us slice to Alpha.
    let beta = rt.manager.vm(1).unwrap();
    beta.host_stop();
    let _ = rt.dispatch(&mut local).unwrap(); // Mail scan retires Beta; Alpha runs in this pass.
    assert!(beta.try_join(0, rt.manager.link()).unwrap());
    assert_eq!(beta.lifecycle.state(), State::Stopped);
    let before0 = f.steps(0);
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0) - before0, 10);
}

#[test]
fn boot_hart_console_rx_and_tx_yield_extended_slice_promptly() {
    let f = ExtendedFixture::new(1);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    rt.manager.start(rt.board, 0).unwrap();

    // Queued native RX byte breaks the extended boot-hart slice on the very first exit.
    rt.board.0.input.lock().push_back(b'k');
    let before = f.steps(0);
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0) - before, 1);
    assert_eq!(rt.board.pop_console_rx(), Some(b'k'));

    // Guest UART TX write causes the extended slice to yield once vm.quantum (300 us) elapses.
    NOW.with(|n| n.set(0));
    rt.manager
        .vm(0)
        .unwrap()
        .cpu(0)
        .unwrap()
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .mmio = Some(MmioAccess {
        gpa: GuestBoardLayout::DEFAULT.uart.base as usize,
        width: 1,
        write: Some(b'A' as usize),
    });
    let before = f.steps(0);
    assert!(rt.dispatch(&mut local).unwrap());
    assert_eq!(f.steps(0) - before, 3);
}

#[test]
fn remote_worker_retains_extended_slice_even_when_boot_console_is_active() {
    let f = ExtendedFixture::new(2);
    let rt = &f.runtime;
    rt.manager.start(rt.board, 0).unwrap(); // slot 0 -> owner 0 (boot)
    rt.manager.start(rt.board, 1).unwrap(); // slot 1 -> owner 1 (remote worker)

    rt.board.0.input.lock().push_back(b'z');
    OWNER.with(|o| o.set(1));
    let mut worker = Dispatcher::new();
    assert!(rt.dispatch(&mut worker).unwrap());
    assert_eq!(f.steps(1), 10);
}
