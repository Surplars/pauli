//! console protocol and isolation checks.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

//! Host-only console codec acceptance, including OpenSBI-style LF expansion.
//! Run with an explicit host target; no guest, disk, network or QEMU required.
use super::*;
use crate::console::{Decoder, Encoder, Event, Source};

fn firmware_write(byte: u8, wire: &mut Vec<u8>) {
    if byte == b'\n' {
        wire.push(b'\r');
    }
    wire.push(byte);
}

#[test]
fn acceptance() {
    let mut wire = Vec::new();
    let mut expected = Vec::new();
    // Firmware writes before the source encoder is installed remain unframed
    // host data. Decoding must not "repair" or reinterpret their line endings.
    for byte in b"OpenSBI\n" {
        firmware_write(*byte, &mut wire);
    }
    expected.extend(
        b"OpenSBI\r\n"
            .iter()
            .map(|byte| Event::Byte(Source::Host, *byte)),
    );
    let framed_start = wire.len();
    let mut encoder = Encoder::new();
    encoder.begin(0x0a00_1e0a, |byte| firmware_write(byte, &mut wire));
    expected.push(Event::Begin(0x0a00_1e0a));
    for source in [
        Source::Host,
        Source::Guest(0x0a00_1e0a),
        Source::Guest(u32::MAX),
        Source::Host,
    ] {
        for byte in 0..=u8::MAX {
            encoder.write(source, byte, |byte| firmware_write(byte, &mut wire));
            expected.push(Event::Byte(source, byte));
        }
    }
    encoder.attach(0x0a00_1e0a, |byte| firmware_write(byte, &mut wire));
    expected.push(Event::Attach(0x0a00_1e0a));
    // Guest strings resembling source/control/newline frames stay guest bytes.
    for byte in b"\x1eH\x1eA00000000;\x1eD\x1eN\r\n\n" {
        encoder.write(Source::Guest(7), *byte, |byte| {
            firmware_write(byte, &mut wire)
        });
        expected.push(Event::Byte(Source::Guest(7), *byte));
    }
    encoder.detach(|byte| firmware_write(byte, &mut wire));
    expected.push(Event::Detach);
    assert!(!wire[framed_start..].contains(&b'\n'));

    // Byte-at-a-time decode covers every possible transport frame boundary.
    let mut decoder = Decoder::new();
    let actual: Vec<_> = wire
        .into_iter()
        .filter_map(|byte| decoder.push(byte).unwrap())
        .collect();
    assert!(decoder.complete());
    assert_eq!(actual, expected);

    let mut partial = Decoder::new();
    assert!(partial.push(0x1e).unwrap().is_none());
    assert!(!partial.complete());
    assert_eq!(
        partial.push(b'N').unwrap(),
        Some(Event::Byte(Source::Host, b'\n'))
    );
    assert!(partial.complete());
    assert!(partial.push(0x1e).unwrap().is_none());
    assert!(partial.push(b'X').is_err());
    println!(
        "Console codec PASS: all 256 bytes, firmware LF expansion, sources/control/escaping and fragmented frames"
    );
}
