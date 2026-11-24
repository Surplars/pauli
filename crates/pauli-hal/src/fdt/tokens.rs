//! Checked FDT structure tokens and UTF-8/NUL string decoding.
//! Ownership/Lock Ordering: iterator borrows immutable sections; no locks.
//! Invariants: each cursor advances inside its section; arithmetic cannot wrap.

use super::{Error, Result, word};
const BEGIN: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

pub(super) enum Token<'a> {
    Begin(&'a str),
    EndNode,
    Property(&'a str, &'a [u8]),
    Nop,
    End,
}
#[derive(Clone)]
pub(super) struct Tokens<'a> {
    structure: &'a [u8],
    strings: &'a [u8],
    pub offset: usize,
}
impl<'a> Tokens<'a> {
    pub fn new(structure: &'a [u8], strings: &'a [u8]) -> Self {
        Self {
            structure,
            strings,
            offset: 0,
        }
    }
    pub fn next(&mut self) -> Result<Token<'a>> {
        let tag = word(self.structure, self.offset)?;
        self.offset += 4;
        Ok(match tag {
            BEGIN => {
                let tail = self.structure.get(self.offset..).ok_or(Error::Bounds)?;
                let end = tail
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or(Error::String)?;
                self.offset = aligned(self.offset.checked_add(end + 1).ok_or(Error::Bounds)?)?;
                if self.offset > self.structure.len() {
                    return Err(Error::Bounds);
                }
                Token::Begin(core::str::from_utf8(&tail[..end]).map_err(|_| Error::String)?)
            }
            PROP => {
                let len = word(self.structure, self.offset)? as usize;
                let name = word(self.structure, self.offset + 4)? as usize;
                self.offset += 8;
                let end = self.offset.checked_add(len).ok_or(Error::Bounds)?;
                let data = self.structure.get(self.offset..end).ok_or(Error::Bounds)?;
                self.offset = aligned(end)?;
                if self.offset > self.structure.len() {
                    return Err(Error::Bounds);
                }
                Token::Property(
                    string(self.strings.get(name..).ok_or(Error::String)?)?,
                    data,
                )
            }
            END_NODE => Token::EndNode,
            NOP => Token::Nop,
            END => Token::End,
            _ => return Err(Error::Token),
        })
    }
}
fn aligned(value: usize) -> Result<usize> {
    value
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or(Error::Bounds)
}
pub(super) fn string(raw: &[u8]) -> Result<&str> {
    let end = raw
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::String)?;
    core::str::from_utf8(&raw[..end]).map_err(|_| Error::String)
}
