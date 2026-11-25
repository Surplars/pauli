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
const GIGA: usize = 1 << 30;
const ALIGNED_RAM_MIN: usize = 0xc000_0000;
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
    let raw = n.unique_property("reg")?.ok_or(Error::Cells)?;
    if raw.is_empty() || !raw.len().is_multiple_of(16) {
        return Err(Error::Cells);
    }
    for item in raw.chunks_exact(16) {
        visit(reg(item)?)?;
    }
    Ok(())
}

// Find the exclusive loan first: the RAM address is supplied by the native
// reservation and must be known before checking any bank or other reservation.
fn guest_ram(tree: Fdt<'_>) -> Result<Option<PhysRange>> {
    let mut ram = None;
    let mut reserved = false;
    let mut saw_reserved = false;
    for n in tree.nodes() {
        let n = n?;
        if n.depth() == 1 {
            reserved = n.name() == "reserved-memory";
            if reserved && core::mem::replace(&mut saw_reserved, true) {
                return Err(Error::Reservation);
            }
        } else if reserved && n.depth() == 2 && n.available()? {
            n.unique_property("compatible")?;
            if n.compatible(GUEST_COMPAT)? {
                if ram.is_some()
                    || n.unique_property("no-map")? != Some(&[])
                    || n.property("reusable")?.is_some()
                {
                    return Err(Error::Reservation);
                }
                ram = Some(reg(n.unique_property("reg")?.ok_or(Error::Reservation)?)?);
            }
        }
    }
    Ok(ram)
}

fn external<'a>(
    chosen: &Node<'a>,
    host: PhysRange,
    declared: Option<PhysRange>,
) -> Result<Option<External<'a>>> {
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
    let irq = chosen.unique_property("openion,guest-irq")?;
    let Some(raw) = chosen.unique_property("openion,guest-image")? else {
        // A stock Linux initrd is not an instruction to start a VM.
        return if args.is_none() && role.is_none() && irq.is_none() {
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
    let aia = match irq {
        None | Some(b"plic\0") => false,
        Some(b"aia\0") => true,
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
    let ram = declared.ok_or(Error::Reservation)?;
    let legacy = ram.base() == GUEST_RAM_BASE && ram.size() == GUEST_RAM_SIZE;
    // Sv39x4 represents 56-bit HPAs. A manager needs a private tail beyond
    // the single Linux root leaf for its firmware service.
    let tail = if manager {
        pauli_config::ALIGN as usize
    } else {
        0
    };
    let aligned = ram.base() >= ALIGNED_RAM_MIN
        && ram.base().is_multiple_of(GIGA)
        && ram.size() == GIGA + tail
        && ram.end() <= 1usize << 56;
    if !legacy && !aligned {
        return Err(Error::Reservation);
    }
    Ok(Some(External {
        image,
        initrd,
        ram,
        bootargs,
        manager,
        aia,
    }))
}

pub(super) fn granted(tree: Fdt<'_>) -> Result<Option<External<'_>>> {
    let host = PhysRange::new(RAM_BASE, RAM_BYTES).map_err(|_| Error::Bounds)?;
    let declared = guest_ram(tree)?;
    let guest = match tree.find("/chosen")? {
        Some(chosen) => external(&chosen, host, declared)?,
        None => None,
    };
    if declared.is_some() && guest.is_none() {
        return Err(Error::Reservation);
    }
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
        } else if reserved && n.depth() == 2 && n.available()? && !n.compatible(GUEST_COMPAT)? {
            // The sole compatible reservation was validated by guest_ram().
            each_reg(&n, disjoint)?;
        }
    }
    if !covered.into_iter().all(|yes| yes) {
        return Err(Error::Reservation);
    }
    Ok(guest)
}

#[cfg(test)]
mod tests;
