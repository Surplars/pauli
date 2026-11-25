//! Stage Linux, optionally as a management domain beside a stopped firmware service.
//! Only the boot hart stages the configuration. Source bytes stay immutable
//! through every guest epoch; the monitor only ever hands guest RAM to VS.
use super::assignment::Images;
use super::handoff::{External, TIMEBASE_HZ};
use pauli_config::{
    ALIGN, BootConfig, FirmwareLayout, GuestBoardLayout, MAX_GUEST_DTB, VmConfig, VmImage,
};
use pauli_core::{HvError, Result, board::BootImages};
use pauli_hal::{addr::PAGE_SIZE, mem::PhysRange};

/// The core retains these three exact slices in its cold-restart image plan.
/// No reference is ever made to a gap between Image and initrd, or to all DDR.
/// # Safety
/// The board validated the loader's ranges and owns their read-only, identity
/// mapped bytes for the entire boot. No loader or DMA engine may modify them.
pub(super) unsafe fn stage(
    input: External<'_>,
    service: &'static [u8],
    trees: &'static mut [[u8; MAX_GUEST_DTB]; 2],
    layout: GuestBoardLayout,
) -> Result<(BootConfig, Images)> {
    // SAFETY: only the exact validated, initialized Image bytes are borrowed.
    let kernel = unsafe { source(input.image) };
    let code = PhysRange::new(service.as_ptr() as usize, service.len())
        .map_err(|_| HvError::InvalidImage)?;
    let [linux_tree, service_tree] = trees;
    let boot = config(&input, kernel, linux_tree, code, service_tree, layout)?;
    let initrd = match input.initrd {
        // SAFETY: the optional archive has the same retained, disjoint source contract.
        Some(range) => unsafe { source(range) },
        None => &[],
    };
    let linux = BootImages {
        kernel,
        initrd,
        dtb: &linux_tree[..boot.vms[0].ok_or(HvError::InvalidConfig)?.dtb_size as usize],
    };
    let peer = boot.vms[1].map(|vm| BootImages {
        kernel: service,
        initrd: &[],
        dtb: &service_tree[..vm.dtb_size as usize],
    });
    Ok((boot, [Some(linux), peer]))
}

unsafe fn source(range: PhysRange) -> &'static [u8] {
    // SAFETY: caller owns the lifetime, validity and immutability of this exact physical span.
    unsafe { core::slice::from_raw_parts(range.base() as *const u8, range.size()) }
}

fn linux(input: &External<'_>, header: &[u8]) -> Result<VmConfig> {
    let mut vm = VmConfig::new(if input.manager { "manager" } else { "linux" })
        .map_err(|_| HvError::InvalidConfig)?;
    if input.aia && !cfg!(feature = "aia") {
        return Err(HvError::Unsupported);
    }
    vm.aia = input.aia;
    let file_size = input.image.size() as u64;
    vm.image = VmImage {
        kernel_source: input.image.base() as u64,
        kernel_size: file_size,
        kernel_memory_size: pauli_config::linux_image::memory_size(header, file_size)
            .map_err(|_| HvError::InvalidImage)?,
        initrd_source: input.initrd.map_or(0, |r| r.base() as u64),
        initrd_size: input.initrd.map_or(0, |r| r.size() as u64),
    };
    vm.host_base = input.ram.base() as u64;
    // Preserve a 2 MiB boundary for Linux superpage mappings. The final tail
    // is not a pool: the service receives just its private 64 KiB, no more.
    vm.memory_size = (input.ram.size() as u64)
        .checked_sub(if input.manager { ALIGN } else { 0 })
        .ok_or(HvError::InvalidMemory)?;
    vm.autostart = true;
    Ok(vm)
}

fn service(code: PhysRange, host: u64) -> Result<VmConfig> {
    if code.size() > PAGE_SIZE {
        return Err(HvError::InvalidImage);
    }
    let mut vm = VmConfig::new("service").map_err(|_| HvError::InvalidConfig)?;
    vm.firmware = Some(FirmwareLayout {
        rx_pages: 1,
        ro_pages: 0,
    });
    vm.host_base = host;
    vm.memory_size = 64 * 1024;
    vm.image.kernel_source = code.base() as u64;
    vm.image.kernel_size = code.size() as u64;
    vm.image.kernel_memory_size = vm.image.kernel_size;
    Ok(vm)
}

fn render(vm: &mut VmConfig, args: &str, out: &mut [u8], layout: GuestBoardLayout) -> Result<()> {
    vm.dtb_size = pauli_config::guest_dtb::riscv_with_sstc(
        vm,
        layout,
        TIMEBASE_HZ as u32,
        args,
        pauli_arch_riscv::sstc_available(),
        out,
    )
    .map_err(|_| HvError::InvalidImage)? as u64;
    Ok(())
}

fn config(
    input: &External<'_>,
    header: &[u8],
    tree: &mut [u8],
    code: PhysRange,
    peer_tree: &mut [u8],
    layout: GuestBoardLayout,
) -> Result<BootConfig> {
    if input.manager && !cfg!(feature = "guest-control") {
        return Err(HvError::Unsupported);
    }
    let mut vm = linux(input, header)?;
    render(&mut vm, input.bootargs, tree, layout)?;
    // The boot manifest encodes a page-aligned numeric envelope; unlike the
    // individual slices in BootImages, this envelope grants no read authority.
    let archive = input.initrd.unwrap_or(input.image);
    let mut base = archive.base().min(input.image.base());
    let mut limit = archive.end().max(input.image.end());
    let mut dtb_size = vm.dtb_size;
    let peer = if input.manager {
        let mut peer = service(code, vm.host_base + vm.memory_size)?;
        render(&mut peer, "", peer_tree, layout)?;
        // DTB buffers have a fixed slot stride; only their generated prefixes
        // appear as restart images. Linux arguments are never copied to the peer.
        peer.dtb_offset = MAX_GUEST_DTB as u64;
        dtb_size = peer.dtb_offset + peer.dtb_size;
        base = base.min(code.base());
        limit = limit.max(code.end());
        Some(peer)
    } else {
        None
    };
    let boot = BootConfig {
        template_base: base as u64,
        template_size: (limit.next_multiple_of(PAGE_SIZE) - base) as u64,
        dtb_size,
        management: input.manager,
        smoke_payload: false,
        disks: [None; pauli_config::MAX_DISKS],
        vms: [Some(vm), peer, None, None],
    };
    boot.validate().map_err(|_| HvError::InvalidConfig)?;
    Ok(boot)
}

#[cfg(test)]
mod tests;
