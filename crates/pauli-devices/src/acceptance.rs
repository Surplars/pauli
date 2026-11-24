//! Deterministic model vectors shared by the legacy board harness and unit tests.
//! Ownership/Lock Ordering: local deterministic fixtures; no locks or real devices.
//! Invariants: assertions concern trusted test vectors, never live guest input.
//! Legacy harness API stays until the runtime/selftest migration in Phase 3.
use crate::uart16550::Uart16550;

pub fn console() {
    use crate::console::{Decoder, Encoder, Event, Source};
    let mut encoder = Encoder::new();
    let mut wire = [0u8; 128];
    let mut len = 0;
    let records = [
        (Source::Host, &b"host\n"[..]),
        (
            Source::Guest(7),
            &b"guest\x1eH\x1eG00000000;\x1eD\x1eA00000007;\n"[..],
        ),
        (Source::Guest(0x1e0a1b0d), &b"other\n"[..]),
        (Source::Host, &b"done"[..]),
    ];
    for (source, bytes) in records {
        for &byte in bytes {
            encoder.write(source, byte, |byte| {
                wire[len] = byte;
                len += 1;
            });
        }
    }
    encoder.begin(7, |byte| {
        wire[len] = byte;
        len += 1;
    });
    encoder.attach(7, |byte| {
        wire[len] = byte;
        len += 1;
    });
    encoder.detach(|byte| {
        wire[len] = byte;
        len += 1;
    });
    let mut decoder = Decoder::new();
    let mut cursor = 0;
    for (source, bytes) in records {
        for &expected in bytes {
            loop {
                let decoded = decoder.push(wire[cursor]).unwrap();
                cursor += 1;
                if let Some(Event::Byte(actual_source, byte)) = decoded {
                    assert_eq!(actual_source, source);
                    assert_eq!(byte, expected);
                    break;
                }
            }
        }
    }
    for expected in [Event::Begin(7), Event::Attach(7), Event::Detach] {
        loop {
            let event = decoder.push(wire[cursor]).unwrap();
            cursor += 1;
            if let Some(event) = event {
                assert_eq!(event, expected);
                break;
            }
        }
    }
    assert_eq!(cursor, len);
    assert!(decoder.complete());
    let mut invalid = Decoder::new();
    invalid.push(0x1e).unwrap();
    assert!(!invalid.complete());
    assert!(invalid.push(b'?').is_err());
}
pub fn uart() {
    let mut a = Uart16550::new(10);
    let b = Uart16550::new(10);
    assert_eq!(a.write(3, 0x80), Some(None));
    assert_eq!(a.write(0, 2), Some(None));
    assert_eq!(a.read(0), Some(2));
    a.write(3, 3);
    a.write(2, 0xc1);
    a.write(1, 3);
    assert_eq!(a.read(2), Some(0xc2));
    assert!(!a.irq_pending());
    assert!(a.receive(b'x'));
    assert!(!a.irq_pending());
    a.advance(10);
    assert_eq!(a.read(2), Some(0xcc));
    assert!(a.irq_pending());
    assert_eq!(a.read(0), Some(b'x'));
    assert!(!a.irq_pending());
    assert!(!b.irq_pending()); // no cross-VM state
    for byte in 0..16 {
        assert!(a.receive(byte));
    }
    assert!(!a.receive(16));
    assert_eq!(a.overruns, 1);
    a.write(1, 7);
    assert_eq!(a.read(2), Some(0xc6));
    assert_eq!(a.read(5), Some(0x63));
    assert_eq!(a.read(2), Some(0xc4));
    for byte in 0..16 {
        assert_eq!(a.read(0), Some(byte));
    }
    a.write(2, 7);
    a.write(1, 0);
    assert!(!a.irq_pending());
    a.write(4, 0x1b);
    assert_eq!(a.read(6).unwrap() & 0xf0, 0xb0);
    assert_eq!(a.write(0, 0x55), Some(None));
    assert_eq!(a.read(0), Some(0x55));
    a.write(4, 0);
    assert_eq!(a.write(0, b'z'), Some(Some(b'z')));
    assert_eq!(a.read(8), None);
    assert_eq!(a.write(8, 0), None);
}
