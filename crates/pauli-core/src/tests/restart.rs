//! Cold restart through the real VM manager, dispatcher and guest RAM boundary.
//! Ownership/Lock Ordering: simulated owners dispatch serially; I/O owns each RAM handle.
//! Invariants: neither a stop request nor a successful join clears a peer or a live epoch.
use super::model::{Arch, Fixture, OWNER};
use crate::{
    HvError,
    arch::ExitKind,
    runtime::Dispatcher,
    vm::{State, Vm},
};
use pauli_config::KERNEL_OFFSET;
use pauli_devices::memory::GuestMemory;

// Probe leading RAM, kernel, kernel BSS, absent initrd, DTB and the very last byte.
fn markers(vm: &Vm<Arch>, dirty: Option<u8>) -> [u8; 6] {
    let cfg = &vm.config;
    let mut io = vm.devices.io.lock();
    [
        0,
        KERNEL_OFFSET,
        KERNEL_OFFSET + cfg.image.kernel_size,
        cfg.initrd_offset(),
        cfg.guest_dtb_offset(),
        cfg.memory_size - 1,
    ]
    .map(|offset| {
        let address = cfg.guest_base + offset;
        if let Some(byte) = dirty {
            assert!(io.ram.write(address, &[byte]));
        }
        let mut value = [0];
        assert!(io.ram.read(address, &mut value));
        value[0]
    })
}

#[test]
fn stop_and_fault_restore_only_the_joined_vm() {
    let fixture = Fixture::new(2, 1);
    let rt = &fixture.runtime;
    let vm = &rt.manager.vm(0).unwrap();
    let peer = &rt.manager.vm(1).unwrap();
    rt.manager.start(rt.board, 0).unwrap();
    rt.manager.start(rt.board, 1).unwrap();
    let pristine = [0, 0x31, 0, 0, 0xd1, 0];
    assert_eq!(markers(vm, None), pristine);
    assert_eq!(markers(peer, None), [0, 0x32, 0, 0, 0xd1, 0]);
    OWNER.with(|o| o.set(1));
    rt.dispatch(&mut Dispatcher::new()).unwrap();
    let peer_epoch = peer.epoch();
    markers(peer, Some(0xc7));
    OWNER.with(|o| o.set(0));
    let mut owner = Dispatcher::new();
    for fault in [false, true] {
        assert!(rt.dispatch(&mut owner).unwrap());
        let old_epoch = vm.epoch();
        markers(vm, Some(0xa9));
        vm.request_stop(fault, false);
        assert!(!vm.try_join(0, rt.manager.link()).unwrap());
        assert_eq!(rt.manager.start(rt.board, 0), Err(HvError::Busy));
        assert_eq!(markers(vm, None), [0xa9; 6]);
        rt.dispatch(&mut owner).unwrap();
        assert!(vm.try_join(0, rt.manager.link()).unwrap());
        assert_eq!(markers(vm, None), [0xa9; 6]);
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(vm.epoch(), old_epoch);
        assert_eq!(markers(vm, None), pristine);
        assert_eq!((peer.epoch(), markers(peer, None)), (peer_epoch, [0xc7; 6]));
    }
}

#[test]
fn small_firmware_cold_restore_leaves_peer_ram_and_epoch_alone() {
    let fixture = Fixture::with_config(1, [1; 2], 0, |_, cfg| {
        cfg.memory_size = 32 * 1024;
        cfg.firmware = Some(pauli_config::FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    });
    let rt = &fixture.runtime;
    assert_eq!(rt.manager.capacity(), 2);
    let check = |slot, write: Option<u8>| {
        let vm = rt.manager.vm(slot).unwrap();
        let mut io = vm.devices.io.lock();
        let base = vm.config.guest_base;
        let mut bytes = [0; 3];
        if let Some(b) = write {
            assert!(io.ram.write(base + 4096, &[b]));
        }
        for (i, offset) in [0, 4096, vm.config.guest_dtb_offset()]
            .into_iter()
            .enumerate()
        {
            assert!(io.ram.read(base + offset, &mut bytes[i..i + 1]));
        }
        bytes
    };
    rt.manager.start(rt.board, 0).unwrap();
    rt.manager.start(rt.board, 1).unwrap();
    let peer = rt.manager.vm(0).unwrap();
    let peer_epoch = peer.epoch();
    let steps = |vm: &Vm<Arch>| {
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
    };
    assert_eq!(check(0, Some(0xcd)), [0x31, 0xcd, 0xd1]);
    let mut owner = Dispatcher::new();
    for fault in [false, true] {
        let vm = rt.manager.vm(1).unwrap();
        let old = vm.epoch();
        rt.dispatch(&mut owner).unwrap();
        assert_eq!(check(1, Some(0xef)), [0x32, 0xef, 0xd1]);
        let cpu = vm.cpu(0).unwrap();
        if fault {
            {
                let mut data = cpu.data.lock();
                let context = data.context.as_mut().unwrap();
                context.deadline = Some(42);
                context.script.push_back(ExitKind::Fault);
            }
            // Two runnable single-vCPU guests share this round-robin owner.
            for _ in 0..2 {
                rt.dispatch(&mut owner).unwrap();
            }
            assert_eq!(vm.terminal_exit().map(|e| e.kind), Some(ExitKind::Fault));
        } else {
            vm.host_stop();
            assert!(vm.terminal_exit().is_none());
        }
        assert_eq!(vm.lifecycle.state(), State::Stopping);
        assert_eq!(rt.manager.start(rt.board, 1), Err(HvError::Busy));
        let frozen = steps(&vm);
        let peer_before = steps(&peer);
        for _ in 0..2 {
            rt.dispatch(&mut owner).unwrap();
            rt.manager.service(rt.board);
        }
        assert_eq!(
            vm.lifecycle.state(),
            if fault {
                State::Faulted
            } else {
                State::Stopped
            }
        );
        assert_eq!((vm.epoch(), steps(&vm)), (old, frozen));
        assert!(steps(&peer) > peer_before);
        rt.manager.start(rt.board, 1).unwrap();
        assert_ne!(vm.epoch(), old);
        assert!(vm.terminal_exit().is_none());
        {
            let data = cpu.data.lock();
            let fresh = data.context.as_ref().unwrap();
            assert_eq!(
                (fresh.epoch, fresh.steps, fresh.deadline),
                (vm.epoch(), 0, None)
            );
        }
        assert_eq!(check(1, None), [0x32, 0, 0xd1]);
        assert_eq!(check(0, None), [0x31, 0xcd, 0xd1]);
        assert_eq!(peer.epoch(), peer_epoch);
    }
}
