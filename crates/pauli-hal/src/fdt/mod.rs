//! Bounded read-only FDT traversal for boot discovery, not a driver registry.
//! Ownership/Lock Ordering: borrowed immutable blob; no allocation, locks or callbacks.
//! Invariants: header, tags, strings and depth are validated before nodes are exposed.

mod node;
mod tokens;
pub use node::{Node, Nodes};
use tokens::{Token, Tokens};

const MAGIC: u32 = 0xd00d_feed;
const HEADER_BYTES: usize = 40;
const TOTAL_SIZE: usize = 4;
const STRUCT_OFFSET: usize = 8;
const STRINGS_OFFSET: usize = 12;
const RESERVATIONS_OFFSET: usize = 16;
const VERSION: usize = 20;
const STRINGS_SIZE: usize = 32;
const STRUCT_SIZE: usize = 36;
pub const MAX_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Header,
    Bounds,
    Version,
    Token,
    Depth,
    String,
    Cells,
    Property,
    Reservation,
}
pub type Result<T> = core::result::Result<T, Error>;

#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    structure: &'a [u8],
    strings: &'a [u8],
    reservations: &'a [u8],
}

impl<'a> Fdt<'a> {
    pub fn new(blob: &'a [u8]) -> Result<Self> {
        if blob.len() < HEADER_BYTES || word(blob, 0)? != MAGIC {
            return Err(Error::Header);
        }
        let total = word(blob, TOTAL_SIZE)? as usize;
        if total < HEADER_BYTES || total > blob.len() {
            return Err(Error::Bounds);
        }
        if word(blob, VERSION)? < 17 {
            return Err(Error::Version);
        }
        let blob = &blob[..total];
        let structure_offset = word(blob, STRUCT_OFFSET)? as usize;
        let strings_offset = word(blob, STRINGS_OFFSET)? as usize;
        let structure_size = word(blob, STRUCT_SIZE)? as usize;
        let strings_size = word(blob, STRINGS_SIZE)? as usize;
        let structure = block(blob, structure_offset, structure_size, 4)?;
        let strings = block(blob, strings_offset, strings_size, 4)?;
        if structure_size % 4 != 0
            || overlap(
                structure_offset,
                structure_size,
                strings_offset,
                strings_size,
            )
        {
            return Err(Error::Bounds);
        }
        let reserve = word(blob, RESERVATIONS_OFFSET)? as usize;
        let limit = structure_offset.min(strings_offset);
        if reserve < HEADER_BYTES || reserve % 8 != 0 || reserve > limit {
            return Err(Error::Reservation);
        }
        let reservations = blob.get(reserve..limit).ok_or(Error::Bounds)?;
        let fdt = Self {
            structure,
            strings,
            reservations,
        };
        fdt.validate()?;
        Ok(fdt)
    }
    fn validate(&self) -> Result<()> {
        let mut tokens = Tokens::new(self.structure, self.strings);
        let mut depth = 0usize;
        let mut root = false;
        let mut children = [false; MAX_DEPTH];
        loop {
            match tokens.next()? {
                Token::Begin(name) => {
                    if depth >= MAX_DEPTH {
                        return Err(Error::Depth);
                    }
                    if depth == 0 {
                        if root || !name.is_empty() {
                            return Err(Error::Token);
                        }
                        root = true;
                    } else {
                        children[depth - 1] = true;
                    }
                    children[depth] = false;
                    depth += 1;
                }
                Token::EndNode => {
                    depth = depth.checked_sub(1).ok_or(Error::Token)?;
                }
                Token::Property(_, _) if depth == 0 || children[depth - 1] => {
                    return Err(Error::Property);
                }
                Token::Property(_, _) | Token::Nop => {}
                Token::End if depth == 0 && root => break,
                Token::End => return Err(Error::Token),
            }
        }
        // Validate the reservation terminator even if a board does not inspect it.
        self.for_each_reservation(|_, _| Ok(()))
    }
    pub fn nodes(self) -> Nodes<'a> {
        Nodes::new(self)
    }
    pub fn find(self, path: &str) -> Result<Option<Node<'a>>> {
        if !path.starts_with('/') {
            return Err(Error::String);
        }
        let components = || path.split('/').filter(|part| !part.is_empty());
        let count = components().count();
        if count >= MAX_DEPTH {
            return Err(Error::Depth);
        }
        let mut matched = [false; MAX_DEPTH];
        for node in self.nodes() {
            let node = node?;
            let depth = node.depth();
            matched[depth] = depth == 0
                || (matched[depth - 1] && components().nth(depth - 1) == Some(node.name()));
            if matched[depth] && depth == count {
                return Ok(Some(node));
            }
        }
        Ok(None)
    }
    pub fn for_each_reservation(self, mut visit: impl FnMut(u64, u64) -> Result<()>) -> Result<()> {
        for pair in self.reservations.chunks_exact(16) {
            let base = cells(&pair[..8], 2)?;
            let size = cells(&pair[8..], 2)?;
            if base == 0 && size == 0 {
                return Ok(());
            }
            if size == 0 || base.checked_add(size).is_none() {
                return Err(Error::Reservation);
            }
            visit(base, size)?;
        }
        Err(Error::Reservation)
    }
}

fn block(blob: &[u8], start: usize, size: usize, alignment: usize) -> Result<&[u8]> {
    if start < HEADER_BYTES || start % alignment != 0 {
        return Err(Error::Bounds);
    }
    blob.get(start..start.checked_add(size).ok_or(Error::Bounds)?)
        .ok_or(Error::Bounds)
}
fn overlap(a: usize, al: usize, b: usize, bl: usize) -> bool {
    a < b + bl && b < a + al
}
fn word(blob: &[u8], start: usize) -> Result<u32> {
    let raw = blob
        .get(start..start.checked_add(4).ok_or(Error::Bounds)?)
        .ok_or(Error::Bounds)?;
    Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
}
fn cells(raw: &[u8], count: usize) -> Result<u64> {
    if count > 2 || raw.len() != count * 4 {
        return Err(Error::Cells);
    }
    let mut value = 0u64;
    for chunk in raw.chunks_exact(4) {
        value = (value << 32) | u64::from(word(chunk, 0)?);
    }
    Ok(value)
}
