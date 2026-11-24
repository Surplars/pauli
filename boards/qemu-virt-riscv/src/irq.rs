//! Select the native supervisor IRQ domain from FDT, never a machine domain.
//! Ownership/Lock Ordering: boot hart configures routes; handler does bounded dispatch.
//! Invariants: native file zero is HS-only; MSI route indices come from FDT endpoints.
use crate::fdt::{Topology, parsed, resource, supervisor_contexts};
use pauli_arch_riscv::host::{
    self,
    aplic::{Aplic, SOURCE_LEVEL_HIGH},
    imsic,
    plic::Plic,
    trap::Event,
};
use pauli_core::{HvError, Result};
use pauli_hal::{
    fdt::{Fdt, Node},
    irq::{HostIrqController, IrqRoutes},
    mem::PhysRange,
    sync::Once,
};

const MAX_CLAIMS: usize = 16;
pub enum Root {
    Plic(Plic),
    Direct(Aplic, usize),
    Msi(Aplic, usize, usize),
}
pub static ROUTER: Once<(Root, IrqRoutes<6>)> = Once::new();
impl Root {
    pub fn discover(
        fdt: Fdt<'_>,
        topology: &Topology,
        ram: PhysRange,
    ) -> Result<(Self, Option<usize>)> {
        let files = supervisor_imsic(fdt, topology, ram)?;
        for node in fdt.nodes() {
            let node = parsed(node)?;
            if node.depth() != 2 || !parsed(node.available())? {
                continue;
            }
            if parsed(node.compatible("riscv,plic0"))?
                || parsed(node.compatible("sifive,plic-1.0.0"))?
            {
                let sources = parsed(node.u32("riscv,ndev"))?.ok_or(HvError::InvalidDevice)?;
                let contexts = contexts(&node, topology)?.ok_or(HvError::InvalidDevice)?;
                // SAFETY: FDT selected only S contexts and validated the native register range.
                let plic =
                    unsafe { Plic::new(resource(&node, ram)?, contexts, topology.count, sources) }?;
                return Ok((Self::Plic(plic), None));
            }
            if parsed(node.compatible("riscv,aplic"))? {
                if let Some(root) = aplic(&node, topology, ram, files)? {
                    return Ok(root);
                }
            }
        }
        Err(HvError::Unsupported)
    }
    pub fn enable(&self, source: u32) -> Result<()> {
        match self {
            Self::Plic(plic) => plic.enable(host::cpu_id(), source)?,
            Self::Direct(aplic, target) => {
                aplic.route(source, *target, source, SOURCE_LEVEL_HIGH)?;
                aplic.set_enabled(source, true)?;
            }
            Self::Msi(aplic, target, identities) => {
                if source == 0 || source as usize > *identities {
                    return Err(HvError::InvalidDevice);
                }
                aplic.route(source, *target, source, SOURCE_LEVEL_HIGH)?;
                // SAFETY: only the discovered boot hart S-file and its assigned source identity.
                unsafe {
                    imsic::set_enabled(source as usize, true);
                }
                aplic.set_enabled(source, true)?;
                aplic.rearm_level(source)?;
            }
        }
        Ok(())
    }
    fn claim(&self) -> Option<u32> {
        match self {
            Self::Plic(plic) => plic.claim(host::cpu_id()),
            Self::Direct(aplic, idx) => aplic.claim_direct(*idx),
            Self::Msi(..) => {
                // SAFETY: CSR access only on the boot hart owning this discovered S-file.
                let source = unsafe { imsic::claim() };
                (source != 0).then_some(source)
            }
        }
    }
    fn complete(&self, source: u32) {
        match self {
            Self::Plic(plic) => plic.complete(host::cpu_id(), source),
            Self::Msi(aplic, ..) => {
                let _ = aplic.rearm_level(source);
            }
            Self::Direct(..) => {}
        }
    }
}
pub fn event(event: Event) {
    if !matches!(event, Event::External) {
        return;
    }
    let Some((root, routes)) = ROUTER.get() else {
        return;
    };
    for _ in 0..MAX_CLAIMS {
        let Some(source) = root.claim() else {
            break;
        };
        routes.dispatch(source);
        root.complete(source);
    }
}
fn contexts(node: &Node<'_>, topo: &Topology) -> Result<Option<[usize; pauli_arch_riscv::MAX_CPUS]>> {
    let Some(raw) = parsed(node.property("interrupts-extended"))? else {
        return Ok(None);
    };
    supervisor_contexts(raw, topo)
}
#[derive(Clone, Copy)]
struct Files {
    phandle: u32,
    target: usize,
    ids: usize,
    guest_ids: Option<usize>,
}
fn supervisor_imsic(fdt: Fdt<'_>, topo: &Topology, ram: PhysRange) -> Result<Option<Files>> {
    if !topo.aia {
        return Ok(None);
    }
    for node in fdt.nodes() {
        let node = parsed(node)?;
        if node.depth() != 2
            || !parsed(node.compatible("riscv,imsics"))?
            || !parsed(node.available())?
        {
            continue;
        }
        let Some(context) = contexts(&node, topo)? else {
            continue;
        };
        let ids = parsed(node.u32("riscv,num-ids"))?.ok_or(HvError::InvalidDevice)? as usize;
        let phandle = parsed(node.u32("phandle"))?.ok_or(HvError::InvalidDevice)?;
        let bits = parsed(node.u32("riscv,guest-index-bits"))?.unwrap_or(0);
        if parsed(node.u32("riscv,group-index-bits"))?.unwrap_or(0) != 0 || bits > 7 {
            return Err(HvError::Unsupported);
        }
        let res = resource(&node, ram)?;
        if res.size < (4096 << bits) * topo.count {
            return Err(HvError::InvalidDevice);
        }
        let guest_ids = if bits != 0 {
            Some(parsed(node.u32("riscv,num-guest-ids"))?.map_or(ids, |n| n as usize))
        } else {
            None
        };
        return Ok(Some(Files {
            phandle,
            target: context[topo.boot],
            ids,
            guest_ids,
        }));
    }
    Ok(None)
}
fn aplic(
    node: &Node<'_>,
    topo: &Topology,
    ram: PhysRange,
    files: Option<Files>,
) -> Result<Option<(Root, Option<usize>)>> {
    let sources = parsed(node.u32("riscv,num-sources"))?.ok_or(HvError::InvalidDevice)?;
    if let Some(parent) = parsed(node.u32("msi-parent"))? {
        let Some(files) = files.filter(|files| files.phandle == parent) else {
            return Ok(None);
        };
        // SAFETY: the matched S-file is on this exclusive boot owner; S-domain
        // MSI destination was configured by OpenSBI, not by the guest.
        let root = unsafe {
            imsic::init(files.ids)?;
            Aplic::new(resource(node, ram)?, sources, true)?
        };
        return Ok(Some((
            Root::Msi(root, files.target, files.ids),
            files.guest_ids,
        )));
    }
    let Some(context) = contexts(node, topo)? else {
        return Ok(None);
    };
    // SAFETY: interrupts-extended selected the supervisor direct domain.
    let root = unsafe { Aplic::new(resource(node, ram)?, sources, false) }?;
    root.init_direct(context[topo.boot])?;
    Ok(Some((Root::Direct(root, context[topo.boot]), None)))
}
