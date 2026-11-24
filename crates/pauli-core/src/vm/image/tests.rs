//! Host-only checks and an opt-in timing sample of the real cold restore path.
//! Ownership/Lock Ordering: each fixture owns its backing; no guest or DMA runs.
//! Invariants: boot templates never overlap the destination or change between epochs.
use super::*;
use pauli_devices::memory::GuestMemory;
use pauli_hal::mem::PhysRange;
use std::{boxed::Box, time::Instant, vec};

const MIB: usize = 1024 * 1024;

#[test]
fn embedded_restores_at_zero_without_linux_offsets() {
    #[repr(align(4096))]
    struct Page([u8; 4096]);
    let mut backing = Page([0xee; 4096]);
    let mut cfg = VmConfig::new("embedded").unwrap();
    cfg.memory_size = backing.0.len() as u64;
    cfg.host_base = backing.0.as_mut_ptr() as u64;
    cfg.image.kernel_size = 5;
    cfg.image.kernel_memory_size = 5;
    let images = || BootImages {
        kernel: b"hello",
        initrd: &[],
        dtb: &[],
    };
    let range = PhysRange::new(cfg.host_base as usize, cfg.memory_size as usize).unwrap();
    // SAFETY: no mappings or native hardware share this private aligned test page.
    let mut ram = unsafe { GuestRam::new(range, cfg.guest_base as usize) }.unwrap();
    cfg.image.kernel_size += 1;
    assert!(matches!(
        prepare(&cfg, images(), &ram, Payload::Embedded),
        Err(HvError::InvalidImage)
    ));
    cfg.image.kernel_size -= 1;
    let plan = prepare(&cfg, images(), &ram, Payload::Embedded).unwrap();
    // SAFETY: test RAM is still inactive and cannot be reached by any other owner.
    unsafe { ram.restore_retired(&plan) }.unwrap();
    drop(ram);
    assert_eq!(&backing.0[..5], b"hello");
    assert!(backing.0[5..].iter().all(|&b| b == 0));
}

fn with_linux_ram(images: BootImages, check: impl FnOnce(&VmConfig, &mut GuestRam, &RamImage)) {
    let mut backing = vec![0xa5u8; 64 * MIB].into_boxed_slice();
    let mut cfg = VmConfig::new("sample").unwrap();
    cfg.host_base = backing.as_mut_ptr() as u64;
    cfg.memory_size = backing.len() as u64;
    cfg.image.kernel_size = images.kernel.len() as u64;
    cfg.image.kernel_memory_size = cfg.image.kernel_size + MIB as u64;
    cfg.image.initrd_size = images.initrd.len() as u64;
    cfg.dtb_size = images.dtb.len() as u64;
    let range = PhysRange::new(cfg.host_base as usize, backing.len()).unwrap();
    // SAFETY: private backing remains allocated and is accessed only through ram.
    let mut ram = unsafe { GuestRam::new(range, cfg.guest_base as usize) }.unwrap();
    let image = prepare(&cfg, images, &ram, Payload::Linux).unwrap();
    check(&cfg, &mut ram, &image);
}

#[test]
fn linux_restores_templates_but_zeros_bss_and_padding() {
    let images = BootImages {
        kernel: b"Image",
        initrd: b"cpio",
        dtb: b"FDT",
    };
    with_linux_ram(images, |cfg, ram, image| {
        // SAFETY: the closure owns the fixture and no CPU/device has been published.
        unsafe { ram.restore_retired(image) }.unwrap();
        for (offset, expected) in [
            (KERNEL_OFFSET - 1, &b"\0Image\0"[..]),
            (KERNEL_OFFSET + cfg.image.kernel_memory_size - 1, b"\0\0"),
            ((32 * MIB - 1) as u64, b"\0cpio\0"),
            ((62 * MIB - 1) as u64, b"\0FDT\0"),
            (cfg.memory_size - 1, b"\0"),
        ] {
            let mut probe = [0xa5; 7];
            let result = &mut probe[..expected.len()];
            assert!(ram.read(cfg.guest_base + offset, result));
            assert_eq!(result, expected);
        }
    });
}

/// Run manually in release mode. This measures host memory work, not RISC-V boot time.
#[test]
#[ignore = "host cold-restore timing; run explicitly in release mode"]
fn sample_cold_restore() {
    let template = |len, byte| -> &'static [u8] { Box::leak(vec![byte; len].into_boxed_slice()) };
    let images = BootImages {
        kernel: template(16 * MIB, 0x31),
        initrd: template(8 * MIB, 0x59),
        dtb: template(4096, 0xc3),
    };
    with_linux_ram(images, sample);
}

fn sample(cfg: &VmConfig, ram: &mut GuestRam, image: &RamImage) {
    let mut samples = [0; 11];
    for round in 0..3 + samples.len() {
        let start = Instant::now();
        // SAFETY: no vCPU, translation or device can hold this test RAM.
        unsafe { ram.restore_retired(image) }.unwrap();
        let elapsed = start.elapsed().as_micros();
        if round >= 3 {
            samples[round - 3] = elapsed;
        }
    }
    samples.sort_unstable();
    std::println!("64 MiB RAM; restore us: {samples:?}; median={}", samples[5]);
    let mut last = [0];
    assert!(ram.read(
        cfg.guest_base + cfg.guest_dtb_offset() + cfg.dtb_size - 1,
        &mut last
    ));
    assert_eq!(last, [0xc3]);
}
