//! Copied frames use real split rings, MMIO exits, independent RAM and owner wakes.
use super::model::{Arch, Fixture, IDLE_DEADLINE, KICKED_OWNERS, NOW, OWNER, REJECT_KICK_ONCE};
use crate::{
    arch::ExitKind,
    runtime::Dispatcher,
    vm::{Vm, slot::CpuState},
};
use core::sync::atomic::Ordering::Acquire;
use pauli_config::{CpuAffinity, FirmwareLayout, GuestBoardLayout, MAX_GUEST_VCPUS};
use pauli_devices::{
    memory::GuestMemory,
    net_link::Snapshot,
    virtio_net::{FRAME_MAX, HEADER},
};
use pauli_hal::device::MmioAccess;

const N: u16 = 32;
const NOTIFY: usize = 0x50;
const STATUS: usize = 0x60;
const ACK: usize = 0x64;

fn fixture(exclusive: bool) -> Fixture {
    Fixture::with_config(2, [1; 2], 0, |slot, vm| {
        let mut pins = [0; MAX_GUEST_VCPUS as usize];
        pins[0] = slot as u8;
        vm.cpu_affinity = CpuAffinity::Physical(pins);
        vm.net_peer = Some((1 - slot) as u8);
        vm.exclusive_cpus = exclusive && slot == 1;
        if slot == 1 {
            vm.memory_size = 1 << 20;
            vm.image.kernel_memory_size = 8192;
            vm.firmware = Some(FirmwareLayout {
                rx_pages: 1,
                ro_pages: 1,
            });
        }
    })
}
fn access(reg: usize, write: Option<usize>) -> MmioAccess {
    MmioAccess {
        gpa: GuestBoardLayout::DEFAULT.net.base as usize + reg,
        width: 4,
        write,
    }
}

pub(super) fn carrier(f: &Fixture, vm: &Vm<Arch>) -> bool {
    reg(f, vm, 0x104, None) & (1 << 16) != 0
}

/// Run on the Linux and firmware children created through the manager's lease API.
pub(super) fn verify_leased_duplex(f: &Fixture, a: &Vm<Arch>, b: &Vm<Arch>) {
    use std::{sync::Barrier, thread};
    for vm in [a, b] {
        assert!(vm.slot >= pauli_config::MAX_BOOT_VMS);
        setup(f, vm);
        let lease = vm.config.net_lease.unwrap();
        assert_eq!(reg(f, vm, 0x100, None), 0x06);
        assert_eq!(
            reg(f, vm, 0x104, None) & 0xffff,
            usize::from(lease.channel) | (usize::from(lease.side) << 8)
        );
        assert!(carrier(f, vm));
    }
    let receiver_cpu = b.cpu(0).unwrap();
    let mut dispatcher = Dispatcher::new();
    receiver_cpu
        .data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    OWNER.with(|o| o.set(receiver_cpu.owner));
    for _ in 0..4 {
        f.runtime.dispatch(&mut dispatcher).unwrap();
        if receiver_cpu.run_state() == CpuState::Waiting {
            break;
        }
    }
    assert_eq!(receiver_cpu.run_state(), CpuState::Waiting);
    // One notification from a different endpoint must complete the posted RX
    // and publish its interrupt without a management service call.
    post(b, 0, 0);
    post(a, 1, 0);
    let sender_owner = a.cpu(0).unwrap().owner;
    OWNER.with(|o| o.set(sender_owner));
    KICKED_OWNERS.with(|k| k.set(0));
    reg(f, a, NOTIFY, Some(1));
    assert_eq!(used(b, 0), 1);
    assert_eq!(reg(f, b, STATUS, None), 1);
    if sender_owner != receiver_cpu.owner {
        assert_ne!(
            KICKED_OWNERS.with(|k| k.get()) & (1 << receiver_cpu.owner),
            0
        );
    }
    OWNER.with(|o| o.set(receiver_cpu.owner));
    for _ in 0..4 {
        f.runtime.dispatch(&mut dispatcher).unwrap();
        if receiver_cpu.run_state() != CpuState::Waiting {
            break;
        }
    }
    assert_eq!(receiver_cpu.run_state(), CpuState::Runnable);
    post(a, 0, 0);
    post(b, 1, 0);
    reg(f, b, NOTIFY, Some(1));
    for vm in [a, b] {
        reg(f, vm, ACK, Some(3));
    }
    let m = &f.runtime.manager;
    let simultaneous = Barrier::new(2);
    for start in (1..65).step_by(8) {
        for vm in [a, b] {
            for seq in start..start + 8 {
                post(vm, 0, seq);
                post(vm, 1, seq);
            }
        }
        thread::scope(|scope| {
            for vm in [a, b] {
                let gate = &simultaneous;
                scope.spawn(move || {
                    OWNER.with(|o| o.set(vm.cpu(0).unwrap().owner));
                    gate.wait();
                    assert_eq!(vm.devices.access(access(NOTIFY, Some(1)), || 0, m), Some(0));
                });
            }
        });
        for (vm, peer) in [(a, b), (b, a)] {
            assert_eq!((used(vm, 0), used(vm, 1)), (start + 8, start + 8));
            let io = vm.devices.io.lock();
            for seq in start..start + 8 {
                let mut bytes = [0; 60];
                assert!(io.ram.read(buffer(vm, 0, seq) + HEADER as u64, &mut bytes));
                let mut expected = [seq as u8; 60];
                expected[1] = peer.slot as u8;
                assert_eq!(bytes, expected);
            }
            drop(io);
            reg(f, vm, ACK, Some(3));
        }
    }
    for vm in [a, b] {
        let s = snapshot(f, vm);
        assert_eq!((s.tx, s.rx, s.dropped, s.queued), (65, 65, 0, 0));
    }
    let mut monitor = crate::monitor::Monitor::new();
    f.runtime.board.input.lock().extend(b"vm status sender\n");
    let status = super::uart_capture::host_output(|| monitor.poll(&f.runtime));
    assert!(status.lines().any(|s|s=="VM 5 net-lease 2:0 up=true tx=65 rx=65 dropped=0 full=0 down=0 queued=0"),"{status}");
}
fn reg(f: &Fixture, vm: &Vm<Arch>, addr: usize, write: Option<usize>) -> usize {
    vm.devices
        .access(access(addr, write), || 0, &f.runtime.manager)
        .unwrap()
}
fn rings(vm: &Vm<Arch>, q: u16) -> u64 {
    vm.config.guest_base + 0x10_000 + u64::from(q) * 0x3000
}
fn buffer(vm: &Vm<Arch>, q: u16, index: u16) -> u64 {
    vm.config.guest_base + 0x20_000 + u64::from(q) * 0x10_000 + u64::from(index % N) * 2048
}
fn setup(f: &Fixture, vm: &Vm<Arch>) {
    for (r, v) in [
        (0x70, 1),
        (0x70, 3),
        (0x24, 0),
        (0x20, (1 << 5) | (1 << 16)),
        (0x24, 1),
        (0x20, 1),
        (0x70, 11),
    ] {
        reg(f, vm, r, Some(v));
    }
    for q in 0..2 {
        let addr = rings(vm, q);
        reg(f, vm, 0x30, Some(usize::from(q)));
        reg(f, vm, 0x38, Some(usize::from(N)));
        for (r, gpa) in [(0x80, addr), (0x90, addr + 4096), (0xa0, addr + 8192)] {
            reg(f, vm, r, Some(gpa as u32 as usize));
            reg(f, vm, r + 4, Some((gpa >> 32) as usize));
        }
        reg(f, vm, 0x44, Some(1));
    }
    reg(f, vm, 0x70, Some(15));
    reg(f, vm, ACK, Some(3));
}
fn post(vm: &Vm<Arch>, q: u16, seq: u16) {
    let base = rings(vm, q);
    let addr = buffer(vm, q, seq);
    let mut io = vm.devices.io.lock();
    let tx = q == 1;
    let len = if tx { HEADER + 60 } else { HEADER + FRAME_MAX };
    let mut desc = [0; 16];
    desc[..8].copy_from_slice(&addr.to_le_bytes());
    desc[8..12].copy_from_slice(&(len as u32).to_le_bytes());
    desc[12..14].copy_from_slice(&(if tx { 0u16 } else { 2u16 }).to_le_bytes());
    if tx {
        assert!(io.ram.write(addr, &[0; HEADER]));
        let mut payload = [seq as u8; 60];
        payload[1] = vm.slot as u8;
        assert!(io.ram.write(addr + HEADER as u64, &payload));
    }
    assert!(io.ram.write(base + 16 * u64::from(seq % N), &desc));
    assert!(io.ram.write(
        base + 4096 + 4 + 2 * u64::from(seq % N),
        &(seq % N).to_le_bytes()
    ));
    assert!(
        io.ram
            .write(base + 4098, &seq.wrapping_add(1).to_le_bytes())
    );
}
fn used(vm: &Vm<Arch>, q: u16) -> u16 {
    let mut bytes = [0; 2];
    assert!(
        vm.devices
            .io
            .lock()
            .ram
            .read(rings(vm, q) + 8194, &mut bytes)
    );
    u16::from_le_bytes(bytes)
}
fn snapshot(f: &Fixture, vm: &Vm<Arch>) -> Snapshot {
    vm.devices
        .io
        .lock()
        .net
        .as_ref()
        .unwrap()
        .peer_snapshot(f.runtime.manager.link())
        .unwrap()
}

#[test]
fn queue_notify_completes_parked_peer_rx_before_any_service_poll() {
    for exclusive in [false, true] {
        let f = fixture(exclusive);
        let rt = &f.runtime;
        rt.manager.start(rt.board, 0).unwrap();
        rt.manager.start(rt.board, 1).unwrap();
        let sender = rt.manager.vm(0).unwrap();
        let peer = rt.manager.vm(1).unwrap();
        setup(&f, &sender);
        setup(&f, &peer);
        let cpu = peer.cpu(0).unwrap();
        let mut worker = Dispatcher::new();
        cpu.data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .script
            .push_back(ExitKind::Wait);
        OWNER.with(|o| o.set(cpu.owner));
        assert!(rt.dispatch(&mut worker).unwrap());
        assert_eq!(cpu.run_state(), CpuState::Waiting);
        let now = NOW.with(|n| n.get());
        rt.idle(&mut worker).unwrap();
        assert_eq!(
            IDLE_DEADLINE.with(|d| d.get()),
            (!exclusive).then_some(now + 100)
        );
        for i in 0..20 {
            post(&peer, 0, i);
            post(&sender, 1, i);
        }
        sender
            .cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_mut()
            .unwrap()
            .mmio = Some(access(NOTIFY, Some(1)));
        OWNER.with(|o| o.set(0));
        KICKED_OWNERS.with(|k| k.set(0));
        if exclusive {
            REJECT_KICK_ONCE.with(|k| k.set(Some(cpu.owner)));
        }
        assert!(rt.dispatch(&mut Dispatcher::new()).unwrap());
        // Exactly one fast pass (sixteen frames) and one receiver line transition.
        assert_eq!((used(&sender, 1), used(&peer, 0)), (16, 16));
        assert_eq!(reg(&f, &peer, STATUS, None), 1);
        assert_eq!(snapshot(&f, &sender).tx, 16);
        assert_eq!(snapshot(&f, &peer).rx, 16);
        assert_ne!(KICKED_OWNERS.with(|k| k.get()) & (1 << cpu.owner), 0);
        if exclusive {
            assert_eq!(peer.devices.wake.load(Acquire), 1);
        }
        // A single notify can cover more work than the bounded immediate pass.
        // Periodic service drains the remainder and retries a rejected owner kick.
        rt.manager.service(rt.board);
        assert_eq!((used(&sender, 1), used(&peer, 0)), (20, 20));
        assert_eq!(peer.devices.wake.load(Acquire), 0);
        for i in 0..20 {
            let mut data = [0; 60];
            assert!(
                peer.devices
                    .io
                    .lock()
                    .ram
                    .read(buffer(&peer, 0, i) + HEADER as u64, &mut data)
            );
            let mut expected = [i as u8; 60];
            expected[1] = sender.slot as u8;
            assert_eq!(data, expected);
        }
        OWNER.with(|o| o.set(cpu.owner));
        assert!(rt.dispatch(&mut worker).unwrap());
        assert_eq!(cpu.run_state(), CpuState::Runnable);
        assert!(cpu.data.lock().context.as_ref().unwrap().external);
    }
}

#[test]
fn periodic_tx_services_an_earlier_slot_and_carrier_edges_wake_a_waiter() {
    let f = fixture(true);
    let rt = &f.runtime;
    let peer = rt.manager.vm(0).unwrap();
    let sender = rt.manager.vm(1).unwrap();
    OWNER.with(|o| o.set(1));
    rt.manager.start(rt.board, 0).unwrap();
    setup(&f, &peer);
    assert_eq!(reg(&f, &peer, 0x104, None) & (1 << 16), 0);
    let cpu = peer.cpu(0).unwrap();
    cpu.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Wait);
    let mut waiting = Dispatcher::new();
    OWNER.with(|o| o.set(0));
    assert!(rt.dispatch(&mut waiting).unwrap());
    assert_eq!(cpu.run_state(), CpuState::Waiting);
    OWNER.with(|o| o.set(1));
    KICKED_OWNERS.with(|k| k.set(0));
    rt.manager.start(rt.board, 1).unwrap();
    assert_eq!(reg(&f, &peer, STATUS, None), 2);
    assert_ne!(KICKED_OWNERS.with(|k| k.get()) & 1, 0);
    OWNER.with(|o| o.set(0));
    assert!(rt.dispatch(&mut waiting).unwrap());
    assert_eq!(cpu.run_state(), CpuState::Runnable);
    OWNER.with(|o| o.set(1));
    reg(&f, &peer, ACK, Some(3));
    setup(&f, &sender);
    post(&peer, 0, 0);
    post(&sender, 1, 0);
    rt.manager.service(rt.board);
    assert_eq!(used(&peer, 0), 1);
    reg(&f, &peer, ACK, Some(3));
    sender.host_stop();
    let mut owner = Dispatcher::new();
    rt.dispatch(&mut owner).unwrap();
    rt.manager.service(rt.board);
    assert!(sender.lifecycle.joined());
    assert_eq!(reg(&f, &peer, STATUS, None), 2);
    assert!(!snapshot(&f, &peer).up);
    assert_eq!(snapshot(&f, &sender).tx, 1);
    let mut monitor = crate::monitor::Monitor::new();
    rt.board.input.lock().extend(b"vm status beta\n");
    let output = super::uart_capture::host_output(|| monitor.poll(rt));
    assert!(output.lines().any(|line| line=="VM 2 peer-net up=false tx=1 rx=0 dropped=0 full=0 down=0 queued=0"), "{output}");
    post(&peer, 1, 0);
    reg(&f, &peer, NOTIFY, Some(1));
    assert_eq!(snapshot(&f, &peer).down, 1);
    rt.manager.start(rt.board, 1).unwrap();
    assert_eq!(snapshot(&f, &sender).queued, 0);
    assert_eq!(snapshot(&f, &sender).tx, 0);
    assert!(snapshot(&f, &peer).up);
}

#[test]
fn reciprocal_queue_notifications_complete_parallel_batches_across_ring_wraps() {
    use std::{sync::Barrier, thread};
    let f = fixture(true);
    let rt = &f.runtime;
    let m = &rt.manager;
    let endpoints = [m.vm(0).unwrap(), m.vm(1).unwrap()];
    for slot in 0..2 {
        m.start(rt.board, slot).unwrap();
    }
    for vm in &endpoints {
        setup(&f, vm);
    }
    let simultaneous = Barrier::new(2);
    for batch in 0..64u16 {
        let start = batch * 8;
        for vm in &endpoints {
            for seq in start..start + 8 {
                post(vm, 0, seq);
                post(vm, 1, seq);
            }
        }
        thread::scope(|s| {
            for vm in &endpoints {
                let gate = &simultaneous;
                s.spawn(move || {
                    OWNER.with(|o| o.set(vm.slot));
                    gate.wait();
                    assert_eq!(vm.devices.access(access(NOTIFY, Some(1)), || 0, m), Some(0));
                });
            }
        });
        for vm in &endpoints {
            assert_eq!((used(vm, 0), used(vm, 1)), (start + 8, start + 8));
            for seq in start..start + 8 {
                let mut out = [0; 60];
                assert!(
                    vm.devices
                        .io
                        .lock()
                        .ram
                        .read(buffer(vm, 0, seq) + HEADER as u64, &mut out)
                );
                let mut expected = [seq as u8; 60];
                expected[1] = (1 - vm.slot) as u8;
                assert_eq!(out, expected);
            }
            reg(&f, vm, ACK, Some(3));
        }
    }
    for vm in &endpoints {
        let totals = snapshot(&f, vm);
        assert_eq!(
            (totals.tx, totals.rx, totals.dropped, totals.queued),
            (512, 512, 0, 0)
        );
    }
}
