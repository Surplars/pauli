//! Serial console provenance, independent of CPU, device model and transport.
//! Most bytes pass unchanged. Source switches, literal RS and LF are framed.
//! No encoded frame or payload contains LF: firmware console writers may turn
//! raw LF into CRLF. Escaping it here preserves guest CRLF and binary bytes.
//! Wire: RS H = host; RS G <8 hex digits> ; = guest; RS RS = literal RS.
//! RS N = literal LF. Hex IDs also avoid newline translation in frame headers.
//! RS A <8 hex digits> ; attaches the terminal; RS D returns it to the host.
//! RS B <8 hex digits> ; begins a fresh VM epoch log, independent of attach.
//! Unframed firmware boot output starts in the host channel.
//! Ownership/Lock Ordering: one stream owner per codec; transport serializes writers.
//! Invariants: framing cannot be forged by unescaped payload RS or LF bytes.
//! Decoder: Data -> Escape -> [Id -> EndId] -> Data; malformed input returns Err.
const RS: u8 = 0x1e;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Host,
    Guest(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Byte(Source, u8),
    Attach(u32),
    Begin(u32),
    Detach,
}

pub struct Encoder {
    source: Source,
}
impl Encoder {
    pub const fn new() -> Self {
        Self {
            source: Source::Host,
        }
    }
    fn id_frame(command: u8, id: u32, put: &mut impl FnMut(u8)) {
        put(RS);
        put(command);
        for shift in (0..8).rev() {
            put(b"0123456789abcdef"[((id >> (shift * 4)) & 15) as usize]);
        }
        put(b';');
    }
    pub fn attach(&mut self, id: u32, mut put: impl FnMut(u8)) {
        Self::id_frame(b'A', id, &mut put);
    }
    pub fn begin(&mut self, id: u32, mut put: impl FnMut(u8)) {
        Self::id_frame(b'B', id, &mut put);
    }
    pub fn detach(&mut self, mut put: impl FnMut(u8)) {
        put(RS);
        put(b'D');
    }
    /// The owner serializes this operation with other console writers.
    pub fn write(&mut self, source: Source, byte: u8, mut put: impl FnMut(u8)) {
        if source != self.source {
            match source {
                Source::Host => {
                    put(RS);
                    put(b'H');
                }
                Source::Guest(id) => Self::id_frame(b'G', id, &mut put),
            }
            self.source = source;
        }
        match byte {
            RS => {
                put(RS);
                put(RS); // payload can never forge a source switch
            }
            b'\n' => {
                put(RS);
                put(b'N');
            }
            _ => put(byte),
        }
    }
}

#[derive(Clone, Copy)]
enum IdKind {
    Source,
    Attach,
    Begin,
}
#[derive(Clone, Copy)]
enum State {
    Data,
    Escape,
    Id {
        value: u32,
        digits: u8,
        kind: IdKind,
    },
    EndId(u32, IdKind),
}
#[derive(Debug)]
pub struct InvalidFrame;
pub struct Decoder {
    source: Source,
    state: State,
}
impl Decoder {
    pub const fn new() -> Self {
        Self {
            source: Source::Host,
            state: State::Data,
        }
    }
    /// One-byte state machine naturally supports arbitrary transport chunks.
    pub fn push(&mut self, byte: u8) -> Result<Option<Event>, InvalidFrame> {
        match self.state {
            State::Data => {
                if byte == RS {
                    self.state = State::Escape;
                } else {
                    return Ok(Some(Event::Byte(self.source, byte)));
                }
            }
            State::Escape => match byte {
                RS => {
                    self.state = State::Data;
                    return Ok(Some(Event::Byte(self.source, RS)));
                }
                b'N' => {
                    self.state = State::Data;
                    return Ok(Some(Event::Byte(self.source, b'\n')));
                }
                b'H' => {
                    self.source = Source::Host;
                    self.state = State::Data;
                }
                b'G' | b'A' | b'B' => {
                    self.state = State::Id {
                        value: 0,
                        digits: 0,
                        kind: match byte {
                            b'A' => IdKind::Attach,
                            b'B' => IdKind::Begin,
                            _ => IdKind::Source,
                        },
                    }
                }
                b'D' => {
                    self.state = State::Data;
                    return Ok(Some(Event::Detach));
                }
                _ => return Err(InvalidFrame),
            },
            State::Id {
                value,
                digits,
                kind,
            } => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(InvalidFrame),
                };
                let value = (value << 4) | u32::from(digit);
                self.state = if digits == 7 {
                    State::EndId(value, kind)
                } else {
                    State::Id {
                        value,
                        digits: digits + 1,
                        kind,
                    }
                };
            }
            State::EndId(id, kind) => {
                if byte != b';' {
                    return Err(InvalidFrame);
                }
                self.state = State::Data;
                match kind {
                    IdKind::Attach => return Ok(Some(Event::Attach(id))),
                    IdKind::Begin => return Ok(Some(Event::Begin(id))),
                    IdKind::Source => self.source = Source::Guest(id),
                }
            }
        }
        Ok(None)
    }
    pub fn complete(&self) -> bool {
        matches!(self.state, State::Data)
    }
}
