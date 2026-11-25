//! Read Sstc from the immutable image actually delivered to every guest CPU.
use crate::{Error, MAX_GUEST_VCPUS};
use pauli_hal::fdt::{self, Fdt, Node};

/// Both ISA encodings, when present, must agree on every configured guest hart.
pub fn riscv_sstc(blob: &[u8], vcpus: u8) -> Result<bool, Error> {
    if !(1..=MAX_GUEST_VCPUS).contains(&vcpus) {
        return Err(Error::Vcpus);
    }
    inspect(blob, vcpus).map_err(|_| Error::Dtb)
}

fn inspect(blob: &[u8], vcpus: u8) -> fdt::Result<bool> {
    let mut in_cpus = false;
    let mut found_bus = false;
    let mut address_cells = 0;
    let mut harts = 0usize;
    let mut seen = 0usize;
    let mut capability = None;
    for node in Fdt::new(blob)?.nodes() {
        let node = node?;
        if node.depth() == 1 {
            in_cpus = node.name() == "cpus";
            if in_cpus {
                if found_bus || node.unique_property("#size-cells")? != Some(&0u32.to_be_bytes()) {
                    return Err(fdt::Error::Cells);
                }
                found_bus = true;
                address_cells = match node.unique_property("#address-cells")? {
                    Some([0, 0, 0, 1]) => 1,
                    Some([0, 0, 0, 2]) => 2,
                    _ => return Err(fdt::Error::Cells),
                };
            }
        }
        if !in_cpus || node.depth() != 2 {
            continue;
        }
        let cpu_type = node.unique_property("device_type")?;
        if node.name().split('@').next() != Some("cpu") && cpu_type != Some(b"cpu\0") {
            continue;
        }
        if cpu_type.is_some_and(|kind| kind != b"cpu\0") {
            return Err(fdt::Error::Property);
        }
        let reg = node.unique_property("reg")?.ok_or(fdt::Error::Cells)?;
        if reg.len() != address_cells * 4 {
            return Err(fdt::Error::Cells);
        }
        let (hart, _) = node.reg()?.ok_or(fdt::Error::Cells)?;
        let available = match node.unique_property("status")? {
            None | Some(b"okay\0" | b"ok\0") => true,
            Some(b"disabled\0" | b"reserved\0" | b"fail\0") => false,
            Some(reason)
                if reason.starts_with(b"fail-")
                    && reason.strip_suffix(&[0]).is_some_and(|s| !s.contains(&0)) =>
            {
                false
            }
            _ => return Err(fdt::Error::String),
        };
        if hart < u64::from(vcpus) {
            if seen & (1 << hart) != 0 {
                return Err(fdt::Error::Cells);
            }
            seen |= 1 << hart;
        }
        if !available {
            continue;
        }
        if hart >= u64::from(vcpus) {
            return Err(fdt::Error::Cells);
        }
        harts |= 1 << hart;
        let current = cpu_sstc(&node)?;
        if capability
            .replace(current)
            .is_some_and(|old| old != current)
        {
            return Err(fdt::Error::Property);
        }
    }
    if harts != (1usize << vcpus) - 1 {
        return Err(fdt::Error::Cells);
    }
    Ok(capability == Some(true))
}

fn cpu_sstc(node: &Node<'_>) -> fdt::Result<bool> {
    let modern = node
        .unique_property("riscv,isa-extensions")?
        .map(extensions)
        .transpose()?;
    let legacy = node
        .unique_property("riscv,isa")?
        .map(legacy_isa)
        .transpose()?;
    if matches!((modern, legacy), (Some(a), Some(b)) if a != b) {
        return Err(fdt::Error::Property);
    }
    Ok(modern.or(legacy).unwrap_or(false))
}

fn extensions(raw: &[u8]) -> fdt::Result<bool> {
    let list = raw.strip_suffix(&[0]).ok_or(fdt::Error::String)?;
    let mut found = false;
    for token in list.split(|&c| c == 0) {
        if !isa_token(token)
            || token
                .strip_prefix(b"sstc")
                .and_then(|suffix| suffix.first())
                .is_some_and(u8::is_ascii_digit)
        {
            return Err(fdt::Error::String);
        }
        if token == b"sstc" {
            if found {
                return Err(fdt::Error::Property);
            }
            found = true;
        }
    }
    Ok(found)
}

fn legacy_isa(raw: &[u8]) -> fdt::Result<bool> {
    let text = core::str::from_utf8(raw.strip_suffix(&[0]).ok_or(fdt::Error::String)?)
        .map_err(|_| fdt::Error::String)?;
    let mut tokens = text
        .strip_prefix("rv64")
        .ok_or(fdt::Error::String)?
        .split('_');
    let base = tokens.next().ok_or(fdt::Error::String)?;
    if !isa_token(base.as_bytes()) || !base.starts_with(['i', 'g', 'e']) {
        return Err(fdt::Error::String);
    }
    // A multi-letter extension may directly follow the base's single-letter
    // extensions. Recognize Sstc there only when it is the first such extension.
    let mut found = match base.find(['s', 'z', 'x']) {
        Some(i) => sstc_token(&base[i..])?,
        None => false,
    };
    for token in tokens {
        if !isa_token(token.as_bytes()) {
            return Err(fdt::Error::String);
        }
        if sstc_token(token)? {
            if found {
                return Err(fdt::Error::Property);
            }
            found = true;
        }
    }
    Ok(found)
}

fn isa_token(name: &[u8]) -> bool {
    !name.is_empty()
        && name
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn sstc_token(token: &str) -> fdt::Result<bool> {
    let Some(version) = token.strip_prefix("sstc") else {
        return Ok(false);
    };
    let Some(first) = version.as_bytes().first() else {
        return Ok(true);
    };
    if !first.is_ascii_digit() {
        return Ok(false); // A distinct extension whose name starts with sstc.
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    if version
        .split_once('p')
        .map_or_else(|| digits(version), |(a, b)| digits(a) && digits(b))
    {
        Ok(true)
    } else {
        Err(fdt::Error::String)
    }
}

#[cfg(test)]
mod tests;
