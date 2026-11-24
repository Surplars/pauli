//! Exact firmware clones through the normal management and scheduling paths.
//! One host owner drives the real lifecycle. Only immutable image bytes are shared.
use super::model::{Fixture, IDLE_DEADLINE, NOW};
use crate::{arch::ExitKind, monitor::backend::Target, runtime::Dispatcher};
use pauli_config::FirmwareLayout;
use pauli_control::{Backend, ChildName, ChildSpec, DiskSlot, Error, Shape, VmId};
use pauli_devices::memory::GuestMemory;
use pauli_hal::{addr::PAGE_SIZE, mem::PhysRange};

const RAM: u64 = 32 * 1024;
const TEMPLATE: VmId = VmId(2);

fn fixture() -> Fixture {
    // A 1 MiB spare bank cannot hold even one minimum-size Linux VM.
    Fixture::with_config(1, [1, 1], 1, |_, vm| {
        vm.memory_size = RAM;
        vm.firmware = Some(FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    })
}

fn spec() -> ChildSpec {
    ChildName::new("sensor").unwrap().into()
}

fn memory(f: &Fixture, id: VmId, write: Option<u8>) -> (PhysRange, [u8; 3]) {
    let vm = f.runtime.manager.vm(id.0 as usize - 1).unwrap();
    let base = vm.config.guest_base;
    let ram = &mut vm.devices.io.lock().ram;
    if let Some(value) = write {
        assert!(ram.write(base + PAGE_SIZE as u64, &[value]));
    }
    let offsets = [0, PAGE_SIZE as u64, vm.config.guest_dtb_offset()];
    let mut bytes = [0; 3];
    for (byte, offset) in bytes.iter_mut().zip(offsets) {
        assert!(ram.read(base + offset, core::slice::from_mut(byte)));
    }
    (ram.host_range(), bytes) // Firmware entry, first writable byte, immutable DTB.
}

#[test]
fn exact_clone_preserves_its_boot_image_and_never_shares_live_ram() {
    let f = fixture();
    let rt = &f.runtime;
    let mut host = Target(rt);
    let mut local = Dispatcher::new();
    host.start(TEMPLATE).unwrap();
    let parent = host.info(TEMPLATE).unwrap();
    let (source, _) = memory(&f, TEMPLATE, Some(0xe1));
    let child = host.create(None, TEMPLATE, spec()).unwrap();
    assert_eq!(
        (child.vcpus, child.memory_bytes, child.active),
        (1, RAM, false)
    );
    let (private, boot) = memory(&f, child.vm, None);
    assert!(!source.overlaps(private));
    assert_eq!(boot, [0x32, 0, 0xd1]);

    host.start(child.vm).unwrap();
    for _ in 0..2 {
        assert!(rt.dispatch(&mut local).unwrap());
    }
    let child_slot = child.vm.0 as usize - 1;
    assert!(!rt.manager.vm(child_slot).unwrap().retired());
    memory(&f, child.vm, Some(0xac));
    assert_eq!(memory(&f, TEMPLATE, None).1[1], 0xe1);
    host.stop(child.vm).unwrap();
    assert_eq!(host.destroy(child.vm, child.epoch), Err(Error::Busy));
    rt.dispatch(&mut local).unwrap();
    rt.manager.service(rt.board);

    host.start(child.vm).unwrap(); // Cold restart restores the child's image, not live template RAM.
    let second = host.info(child.vm).unwrap();
    assert_ne!(second.epoch, child.epoch);
    assert_eq!(memory(&f, child.vm, None), (private, boot));
    memory(&f, child.vm, Some(0xfa));
    host.stop(child.vm).unwrap();
    rt.dispatch(&mut local).unwrap();
    rt.manager.service(rt.board);
    assert_eq!(host.destroy(child.vm, child.epoch), Err(Error::Busy));
    host.destroy(child.vm, second.epoch).unwrap();
    let replacement = host.create(None, TEMPLATE, spec()).unwrap();
    assert_eq!(replacement.vm, child.vm);
    assert_ne!(replacement.epoch, second.epoch);
    assert_eq!(memory(&f, replacement.vm, None).1, boot);
    assert_eq!(host.info(TEMPLATE).unwrap(), parent);
    assert_eq!(memory(&f, TEMPLATE, None), (source, [0x32, 0xe1, 0xd1]));
}

#[test]
fn unsupported_firmware_shapes_and_disks_publish_nothing() {
    let f = fixture();
    let mut host = Target(&f.runtime);
    for (cpus, mib, disk) in [(2, 0, None), (0, 1, None), (0, 0, DiskSlot::new(0))] {
        let request = ChildSpec {
            shape: Shape {
                vcpus: core::num::NonZeroU8::new(cpus),
                memory_mib: core::num::NonZeroU16::new(mib),
            },
            disk,
            ..spec()
        };
        assert!(matches!(
            host.create(None, TEMPLATE, request),
            Err(Error::Unsupported | Error::Invalid)
        ));
        assert_eq!(host.snapshot().count(), 2);
    }
    assert_eq!(host.create(None, TEMPLATE, spec()).unwrap().vm, VmId(3));
}

#[cfg(feature = "selftest")]
#[test]
fn a_reused_child_slot_starts_without_a_uart_sample() {
    let f = fixture();
    let rt = &f.runtime;
    let mut host = Target(rt);
    let old = host.create(None, TEMPLATE, spec()).unwrap();
    let slot = old.vm.0 as usize - 1;
    host.start(old.vm).unwrap();
    {
        let vm = rt.manager.vm(slot).unwrap();
        vm.devices.io.lock().console.access(1, Some(1)).unwrap();
        host.input(old.vm, b'x').unwrap();
        let mut bank = vm.devices.irqs.lock();
        assert!(bank.owner_pending::<crate::tests::model::Smp>(0, 0, || 20));
        let rise = bank.rx.last.unwrap();
        assert_eq!((rise.seq, rise.owner.unwrap().ticks), (1, 20));
    }
    host.stop(old.vm).unwrap();
    rt.dispatch(&mut Dispatcher::new()).unwrap();
    rt.manager.service(rt.board);
    host.destroy(old.vm, old.epoch).unwrap();
    let fresh = host.create(None, TEMPLATE, spec()).unwrap();
    assert_eq!(fresh.vm, old.vm);
    assert_ne!(fresh.epoch, old.epoch);
    let vm = rt.manager.vm(slot).unwrap();
    assert!(vm.devices.irqs.lock().rx.last.is_none());
}

#[test]
fn a_later_created_firmware_vcpu_contributes_its_own_idle_deadline() {
    let f = fixture();
    let rt = &f.runtime;
    let mut host = Target(rt);
    let mut local = Dispatcher::new();
    let child = host.create(None, TEMPLATE, spec()).unwrap();
    for id in [VmId(1), TEMPLATE, child.vm] {
        host.start(id).unwrap();
        let vm = rt.manager.vm(id.0 as usize - 1).unwrap();
        let mut data = vm.cpu(0).unwrap().data.lock();
        let cpu = data.context.as_mut().unwrap();
        cpu.script.push_back(ExitKind::Wait);
        cpu.deadline = (id == child.vm).then_some(1680);
    }
    for _ in 0..3 {
        assert!(rt.dispatch(&mut local).unwrap());
    }
    NOW.with(|now| now.set(1000));
    assert!(!rt.dispatch(&mut local).unwrap());
    let steps = |id: VmId| {
        let vm = rt.manager.vm(id.0 as usize - 1).unwrap();
        vm.cpu(0)
            .unwrap()
            .data
            .lock()
            .context
            .as_ref()
            .unwrap()
            .steps
    };
    let ids = [VmId(1), TEMPLATE, child.vm];
    let [a, b, c] = ids.map(steps);
    rt.idle(&mut local).unwrap();
    assert_eq!(IDLE_DEADLINE.with(|cmp| cmp.get()), Some(1680));
    NOW.with(|now| now.set(1680));
    assert!(rt.dispatch(&mut local).unwrap());
    let [next_a, next_b, next_c] = ids.map(steps);
    assert_eq!((next_a, next_b), (a, b)); // Only the child had a timer.
    assert!(next_c > c);
}
