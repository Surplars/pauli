//! A guest posts one split-ring write through its assigned MMIO aperture.
//! Ownership/Lock Ordering: guest fills its own RAM before notifying the frontend.
//! Invariants: both queue and payload use only addresses inside this VM.
use super::{Arch, Fixture};
use crate::vm::Vm;
use pauli_config::GuestBoardLayout;
use pauli_devices::memory::GuestMemory;
use pauli_hal::device::MmioAccess;

const STATUS: usize = 0x70;
const DEVICE_FEATURES: usize = 0x10;
const EVENT_IDX: usize = 1 << 29;
const DRIVER_FEATURES_SEL: usize = 0x24;
const DRIVER_FEATURES: usize = 0x20;
const QUEUE_SIZE: usize = 0x38;
const DESC: usize = 0x80;
const AVAIL: usize = 0x90;
const USED: usize = 0xa0;
const READY: usize = 0x44;
const KICK: usize = 0x50;
const NEXT: u16 = 1;
const DEVICE_WRITES: u16 = 2;

pub(super) fn access(
    f: &Fixture,
    vm: &Vm<Arch>,
    reg: usize,
    write: Option<usize>,
) -> Option<usize> {
    vm.devices.access(
        MmioAccess {
            gpa: GuestBoardLayout::DEFAULT.block.base as usize + reg,
            width: 4,
            write,
        },
        || 0,
        &f.runtime.manager,
    )
}
pub(super) fn write(f: &Fixture, vm: &Vm<Arch>, byte: u8) {
    post(f, vm, Some(byte));
}
pub(super) fn read(f: &Fixture, vm: &Vm<Arch>) {
    post(f, vm, None);
}
fn post(f: &Fixture, vm: &Vm<Arch>, data: Option<u8>) {
    let base = vm.config.guest_base;
    let [desc, avail, used, header, payload, status] = [1, 2, 3, 4, 5, 6].map(|i| base + i * 4096);
    let reg = |offset, data| assert_eq!(access(f, vm, offset, Some(data)), Some(0));
    let low = access(f, vm, DEVICE_FEATURES, None).unwrap() & !EVENT_IDX;
    for (port, data) in [
        (STATUS, 1),
        (STATUS, 3),
        (DRIVER_FEATURES, low),
        (DRIVER_FEATURES_SEL, 1),
        (DRIVER_FEATURES, 1),
        (STATUS, 11),
        (QUEUE_SIZE, 8),
    ] {
        reg(port, data);
    }
    for (port, address) in [(DESC, desc), (AVAIL, avail), (USED, used)] {
        reg(port, address as u32 as usize);
        reg(port + 4, (address >> 32) as usize);
    }
    reg(READY, 1);
    reg(STATUS, 15);
    assert_eq!(access(f, vm, STATUS, None), Some(15));
    {
        let mut io = vm.devices.io.lock();
        let ram = &mut io.ram;
        for (i, (addr, len, flags, next)) in [
            (header, 16u32, NEXT, 1u16),
            (
                payload,
                512,
                if data.is_some() {
                    NEXT
                } else {
                    NEXT | DEVICE_WRITES
                },
                2,
            ),
            (status, 1, DEVICE_WRITES, 0),
        ]
        .into_iter()
        .enumerate()
        {
            let entry = desc + i as u64 * 16;
            assert!(ram.write(entry, &addr.to_le_bytes()));
            assert!(ram.write(entry + 8, &len.to_le_bytes()));
            assert!(ram.write(entry + 12, &flags.to_le_bytes()));
            assert!(ram.write(entry + 14, &next.to_le_bytes()));
        }
        assert!(ram.write(header, &u32::from(data.is_some()).to_le_bytes())); // OUT=1, IN=0.
        assert!(ram.write(payload, &[data.unwrap_or(0); 512]));
        assert!(ram.write(status, &[0xff])); // An untouched status is not success.
        assert!(ram.write(avail + 2, &1u16.to_le_bytes()));
    }
    reg(KICK, 0);
    f.runtime.manager.service(f.runtime.board);
}
pub(super) fn completed(vm: &Vm<Arch>, status: u8, byte: u8) {
    let io = vm.devices.io.lock();
    let base = vm.config.guest_base;
    let (mut used, mut result, mut payload) = ([0; 2], [0; 1], [0; 512]);
    assert!(io.ram.read(base + 3 * 4096 + 2, &mut used));
    assert!(io.ram.read(base + 6 * 4096, &mut result));
    assert!(io.ram.read(base + 5 * 4096, &mut payload));
    assert_eq!((u16::from_le_bytes(used), result[0]), (1, status));
    assert_eq!(payload, [byte; 512]);
}
