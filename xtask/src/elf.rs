//! Read the linked RV64 load spans and produce a flat image without objcopy.
//! Ownership/Lock Ordering: local input ELF, output only after complete validation.
//! Invariants: physical LOAD spans stay within the board entry/carveout, including BSS.
use crate::config::Board;
use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};
pub mod firmware;
pub struct Layout {
    pub entry: u64,
    pub file_span: u64,
    pub ram_span: u64,
}
struct Segment {
    offset: usize,
    bytes: usize,
    address: u64,
    memory: u64,
    vaddr: u64,
    flags: u32,
}
fn read<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N]> {
    data.get(at..at.checked_add(N).context("ELF overflow")?)
        .context("truncated ELF")?
        .try_into()
        .context("ELF field")
}
fn segments(data: &[u8], expected: u64, limit: u64) -> Result<(Layout, Vec<Segment>)> {
    ensure!(
        data.get(..6) == Some(b"\x7fELF\x02\x01") && u16::from_le_bytes(read(data, 18)?) == 243,
        "expected RV64 little-endian ELF"
    );
    let entry = u64::from_le_bytes(read(data, 24)?);
    ensure!(
        entry == expected,
        "ELF entry {entry:#x} differs from configured {expected:#x}"
    );
    let phoff = usize::try_from(u64::from_le_bytes(read(data, 32)?))?;
    let stride = usize::from(u16::from_le_bytes(read(data, 54)?));
    let count = usize::from(u16::from_le_bytes(read(data, 56)?));
    ensure!(
        stride >= 56 && (1..=32).contains(&count),
        "bad ELF program header table"
    );
    let header = data
        .get(phoff..phoff.checked_add(count * stride).context("ELF overflow")?)
        .context("program header bounds")?;
    let mut out = Vec::new();
    let mut span = Layout {
        entry,
        file_span: 0,
        ram_span: 0,
    };
    for ph in header.chunks_exact(stride) {
        let kind = u32::from_le_bytes(read(ph, 0)?);
        ensure!(
            !matches!(kind, 2 | 3),
            "expected a static ELF without interpreter"
        );
        if kind != 1 {
            continue;
        }
        let part = Segment {
            vaddr: u64::from_le_bytes(read(ph, 16)?),
            flags: u32::from_le_bytes(read(ph, 4)?),
            offset: usize::try_from(u64::from_le_bytes(read(ph, 8)?))?,
            address: u64::from_le_bytes(read(ph, 24)?),
            bytes: usize::try_from(u64::from_le_bytes(read(ph, 32)?))?,
            memory: u64::from_le_bytes(read(ph, 40)?),
        };
        let base = part
            .address
            .checked_sub(entry)
            .context("ELF segment before entry")?;
        let end = base
            .checked_add(part.memory)
            .context("ELF segment overflow")?;
        ensure!(
            part.bytes as u64 <= part.memory
                && part
                    .offset
                    .checked_add(part.bytes)
                    .is_some_and(|n| n <= data.len())
                && end <= limit,
            "ELF load segment outside assigned memory"
        );
        span.ram_span = span.ram_span.max(end);
        if part.bytes != 0 {
            span.file_span = span.file_span.max(base + part.bytes as u64);
        }
        out.push(part);
    }
    ensure!(
        span.file_span != 0
            && out
                .iter()
                .any(|part| part.address == entry && part.bytes != 0),
        "ELF has no entry bytes"
    );
    Ok((span, out))
}
pub fn inspect(input: &Path, board: &Board) -> Result<Layout> {
    let elf = fs::read(input)?;
    Ok(segments(&elf, board.entry, board.limit)?.0)
}
pub fn image(input: &Path, output: &Path, board: &Board) -> Result<()> {
    ensure!(input != output, "refusing to overwrite the ELF");
    let (_, flat) = flat_image(input, board)?;
    // Create rather than replace arbitrary user paths. The build target can be
    // regenerated; choose another --output or remove an obsolete .bin explicitly.
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?
        .write_all(&flat)?;
    Ok(())
}
pub(crate) fn flat_image(input: &Path, board: &Board) -> Result<(Layout, Vec<u8>)> {
    let data = fs::read(input)?;
    let (layout, segments) = segments(&data, board.entry, board.limit)?;
    let flat = flatten(&data, &layout, segments);
    Ok((layout, flat))
}
fn flatten(data: &[u8], layout: &Layout, segments: Vec<Segment>) -> Vec<u8> {
    let mut flat = vec![0; layout.file_span as usize];
    for part in segments.into_iter().filter(|part| part.bytes != 0) {
        let start = (part.address - layout.entry) as usize;
        flat[start..start + part.bytes]
            .copy_from_slice(&data[part.offset..part.offset + part.bytes]);
    }
    flat
}
