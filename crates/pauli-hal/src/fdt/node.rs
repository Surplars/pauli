//! FDT nodes with parent bus cell widths and borrowed property access.
//! Ownership/Lock Ordering: immutable blob views, no global registry or locks.
//! Invariants: reg returns bus addresses; translating nonidentity buses is board policy.

use super::{
    Error, Fdt, MAX_DEPTH, Result, cells,
    tokens::{Token, Tokens, string},
    word,
};

#[derive(Clone, Copy)]
struct Context {
    address: usize,
    size: usize,
}
const DEFAULT: Context = Context {
    address: 2,
    size: 1,
};

pub struct Node<'a> {
    name: &'a str,
    depth: usize,
    properties: &'a [u8],
    strings: &'a [u8],
    parent: Context,
}
impl<'a> Node<'a> {
    pub fn name(&self) -> &'a str {
        self.name
    }
    pub fn depth(&self) -> usize {
        self.depth
    }
    pub fn property(&self, name: &str) -> Result<Option<&'a [u8]>> {
        self.lookup(name, false)
    }
    /// A second definition of an authority-bearing property is ambiguous,
    /// even if both contain the same bytes.
    pub fn unique_property(&self, name: &str) -> Result<Option<&'a [u8]>> {
        self.lookup(name, true)
    }
    fn lookup(&self, name: &str, unique: bool) -> Result<Option<&'a [u8]>> {
        let mut found = None;
        let mut tokens = Tokens::new(self.properties, self.strings);
        while tokens.offset < self.properties.len() {
            match tokens.next()? {
                Token::Property(key, data) if key == name => {
                    if !unique {
                        return Ok(Some(data));
                    }
                    if found.replace(data).is_some() {
                        return Err(Error::Property);
                    }
                }
                Token::Property(_, _) | Token::Nop => {}
                _ => return Err(Error::Property),
            }
        }
        Ok(found)
    }
    pub fn u32(&self, name: &str) -> Result<Option<u32>> {
        self.property(name)?
            .map(|raw| {
                if raw.len() == 4 {
                    word(raw, 0)
                } else {
                    Err(Error::Property)
                }
            })
            .transpose()
    }
    pub fn text(&self, name: &str) -> Result<Option<&'a str>> {
        self.property(name)?.map(string).transpose()
    }
    pub fn compatible(&self, name: &str) -> Result<bool> {
        let Some(raw) = self.property("compatible")? else {
            return Ok(false);
        };
        if raw.last() != Some(&0) {
            return Err(Error::String);
        }
        Ok(raw
            .split(|byte| *byte == 0)
            .any(|value| value == name.as_bytes()))
    }
    pub fn available(&self) -> Result<bool> {
        Ok(matches!(self.text("status")?, None | Some("ok" | "okay")))
    }
    pub fn reg(&self) -> Result<Option<(u64, u64)>> {
        let Some(raw) = self.property("reg")? else {
            return Ok(None);
        };
        // Wider (e.g. PCI) bus addresses remain traversable, but are not
        // misinterpreted as physical addresses by this minimal RAM/MMIO decoder.
        if self.parent.address > 2 || self.parent.size > 2 {
            return Err(Error::Cells);
        }
        let bytes = (self.parent.address + self.parent.size) * 4;
        if bytes == 0 || raw.is_empty() || raw.len() % bytes != 0 {
            return Err(Error::Cells);
        }
        let split = self.parent.address * 4;
        Ok(Some((
            cells(&raw[..split], self.parent.address)?,
            cells(&raw[split..bytes], self.parent.size)?,
        )))
    }
}

pub struct Nodes<'a> {
    fdt: Fdt<'a>,
    tokens: Tokens<'a>,
    depth: usize,
    stack: [Context; MAX_DEPTH],
    done: bool,
}
impl<'a> Nodes<'a> {
    pub(super) fn new(fdt: Fdt<'a>) -> Self {
        Self {
            fdt,
            tokens: Tokens::new(fdt.structure, fdt.strings),
            depth: 0,
            stack: [DEFAULT; MAX_DEPTH],
            done: false,
        }
    }
    fn node(&mut self, name: &'a str) -> Result<Node<'a>> {
        if self.depth >= MAX_DEPTH {
            return Err(Error::Depth);
        }
        let start = self.tokens.offset;
        let mut end = start;
        let mut peek = self.tokens.clone();
        while let Token::Property(_, _) | Token::Nop = peek.next()? {
            end = peek.offset;
        }
        let node = Node {
            name,
            depth: self.depth,
            properties: &self.fdt.structure[start..end],
            strings: self.fdt.strings,
            parent: if self.depth == 0 {
                DEFAULT
            } else {
                self.stack[self.depth - 1]
            },
        };
        let address = node
            .u32("#address-cells")?
            .unwrap_or(DEFAULT.address as u32) as usize;
        let size = node.u32("#size-cells")?.unwrap_or(DEFAULT.size as u32) as usize;
        self.stack[self.depth] = Context { address, size };
        self.depth += 1;
        Ok(node)
    }
}
impl<'a> Iterator for Nodes<'a> {
    type Item = Result<Node<'a>>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let result = match self.tokens.next() {
                Ok(Token::Begin(name)) => self.node(name),
                Ok(Token::EndNode) => {
                    self.depth = match self.depth.checked_sub(1) {
                        Some(depth) => depth,
                        None => {
                            self.done = true;
                            return Some(Err(Error::Token));
                        }
                    };
                    continue;
                }
                Ok(Token::Property(_, _) | Token::Nop) => continue,
                Ok(Token::End) => {
                    self.done = true;
                    return None;
                }
                Err(error) => Err(error),
            };
            if result.is_err() {
                self.done = true;
            }
            return Some(result);
        }
    }
}
