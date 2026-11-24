//! Nonblocking host line editor and guest-console attachment transport.
//! Ownership/Lock Ordering: sole manager owns framing; all physical output is unlocked.
//! Invariants: detach never changes VM lifecycle; attachment binds an exact epoch.
use core::fmt::{self, Write};
use pauli_devices::console::{Encoder, Source};
pub const DETACH: u8 = 0x1d;
pub struct Terminal {
    pub attached: Option<(usize, u32)>,
    pub output: Output,
    line: [u8; 160],
    len: usize,
    overflow: bool,
}
impl Terminal {
    pub const fn new() -> Self {
        Self {
            attached: None,
            output: Output::Framed(Encoder::new()),
            line: [0; 160],
            len: 0,
            overflow: false,
        }
    }
    pub fn detach(&mut self) {
        self.attached = None;
        if let Output::Framed(encoder) = &mut self.output {
            encoder.detach(put);
        }
        self.output
            .host(format_args!("\nDetached; VM keeps running\nhv> "));
    }
    pub fn attach(&mut self, slot: usize, epoch: u32) {
        self.attached = Some((slot, epoch));
        self.output.host(format_args!(
            "Attached VM {} epoch={}; Ctrl+] detaches\n",
            slot + 1,
            epoch
        ));
        if let Output::Framed(encoder) = &mut self.output {
            encoder.attach(epoch, put);
        }
    }
    pub fn push(&mut self, byte: u8) -> bool {
        match byte {
            b'\r' | b'\n' => {
                self.output.host(format_args!("\n"));
                true
            }
            8 | 127 => {
                if self.len != 0 {
                    self.len -= 1;
                    self.output.host(format_args!("\x08 \x08"));
                }
                false
            }
            0x20..=0x7e => {
                if self.len < self.line.len() {
                    self.line[self.len] = byte;
                    self.len += 1;
                    self.output.host(format_args!("{}", byte as char));
                } else {
                    self.overflow = true;
                }
                false
            }
            _ => false,
        }
    }
    pub fn line(&self, output: &mut [u8; 160]) -> Option<usize> {
        if self.overflow {
            return None;
        }
        output[..self.len].copy_from_slice(&self.line[..self.len]);
        Some(self.len)
    }
    pub fn reset_line(&mut self) {
        self.len = 0;
        self.overflow = false;
    }
}
fn put(byte: u8) {
    let _ = pauli_hal::log::write_bytes(&[byte]);
}
pub enum Output {
    Framed(Encoder),
    /// Slots other than the boot Linux never share its physical terminal.
    Primary,
}
impl Output {
    pub(super) fn direct(&self) -> bool {
        matches!(self, Self::Primary)
    }
    pub fn host(&mut self, args: fmt::Arguments<'_>) {
        let _ = self.write_fmt(args);
    }
    pub fn guest(&mut self, slot: usize, epoch: u32, bytes: &[u8]) {
        match self {
            Self::Primary if slot == 0 => {
                // The guest's serial driver owns CR/LF and binary payloads.
                let _ = pauli_hal::log::write_bytes(bytes);
            }
            Self::Primary => {}
            Self::Framed(encoder) => {
                for &byte in bytes {
                    encoder.write(Source::Guest(epoch), byte, put);
                }
            }
        }
    }
    pub fn begin(&mut self, epoch: u32) {
        if let Self::Framed(encoder) = self {
            encoder.begin(epoch, put);
        }
    }
}
impl Write for Output {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        match self {
            Self::Framed(encoder) => {
                for byte in text.bytes() {
                    encoder.write(Source::Host, byte, put);
                }
            }
            Self::Primary => {
                // Only host diagnostics get serial CRLF; never rewrite guest bytes.
                for line in text.split_inclusive('\n') {
                    if let Some(body) = line.strip_suffix('\n') {
                        pauli_hal::log::write_bytes(body.as_bytes())?;
                        pauli_hal::log::write_bytes(b"\r\n")?;
                    } else {
                        pauli_hal::log::write_bytes(line.as_bytes())?;
                    }
                }
            }
        }
        Ok(())
    }
}
