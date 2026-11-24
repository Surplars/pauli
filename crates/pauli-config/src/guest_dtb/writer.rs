//! Heap-free flattened tree encoder for a caller-owned output buffer.
//! Ownership/Lock Ordering: one private builder, no hardware or locks.
//! Invariants: structure and strings never overlap; only finish publishes a length.
use crate::{Error, MAX_GUEST_DTB, MmioRegion};

type Result<T = ()> = core::result::Result<T, Error>;
const HEADER: usize = 40;
const STRUCTURE: usize = HEADER + 16; // One terminating reservation pair.
const MAGIC: u32 = 0xd00d_feed;
const BEGIN: u32 = 1;
const END_NODE: u32 = 2;
const PROPERTY: u32 = 3;
const END: u32 = 9;

pub(super) struct Writer<'a> {
    out: &'a mut [u8],
    cursor: usize,
    strings: usize,
}
impl<'a> Writer<'a> {
    pub fn new(out: &'a mut [u8]) -> Result<Self> {
        let len = out.len().min(MAX_GUEST_DTB);
        if len < STRUCTURE {
            return Err(Error::Dtb);
        }
        Ok(Self {
            out: &mut out[..len],
            cursor: STRUCTURE,
            strings: len,
        })
    }
    fn push(&mut self, bytes: &[u8]) -> Result {
        self.out[self.cursor..self.strings]
            .get_mut(..bytes.len())
            .ok_or(Error::Dtb)?
            .copy_from_slice(bytes);
        self.cursor += bytes.len();
        Ok(())
    }
    fn word(&mut self, value: u32) -> Result {
        self.push(&value.to_be_bytes())
    }
    fn align(&mut self) -> Result {
        self.push(&[0; 3][..self.cursor.wrapping_neg() & 3])
    }
    pub fn begin(&mut self, parts: &[&[u8]]) -> Result {
        self.word(BEGIN)?;
        for part in parts {
            self.push(part)?;
        }
        self.push(&[0])?;
        self.align()
    }
    pub fn at(&mut self, name: &[u8], address: u64) -> Result {
        self.begin(&[name, b"@", Hex::new(address).bytes()])
    }
    pub fn end(&mut self) -> Result {
        self.word(END_NODE)
    }
    pub fn prop(&mut self, name: &str, parts: &[&[u8]]) -> Result {
        // Append keys backwards from the far end. Their *final* forward offsets
        // are known immediately, without a second rendering pass or a key table.
        let offset = self.out.len() - self.strings;
        let start = self
            .strings
            .checked_sub(name.len() + 1)
            .filter(|&start| start >= self.cursor)
            .ok_or(Error::Dtb)?;
        let key = &mut self.out[start..self.strings];
        key[0] = 0;
        for (dst, src) in key[1..].iter_mut().zip(name.bytes().rev()) {
            *dst = src;
        }
        self.strings = start;
        let len = parts
            .iter()
            .try_fold(0u32, |n, p| n.checked_add(u32::try_from(p.len()).ok()?))
            .ok_or(Error::Dtb)?;
        self.word(PROPERTY)?;
        self.word(len)?;
        self.word(offset as u32)?;
        for part in parts {
            self.push(part)?;
        }
        self.align()
    }
    pub fn text(&mut self, name: &str, value: &str) -> Result {
        self.prop(name, &[value.as_bytes(), &[0]])
    }
    pub fn u32(&mut self, name: &str, v: u32) -> Result {
        self.prop(name, &[&v.to_be_bytes()])
    }
    pub fn u64(&mut self, name: &str, v: u64) -> Result {
        self.prop(name, &[&v.to_be_bytes()])
    }
    pub fn reg(&mut self, r: MmioRegion) -> Result {
        self.prop("reg", &[&r.base.to_be_bytes(), &r.size.to_be_bytes()])
    }
    pub fn finish(mut self) -> Result<usize> {
        self.word(END)?;
        let strings = self.out.len() - self.strings;
        let total = self.cursor + strings;
        self.out[self.strings..].reverse();
        self.out.copy_within(self.strings.., self.cursor);
        let header = [
            MAGIC,
            total as u32,
            STRUCTURE as u32,
            self.cursor as u32,
            HEADER as u32,
            17,
            16,
            0,
            strings as u32,
            (self.cursor - STRUCTURE) as u32,
        ];
        for (dst, word) in self.out[..HEADER].chunks_exact_mut(4).zip(header) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        self.out[HEADER..STRUCTURE].fill(0);
        Ok(total)
    }
}

pub(super) struct Hex([u8; 16], usize);
impl Hex {
    pub fn new(mut value: u64) -> Self {
        let mut text = Self([0; 16], 16);
        loop {
            text.1 -= 1;
            text.0[text.1] = b"0123456789abcdef"[(value & 15) as usize];
            value >>= 4;
            if value == 0 {
                return text;
            }
        }
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0[self.1..]
    }
}
