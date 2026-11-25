//! Description of the RV64 Linux virtual board, shared by staging and provisioning.
//! Ownership/Lock Ordering: render into private host storage; no live guest or locks.
//! Invariants: CPU nodes, interrupts, memory and initrd all describe the same VmConfig.
use crate::{
    Error, GuestBoardLayout, GuestIrqConfig, GuestIrqController, MAX_GUEST_VCPUS, MmioRegion,
    VmConfig,
};
mod control;
mod sstc;
mod writer;
pub use control::validate_control_events;
pub use sstc::riscv_sstc;
use writer::{Hex, Writer};
type Result<T = ()> = core::result::Result<T, Error>;
const CPU_INTC: u32 = 0x10;
const CONTROLLER: u32 = 1;
const MSI: u32 = 2;
const EXTERNAL: u32 = 9;
const LEVEL_HIGH: u32 = 4;
const CONTROL_COMPATIBLE: &str = "openion,pauli-control";

/// Write the canonical tree, with no host addresses. Truncated output on error
/// is never a DTB; only the returned prefix may be staged or used for restore.
pub fn riscv(
    vm: &VmConfig,
    layout: GuestBoardLayout,
    hz: u32,
    args: &str,
    out: &mut [u8],
) -> Result<usize> {
    riscv_with_sstc(vm, layout, hz, args, false, out)
}

/// Write the canonical tree, optionally advertising RISC-V `sstc` for Linux VMs.
pub fn riscv_with_sstc(
    vm: &VmConfig,
    layout: GuestBoardLayout,
    hz: u32,
    args: &str,
    sstc: bool,
    out: &mut [u8],
) -> Result<usize> {
    render(vm, layout, hz, args, sstc, false, out)
}

/// Render the trusted VM's optional devices alongside its admitted timer capability.
pub fn riscv_configured(
    vm: &VmConfig,
    layout: GuestBoardLayout,
    hz: u32,
    args: &str,
    sstc: bool,
    out: &mut [u8],
) -> Result<usize> {
    render(vm, layout, hz, args, sstc, vm.control_events, out)
}

fn render(
    vm: &VmConfig,
    layout: GuestBoardLayout,
    hz: u32,
    args: &str,
    sstc: bool,
    control_events: bool,
    out: &mut [u8],
) -> Result<usize> {
    vm.validate_guest_geometry()?;
    if !layout.admits_ram(vm.guest_base, vm.memory_size) {
        return Err(Error::Memory);
    }
    if hz == 0 || args.as_bytes().contains(&0) {
        return Err(Error::Dtb);
    }
    let irq = layout
        .irq_config(vm.vcpus, vm.aia)
        .map_err(|_| Error::Dtb)?;
    let mut f = Writer::new(out)?;
    f.begin(&[])?;
    f.text(
        "compatible",
        if vm.firmware.is_some() {
            "openion,firmware-guest"
        } else {
            "openion,linux-guest"
        },
    )?;
    f.text("model", "OpenIon isolated RV64 guest")?;
    f.u32("#address-cells", 2)?;
    f.u32("#size-cells", 2)?;
    f.begin(&[b"chosen"])?;
    f.text("bootargs", args)?;
    f.prop(
        "stdout-path",
        &[b"/uart@", Hex::new(layout.uart.base).bytes(), &[0]],
    )?;
    if vm.image.initrd_size != 0 {
        let start = vm.guest_base + vm.initrd_offset();
        f.u64("linux,initrd-start", start)?;
        f.u64("linux,initrd-end", start + vm.image.initrd_size)?;
    }
    f.end()?;
    f.at(b"memory", vm.guest_base)?;
    f.text("device_type", "memory")?;
    f.reg(MmioRegion {
        base: vm.guest_base,
        size: vm.memory_size,
    })?;
    f.end()?;
    cpus(&mut f, vm, hz, sstc)?;
    interrupts(&mut f, irq)?;
    device(
        &mut f,
        b"uart",
        "ns16550a",
        layout.uart,
        layout.uart_irq,
        vm.aia,
    )?;
    f.u32("clock-frequency", 3_686_400)?;
    f.u32("current-speed", 115_200)?;
    f.end()?;
    device(
        &mut f,
        b"rtc",
        "google,goldfish-rtc",
        layout.rtc,
        layout.rtc_irq,
        vm.aia,
    )?;
    f.end()?;
    if vm.disk.is_some() {
        virtio(&mut f, layout.block, layout.block_irq, vm.aia)?;
    }
    if vm.net_peer.is_some() || vm.net_uplink {
        virtio(&mut f, layout.net, layout.net_irq, vm.aia)?;
    }
    if control_events {
        f.begin(&[b"pauli-control"])?;
        f.text("compatible", CONTROL_COMPATIBLE)?;
        device_irq(&mut f, layout.control_irq, vm.aia)?;
        f.end()?;
    }
    f.end()?;
    f.finish()
}
fn cpus(f: &mut Writer<'_>, vm: &VmConfig, hz: u32, sstc: bool) -> Result {
    f.begin(&[b"cpus"])?;
    f.u32("#address-cells", 1)?;
    f.u32("#size-cells", 0)?;
    f.u32("timebase-frequency", hz)?;
    for cpu in 0..u32::from(vm.vcpus) {
        f.at(b"cpu", u64::from(cpu))?;
        f.text("device_type", "cpu")?;
        f.text("compatible", "riscv")?;
        f.text("enable-method", "riscv,sbi")?;
        f.text("status", "okay")?;
        f.u32("reg", cpu)?;
        f.text("mmu-type", "riscv,sv39")?;
        f.text("riscv,isa-base", "rv64i")?;
        let integer = vm.firmware.is_some();
        let has_sstc = !integer && sstc;
        let extra_aia: &[u8] = if vm.aia { b"ssaia\0" } else { &[] };
        let extra_sstc: &[u8] = if has_sstc { b"sstc\0" } else { &[] };
        let fp: &[u8] = if integer { b"" } else { b"f\0d\0" };
        f.prop(
            "riscv,isa-extensions",
            &[
                b"i\0m\0a\0",
                fp,
                b"c\0zicsr\0zifencei\0zicntr\0",
                extra_aia,
                extra_sstc,
            ],
        )?;
        f.text(
            "riscv,isa",
            match (integer, vm.aia, has_sstc) {
                (true, false, _) => "rv64imac_zicsr_zifencei_zicntr",
                (true, true, _) => "rv64imac_zicsr_zifencei_zicntr_ssaia",
                (false, false, false) => "rv64imafdc_zicsr_zifencei_zicntr",
                (false, true, false) => "rv64imafdc_zicsr_zifencei_zicntr_ssaia",
                (false, false, true) => "rv64imafdc_zicsr_zifencei_zicntr_sstc",
                (false, true, true) => "rv64imafdc_zicsr_zifencei_zicntr_ssaia_sstc",
            },
        )?;
        f.begin(&[b"interrupt-controller"])?;
        f.text("compatible", "riscv,cpu-intc")?;
        f.u32("#interrupt-cells", 1)?;
        f.u32("phandle", CPU_INTC + cpu)?;
        f.prop("interrupt-controller", &[])?;
        f.end()?;
        f.end()?;
    }
    f.end()
}
fn interrupts(f: &mut Writer<'_>, irq: GuestIrqConfig) -> Result {
    let (name, compatible, region): (&[u8], _, _) = match irq.controller {
        GuestIrqController::Legacy { region } => (b"plic", "riscv,plic0", region),
        GuestIrqController::Advanced { wired, .. } => (b"aplic", "riscv,aplic", wired),
    };
    let mut entries = [0; 8 * MAX_GUEST_VCPUS as usize];
    let contexts = &mut entries[..8 * usize::from(irq.vcpus)];
    for (cpu, pair) in contexts.chunks_exact_mut(8).enumerate() {
        pair[..4].copy_from_slice(&(CPU_INTC + cpu as u32).to_be_bytes());
        pair[4..].copy_from_slice(&EXTERNAL.to_be_bytes());
    }
    f.at(name, region.base)?;
    f.text("compatible", compatible)?;
    f.reg(region)?;
    f.u32("phandle", CONTROLLER)?;
    f.prop("interrupt-controller", &[])?;
    f.u32("#address-cells", 0)?;
    let GuestIrqController::Advanced { message, ids, .. } = irq.controller else {
        f.u32("#interrupt-cells", 1)?;
        f.u32("riscv,ndev", 31)?;
        f.prop("interrupts-extended", &[contexts])?;
        return f.end();
    };
    f.u32("#interrupt-cells", 2)?;
    f.u32("riscv,num-sources", 31)?;
    f.u32("msi-parent", MSI)?;
    f.end()?;
    f.at(b"imsics", message.base)?;
    f.text("compatible", "riscv,imsics")?;
    f.reg(message)?;
    f.u32("#interrupt-cells", 0)?;
    f.u32("#msi-cells", 0)?;
    f.prop("interrupt-controller", &[])?;
    f.prop("msi-controller", &[])?;
    f.u32("phandle", MSI)?;
    f.u32("riscv,num-ids", ids.max_id() as u32)?;
    f.prop("interrupts-extended", &[contexts])?;
    f.end()
}
fn device(
    f: &mut Writer<'_>,
    name: &[u8],
    compatible: &str,
    region: MmioRegion,
    irq: u32,
    aia: bool,
) -> Result {
    f.at(name, region.base)?;
    f.text("compatible", compatible)?;
    f.reg(region)?;
    device_irq(f, irq, aia)
}
fn device_irq(f: &mut Writer<'_>, irq: u32, aia: bool) -> Result {
    f.u32("interrupt-parent", CONTROLLER)?;
    if aia {
        f.prop(
            "interrupts",
            &[&irq.to_be_bytes(), &LEVEL_HIGH.to_be_bytes()],
        )
    } else {
        f.u32("interrupts", irq)
    }
}
fn virtio(f: &mut Writer<'_>, region: MmioRegion, irq: u32, aia: bool) -> Result {
    device(f, b"virtio", "virtio,mmio", region, irq, aia)?;
    f.prop("dma-coherent", &[])?;
    f.end()
}
