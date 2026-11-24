//! QEMU virt discovery: identity /soc bus, RAM, timebase and verified CPU topology.
//! Ownership/Lock Ordering: immutable OpenSBI DTB; no CSR or device access here.
//! Invariants: context indices follow phandles, not the OpenSBI boot hart order.
use pauli_arch_riscv::MAX_CPUS;
use pauli_core::{HvError, Result};
use pauli_hal::{
    device::DeviceResource,
    fdt::{Fdt, Node},
    mem::PhysRange,
};

/// # Safety
/// OpenSBI supplied a retained QEMU DTB; its header page is readable. The
/// firmware allocation holds the entire advertised blob up to the QEMU limit.
pub unsafe fn firmware_blob(address: usize) -> Result<&'static [u8]> {
    const LIMIT: usize = 2 * 1024 * 1024;
    if address == 0 || address & 7 != 0 {
        return Err(HvError::InvalidConfig);
    }
    // SAFETY: the boot firmware owns and retains this header.
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, 8) };
    if cell(header) != 0xd00d_feed {
        return Err(HvError::InvalidConfig);
    }
    let len = cell(&header[4..]) as usize;
    if !(40..=LIMIT).contains(&len) || address.checked_add(len).is_none() {
        return Err(HvError::InvalidConfig);
    }
    // SAFETY: trusted firmware allocation, with the validated DTB header extent.
    Ok(unsafe { core::slice::from_raw_parts(address as *const u8, len) })
}

pub struct Topology {
    pub harts: [usize; MAX_CPUS],
    pub intc: [u32; MAX_CPUS],
    pub count: usize,
    pub boot: usize,
    pub fd: bool,
    pub aia: bool,
    pub hz: u64,
}
#[cfg(test)]
mod tests;
pub fn discover(fdt: Fdt<'_>, hart: usize) -> Result<Topology> {
    let hz = parsed(fdt.find("/cpus"))?.ok_or(HvError::InvalidConfig)?;
    let hz = parsed(hz.u32("timebase-frequency"))?
        .filter(|hz| *hz != 0)
        .ok_or(HvError::InvalidConfig)?;
    let mut topo = Topology {
        harts: [0; MAX_CPUS],
        intc: [0; MAX_CPUS],
        count: 0,
        boot: 0,
        fd: true,
        aia: false,
        hz: hz.into(),
    };
    let mut parent = None;
    for node in fdt.nodes() {
        let node = parsed(node)?;
        if node.depth() <= 2 {
            parent = None;
        }
        if node.depth() == 2
            && parsed(node.text("device_type"))? == Some("cpu")
            && parsed(node.available())?
        {
            parent = Some(add_cpu(&mut topo, &node)?);
        } else if node.depth() == 3
            && let Some(index) = parent
            && parsed(node.compatible("riscv,cpu-intc"))?
        {
            add_intc(&mut topo, index, &node)?;
        }
    }
    finish_topology(topo, hart)
}

fn finish_topology(mut topo: Topology, hart: usize) -> Result<Topology> {
    if topo.intc[..topo.count].contains(&0) {
        return Err(HvError::InvalidConfig);
    }
    // Match QEMU's physical hart numbering, not the FDT node insertion order.
    for i in 1..topo.count {
        let mut j = i;
        while j > 0 && topo.harts[j - 1] > topo.harts[j] {
            topo.harts.swap(j - 1, j);
            topo.intc.swap(j - 1, j);
            j -= 1;
        }
    }
    topo.boot = topo.harts[..topo.count]
        .iter()
        .position(|id| *id == hart)
        .ok_or(HvError::InvalidConfig)?;
    Ok(topo)
}
fn add_intc(topo: &mut Topology, index: usize, node: &Node<'_>) -> Result<()> {
    let phandle = parsed(node.u32("phandle"))?
        .filter(|id| *id != 0)
        .ok_or(HvError::InvalidConfig)?;
    if topo.intc[index] != 0 || topo.intc[..topo.count].contains(&phandle) {
        return Err(HvError::InvalidConfig);
    }
    topo.intc[index] = phandle;
    Ok(())
}
fn add_cpu(topo: &mut Topology, node: &Node<'_>) -> Result<usize> {
    let hart = parsed(node.reg())?.ok_or(HvError::InvalidConfig)?.0 as usize;
    let index = topo.count;
    if index == MAX_CPUS || topo.harts[..index].contains(&hart) {
        return Err(HvError::InvalidConfig);
    }
    let list = parsed(node.property("riscv,isa-extensions"))?;
    let isa = parsed(node.text("riscv,isa"))?
        .and_then(|s| s.strip_prefix("rv64"))
        .unwrap_or("");
    let has = |ext: &str| {
        if let Some(list) = list {
            return list
                .split(|byte| *byte == 0)
                .any(|item| item == ext.as_bytes());
        }
        let base = isa.split('_').next().unwrap_or("");
        if ext.len() == 1 {
            base.contains(ext) || (matches!(ext, "f" | "d") && base.contains('g'))
        } else {
            isa.split('_').any(|item| item == ext)
        }
    };
    let mmu = parsed(node.text("mmu-type"))?;
    if !has("h") || !matches!(mmu, Some("riscv,sv39" | "riscv,sv48" | "riscv,sv57")) {
        return Err(HvError::Unsupported);
    }
    let aia = has("ssaia");
    if index != 0 && topo.aia != aia {
        return Err(HvError::Unsupported);
    }
    topo.aia = aia;
    topo.fd &= has("f") && has("d") && !has("q");
    topo.harts[index] = hart;
    topo.count += 1;
    Ok(index)
}
pub fn memory(fdt: Fdt<'_>, entry: usize) -> Result<PhysRange> {
    for node in fdt.nodes() {
        let node = parsed(node)?;
        if node.depth() != 1 || parsed(node.text("device_type"))? != Some("memory") {
            continue;
        }
        let (base, bytes) = parsed(node.reg())?.ok_or(HvError::InvalidMemory)?;
        let range =
            PhysRange::new(base as usize, bytes as usize).map_err(|_| HvError::InvalidMemory)?;
        if (range.base()..range.end()).contains(&entry) {
            return Ok(range);
        }
    }
    Err(HvError::InvalidMemory)
}
pub fn check_soc(fdt: Fdt<'_>) -> Result<()> {
    let soc = parsed(fdt.find("/soc"))?.ok_or(HvError::InvalidConfig)?;
    if parsed(soc.property("ranges"))? == Some(&[]) {
        Ok(())
    } else {
        Err(HvError::Unsupported)
    }
}
pub fn resource(node: &Node<'_>, ram: PhysRange) -> Result<DeviceResource> {
    let (base, size) = parsed(node.reg())?.ok_or(HvError::InvalidDevice)?;
    let range = PhysRange::new(base as usize, size as usize).map_err(|_| HvError::InvalidDevice)?;
    if range.overlaps(ram) {
        return Err(HvError::InvalidDevice);
    }
    let irq = parsed(node.property("interrupts"))?
        .and_then(|raw| raw.get(..4))
        .map(cell);
    Ok(DeviceResource {
        base: range.base(),
        size: range.size(),
        irq,
    })
}
pub fn parsed<T>(value: pauli_hal::fdt::Result<T>) -> Result<T> {
    value.map_err(|_| HvError::InvalidConfig)
}
pub fn cell(raw: &[u8]) -> u32 {
    // Callers pass one cell from checked exact-size slices.
    u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])
}

/// PLIC's interleaved M/S contexts and IMSIC's S-only endpoints both use
/// firmware list positions, not dense owner indices or physical hart IDs.
pub fn supervisor_contexts(raw: &[u8], topo: &Topology) -> Result<Option<[usize; MAX_CPUS]>> {
    const SUPERVISOR_EXTERNAL: u32 = 9;
    if raw.is_empty() || !raw.len().is_multiple_of(8) {
        return Err(HvError::InvalidDevice);
    }
    let mut contexts = [usize::MAX; MAX_CPUS];
    for (index, entry) in raw.chunks_exact(8).enumerate() {
        if cell(&entry[4..]) != SUPERVISOR_EXTERNAL {
            continue;
        }
        let owner = topo.intc[..topo.count]
            .iter()
            .position(|id| *id == cell(entry))
            .ok_or(HvError::InvalidDevice)?;
        if contexts[owner] != usize::MAX {
            return Err(HvError::InvalidDevice);
        }
        contexts[owner] = index;
    }
    Ok((!contexts[..topo.count].contains(&usize::MAX)).then_some(contexts))
}
