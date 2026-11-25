//! Wave8's native firmware handoff, not the guest DTB.
//! The boot hart borrows the immutable DTB before using any native device.
//! Only the payload and explicitly admitted guest regions are usable; DDR size grants nothing.
use pauli_config::{GuestBoardLayout, ImsicIds};
use pauli_core::{HvError, Result};
use pauli_hal::fdt::{self, Fdt, Node};
use pauli_hal::mem::PhysRange;
mod memory;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Handoff<'a> {
    pub external: Option<External<'a>>,
    pub guest_ids: Option<usize>,
}
impl Handoff<'_> {
    /// Only an explicitly selected AIA guest needs to reserve native guest file 1.
    pub fn guest_file_ids(&self) -> Option<usize> {
        self.external.filter(|guest| guest.aia).and(self.guest_ids)
    }

    pub fn layout(&self) -> GuestBoardLayout {
        GuestBoardLayout {
            imsic_ids: self
                .guest_ids
                .and_then(ImsicIds::from_count)
                .unwrap_or(ImsicIds::Ids127),
            ..GuestBoardLayout::DEFAULT
        }
    }
}

/// Native physical ownership validated before the board borrows any image bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct External<'a> {
    pub image: PhysRange,
    pub initrd: Option<PhysRange>,
    pub ram: PhysRange,
    /// Private host frames for dynamic domains, separate from the boot guests' RAM.
    pub pool: Option<PhysRange>,
    pub bootargs: &'a str,
    /// Selects the management boot topology, not an RPC capability.
    pub manager: bool,
    pub aia: bool,
    /// Reciprocal copied ports for the two fixed boot VMs.
    pub peer_net: bool,
}

pub(super) const RAM_BASE: usize = 0x8020_0000;
pub(super) const RAM_BYTES: usize = 0x8100_0000 - RAM_BASE;
pub(super) const TIMEBASE_HZ: u64 = 100_000_000;
pub(super) const UART: usize = 0x1000_0000;
pub(super) const UART_IRQ: u32 = 3;
pub(super) const UART_CLOCK: u32 = 24_000_000;
pub(super) const UART_BAUD: u32 = 250_000;
pub(super) const S_APLIC: usize = 0x0c00_4000;
pub(super) const IMSIC_IDS: usize = 127;
const S_IMSIC: u64 = 0x2800_0000;
const HANDOFF_START: usize = 0x8fe0_0000;
const HANDOFF_END: usize = 0x9000_0000;
const FDT_HEADER: usize = 40;
const SEIP: u32 = 9;
const IRQ_LEVEL_HIGH: u32 = 4;

fn check_extent(address: usize, size: usize) -> Result<()> {
    if address.is_multiple_of(8)
        && (HANDOFF_START..HANDOFF_END).contains(&address)
        && (FDT_HEADER..=HANDOFF_END - address).contains(&size)
    {
        Ok(())
    } else {
        Err(HvError::InvalidImage)
    }
}

/// # Safety
/// OpenSBI supplies a retained, initialized DTB at `dtb` in identity-mapped
/// handoff RAM; its header and declared bytes remain readable and immutable
/// for the lifetime of the returned command line.
#[cfg(target_os = "none")]
pub(super) unsafe fn validate(hart: usize, dtb: usize) -> Result<Handoff<'static>> {
    check_extent(dtb, FDT_HEADER)?;
    // SAFETY: the firmware header is inside the checked handoff window.
    let size = unsafe { u32::from_be((dtb as *const u32).add(1).read()) } as usize;
    check_extent(dtb, size)?;
    // SAFETY: only borrow the advertised extent, never the whole reserved region.
    let bytes = unsafe { core::slice::from_raw_parts(dtb as *const u8, size) };
    validate_blob(hart, bytes)
}

fn node<'a>(fdt: Fdt<'a>, path: &str) -> fdt::Result<Node<'a>> {
    let n = fdt.find(path)?.ok_or(fdt::Error::Property)?;
    if n.available()? {
        Ok(n)
    } else {
        Err(fdt::Error::Property)
    }
}

pub(super) fn validate_blob(hart: usize, blob: &[u8]) -> Result<Handoff<'_>> {
    let tree = Fdt::new(blob).map_err(|_| HvError::InvalidImage)?;
    if hart != 0 || !cpu_supported(tree).map_err(|_| HvError::InvalidConfig)? {
        return Err(HvError::Unsupported);
    }
    let guest_ids = discover_io(tree).map_err(|_| HvError::InvalidDevice)?;
    let external = memory::granted(tree).map_err(|error| match error {
        fdt::Error::Property | fdt::Error::String => HvError::InvalidConfig,
        _ => HvError::InvalidMemory,
    })?;
    Ok(Handoff {
        external,
        guest_ids,
    })
}

fn cpu_supported(tree: Fdt<'_>) -> fdt::Result<bool> {
    let cpu = node(tree, "/cpus/cpu@0")?;
    let extensions = cpu.property("riscv,isa-extensions")?.unwrap_or(&[]);
    let required = [
        "i", "m", "a", "f", "d", "c", "h", "zicntr", "zicsr", "zifencei", "ssaia",
    ];
    Ok(node(tree, "/")?.compatible("openion,valence-ooo-ps-ddr")?
        && node(tree, "/cpus")?.u32("timebase-frequency")? == Some(TIMEBASE_HZ as u32)
        && cpu.reg()? == Some((0, 0))
        && cpu.text("riscv,isa-base")? == Some("rv64i")
        && cpu.text("mmu-type")? == Some("riscv,sv39")
        && extensions.last() == Some(&0)
        && required
            .iter()
            .all(|s| extensions.split(|&c| c == 0).any(|ext| ext == s.as_bytes())))
}

fn phandle(n: &Node<'_>) -> fdt::Result<u32> {
    n.u32("phandle")?
        .filter(|&id| id != 0 && id != u32::MAX)
        .ok_or(fdt::Error::Property)
}

fn pair(raw: Option<&[u8]>, a: u32, b: u32) -> bool {
    raw.is_some_and(|v| v.len() == 8 && v[..4] == a.to_be_bytes() && v[4..] == b.to_be_bytes())
}

fn unique_u32(node: &Node<'_>, name: &str) -> fdt::Result<Option<u32>> {
    node.unique_property(name)?
        .map(|raw| {
            raw.try_into()
                .map(u32::from_be_bytes)
                .map_err(|_| fdt::Error::Property)
        })
        .transpose()
}

fn imsic_guest_ids(soc: &Node<'_>, imsic: &Node<'_>) -> fdt::Result<Option<usize>> {
    let address = unique_u32(soc, "#address-cells")?.unwrap_or(2);
    let size = unique_u32(soc, "#size-cells")?.unwrap_or(1);
    let reg = imsic.unique_property("reg")?.ok_or(fdt::Error::Property)?;
    if !(1..=2).contains(&address)
        || !(1..=2).contains(&size)
        || reg.len() != 4 * (address + size) as usize
    {
        return Err(fdt::Error::Cells);
    }
    let (base, len) = imsic.reg()?.ok_or(fdt::Error::Property)?;
    let bits = unique_u32(imsic, "riscv,guest-index-bits")?.unwrap_or(0);
    if bits > 7
        || base != S_IMSIC
        || base.checked_add(len).is_none()
        || len < (4096u64 << bits)
        || unique_u32(imsic, "riscv,group-index-bits")?.unwrap_or(0) != 0
        || unique_u32(imsic, "riscv,num-ids")? != Some(IMSIC_IDS as u32)
    {
        return Err(fdt::Error::Property);
    }
    let guest_ids = unique_u32(imsic, "riscv,num-guest-ids")?;
    match (bits, guest_ids) {
        (0, None) => Ok(None),
        (0, Some(_)) => Err(fdt::Error::Property),
        (_, ids) => {
            let ids = ids.unwrap_or(IMSIC_IDS as u32) as usize;
            ImsicIds::from_count(ids)
                .map(|_| Some(ids))
                .ok_or(fdt::Error::Property)
        }
    }
}

fn discover_io(tree: Fdt<'_>) -> fdt::Result<Option<usize>> {
    let soc = node(tree, "/soc")?;
    let uart = node(tree, "/soc/serial@10000000")?;
    let aplic = node(tree, "/soc/interrupt-controller@c004000")?;
    let imsic = node(tree, "/soc/interrupt-controller@28000000")?;
    let intc = node(tree, "/cpus/cpu@0/interrupt-controller")?;
    let matches = soc.property("ranges")? == Some(&[])
        && uart.compatible("ns16550a")?
        && uart.reg()? == Some((UART as u64, 8))
        && uart.u32("clock-frequency")? == Some(UART_CLOCK)
        && uart.u32("current-speed")? == Some(UART_BAUD)
        && uart.u32("reg-shift")?.unwrap_or(0) == 0
        && uart.u32("reg-io-width")?.unwrap_or(1) == 1
        && pair(uart.property("interrupts")?, UART_IRQ, IRQ_LEVEL_HIGH)
        && uart.u32("interrupt-parent")? == Some(phandle(&aplic)?)
        && aplic.compatible("riscv,aplic")?
        && aplic.reg()? == Some((S_APLIC as u64, 0x4000))
        && aplic.u32("riscv,num-sources")? == Some(31)
        && aplic.u32("msi-parent")? == Some(phandle(&imsic)?)
        && imsic.compatible("riscv,imsics")?
        && intc.compatible("riscv,cpu-intc")?
        && pair(
            imsic.property("interrupts-extended")?,
            phandle(&intc)?,
            SEIP,
        );
    if !matches {
        return Err(fdt::Error::Property);
    }
    imsic_guest_ids(&soc, &imsic)
}

#[cfg(test)]
mod tests;
