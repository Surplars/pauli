//! Manifest fixtures shared by wire, placement and resource checks.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use crate::{BootConfig, CpuAffinity, DiskConfig, Error, VmConfig, VmImage, WIRE_SIZE};
use std::vec::Vec;
fn fixture() -> BootConfig {
    let mut config = BootConfig {
        template_base: 0x8e00_0000,
        template_size: 30 * 1024 * 1024,
        dtb_size: 4096,
        management: true,
        smoke_payload: true,
        disks: [None; 4],
        vms: [None; 4],
    };
    for i in 0..4 {
        let mut vm = VmConfig::new(&format!("guest{i}")).unwrap();
        vm.host_base = 0x9000_0000 + i as u64 * 256 * 1024 * 1024;
        vm.memory_size = (64 + i as u64 * 32) * 1024 * 1024;
        vm.dtb_offset = i as u64 * 1024;
        vm.dtb_size = 1024;
        vm.quantum_us = 100 + i as u64 * 1000;
        vm.image = VmImage {
            kernel_source: 0x8e00_0000,
            kernel_size: 64,
            kernel_memory_size: 4096,
            initrd_source: 0,
            initrd_size: 0,
        };
        config.vms[i] = Some(vm);
    }
    config
}

mod firmware;
mod guest_tree;
mod images;
mod layout;
mod placement;
mod resources;
mod wire;

fn independent() -> BootConfig {
    let config = fixture();
    let mut independent = config;
    for index in 0..4 {
        independent.disks[index] = Some(DiskConfig {
            sectors: 32,
            writable: true,
            managed: false,
        });
        let vm = independent.vms[index].as_mut().unwrap();
        vm.disk = Some(index as u8);
        vm.writable = true;
    }
    independent
}
fn uplink_wire() -> [u8; WIRE_SIZE] {
    let mut uplink = independent();
    uplink.vms[0].as_mut().unwrap().net_uplink = true;
    uplink.vms[0].as_mut().unwrap().stop_on_reset = true;
    uplink.encode().unwrap()
}
