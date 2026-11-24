//! Physical grants for the embedded probe or one externally loaded Linux guest.
//! Only the boot hart walks the immutable native DTB. No physical pointers here:
//! an address becomes usable only when the fixed layout, DDR and reservations agree.
use super::{External, PhysRange, RAM_BASE, RAM_BYTES, node};
use pauli_hal::fdt::{Error, Fdt, Node, Result};

const IMAGE_BASE: usize = 0x8900_0000;
const IMAGE_LIMIT: usize = 64 << 20;
const INITRD_BASE: usize = 0x8100_0000;
const INITRD_LIMIT: usize = 128 << 20;
const GUEST_RAM_BASE: usize = 0xa000_0000;
const GUEST_RAM_SIZE: usize = 512 << 20;
const GUEST_COMPAT: &str = "openion,wave8-guest-ram";
const DEFAULT_ARGS: &str = "console=ttyS0,115200 rdinit=/init";
const MAX_ARGS: usize = 2048;

fn number(raw: &[u8]) -> Result<u64> {
    raw.try_into()
        .map(u64::from_be_bytes)
        .map_err(|_| Error::Cells)
}

fn range(base: u64, size: u64) -> Result<PhysRange> {
    PhysRange::new(
        usize::try_from(base).map_err(|_| Error::Bounds)?,
        usize::try_from(size).map_err(|_| Error::Bounds)?,
    )
    .map_err(|_| Error::Bounds)
}

fn reg(raw: &[u8]) -> Result<PhysRange> {
    if raw.len() != 16 {
        return Err(Error::Cells);
    }
    range(number(&raw[..8])?, number(&raw[8..])?)
}

// Both native buses use two address cells and two size cells. Check every
// tuple, including reservations OpenSBI added after the board DTB was built.
fn each_reg(n: &Node<'_>, mut visit: impl FnMut(PhysRange) -> Result<()>) -> Result<()> {
    let raw = n.property("reg")?.ok_or(Error::Cells)?;
    if raw.is_empty() || !raw.len().is_multiple_of(16) {
        return Err(Error::Cells);
    }
    for item in raw.chunks_exact(16) {
        visit(reg(item)?)?;
    }
    Ok(())
}

fn external<'a>(chosen: &Node<'a>, host: PhysRange) -> Result<Option<External<'a>>> {
    let initrd = match (
        chosen.property("linux,initrd-start")?,
        chosen.property("linux,initrd-end")?,
    ) {
        (None, None) => None,
        (Some(a), Some(b)) => {
            let start = number(a)?;
            let source = range(start, number(b)?.checked_sub(start).ok_or(Error::Bounds)?)?;
            if source.overlaps(host) {
                return Err(Error::Reservation);
            }
            Some(source)
        }
        _ => return Err(Error::Property),
    };
    let args = chosen.property("openion,guest-bootargs")?;
    let role = chosen.unique_property("openion,guest-role")?;
    let Some(raw) = chosen.unique_property("openion,guest-image")? else {
        // A stock Linux initrd is not an instruction to start a VM.
        return if args.is_none() && role.is_none() {
            Ok(None)
        } else {
            Err(Error::Property)
        };
    };
    // This is a native-loader grant, never an option interpreted from a
    // Linux command line. Match the complete DT string, including its NUL.
    let manager = match role {
        None | Some(b"guest\0") => false,
        Some(b"manager\0") => true,
        _ => return Err(Error::Property),
    };
    let image = reg(raw)?;
    if image.base() != IMAGE_BASE
        || image.size() > IMAGE_LIMIT
        || initrd.is_some_and(|r| r.base() != INITRD_BASE || r.size() > INITRD_LIMIT)
    {
        return Err(Error::Bounds);
    }
    let bootargs = match args {
        None => DEFAULT_ARGS,
        Some(raw) => {
            let body = raw.strip_suffix(&[0]).ok_or(Error::String)?;
            if body.len() > MAX_ARGS || body.contains(&0) {
                return Err(Error::String);
            }
            core::str::from_utf8(body).map_err(|_| Error::String)?
        }
    };
    let ram = PhysRange::new(GUEST_RAM_BASE, GUEST_RAM_SIZE).map_err(|_| Error::Bounds)?;
    Ok(Some(External {
        image,
        initrd,
        ram,
        bootargs,
        manager,
    }))
}

pub(super) fn granted(tree: Fdt<'_>) -> Result<Option<External<'_>>> {
    let host = PhysRange::new(RAM_BASE, RAM_BYTES).map_err(|_| Error::Bounds)?;
    let guest = match tree.find("/chosen")? {
        Some(chosen) => external(&chosen, host)?,
        None => None,
    };
    let windows = [
        Some(host),
        guest.map(|g| g.image),
        guest.and_then(|g| g.initrd),
        guest.map(|g| g.ram),
    ];
    let mut covered = windows.map(|w| w.is_none());
    let disjoint = |region: PhysRange| {
        if windows.iter().flatten().any(|w| w.overlaps(region)) {
            Err(Error::Reservation)
        } else {
            Ok(())
        }
    };
    for path in ["/", "/reserved-memory"] {
        let bus = node(tree, path)?;
        if bus.u32("#address-cells")? != Some(2)
            || bus.u32("#size-cells")? != Some(2)
            || (path != "/" && bus.property("ranges")? != Some(&[]))
        {
            return Err(Error::Cells);
        }
    }
    tree.for_each_reservation(|base, size| disjoint(range(base, size)?))?;
    let mut reserved = false;
    let mut granted_ram = false;
    let mut saw_chosen = false;
    for n in tree.nodes() {
        let n = n?;
        if n.depth() == 1 {
            // The chosen policy must be unique, not selected by sibling order.
            if n.name() == "chosen" && core::mem::replace(&mut saw_chosen, true) {
                return Err(Error::Property);
            }
            reserved = n.name() == "reserved-memory";
            if n.text("device_type")? == Some("memory") && n.available()? {
                each_reg(&n, |bank| {
                    for (seen, w) in covered.iter_mut().zip(windows) {
                        *seen |= w.is_some_and(|r| bank.contains(r));
                    }
                    Ok(())
                })?;
            }
        } else if reserved && n.depth() == 2 && n.available()? {
            if !n.compatible(GUEST_COMPAT)? {
                each_reg(&n, disjoint)?;
                continue;
            }
            // This is the only reservation exempt from the overlap check,
            // and only for its own exact tuple. No alias or extra tuple allowed.
            let declared = reg(n.property("reg")?.ok_or(Error::Reservation)?)?;
            if granted_ram
                || guest.map(|g| g.ram) != Some(declared)
                || n.property("no-map")? != Some(&[])
                || n.property("reusable")?.is_some()
            {
                return Err(Error::Reservation);
            }
            granted_ram = true;
        }
    }
    if granted_ram != guest.is_some() || !covered.into_iter().all(|yes| yes) {
        return Err(Error::Reservation);
    }
    Ok(guest)
}

#[cfg(test)]
mod tests;
