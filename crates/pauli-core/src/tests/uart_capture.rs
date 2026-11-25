//! Capture only the calling test's physical wire; parallel monitors never share bytes.
use std::{cell::RefCell, sync::Once, vec::Vec};
std::thread_local! {
    static UART: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}
struct Capture;
impl pauli_hal::log::Console for Capture {
    fn write(&self, bytes: &[u8]) -> core::fmt::Result {
        UART.with_borrow_mut(|out| {
            if let Some(out) = out {
                out.extend_from_slice(bytes);
            }
        });
        Ok(())
    }
}
pub(super) fn capture(body: impl FnOnce()) -> Vec<u8> {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| assert!(pauli_hal::log::install(&Capture).is_ok()));
    UART.set(Some(Vec::new()));
    body();
    UART.take().unwrap()
}

pub(super) fn host_output(body: impl FnOnce()) -> std::string::String {
    use pauli_devices::console::{Decoder, Event, Source};
    let mut wire = Decoder::new();
    let mut text = Vec::new();
    for byte in capture(body) {
        if let Some(Event::Byte(Source::Host, byte)) = wire.push(byte).unwrap() {
            text.push(byte);
        }
    }
    assert!(wire.complete());
    std::string::String::from_utf8(text).unwrap()
}
