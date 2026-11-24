//! Uncontended dedicated-hart slice extension and early boot-console breaks.
//! Ownership/Lock Ordering: sequential mock owners drive real dispatcher logic.
//! Invariants: extension applies only to sole Ready lanes and reverts immediately on contention.
use super::model::{Allocation, Arch, Fixture, NOW, OWNER, TestBoard};
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
        let (board, backing, _) =
            Fixture::test_board(host_cpus, [1, 1], 0, [None; pauli_config::MAX_DISKS], |_, _| {
                BootImages {
                    kernel: &[0x31; 64],
                    initrd: &[],
                    dtb: &[0xd1; 64],
                }
            });
        let board = Box::leak(Box::new(UncontendedBoard(board)));
        let runtime =
            VmRuntime::create(board, Box::leak(Box::new(VmStore::new()))).unwrap();
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
