//! Host-console tests for the read-only remote-fence report.
//! A thread-local receiver captures only this test's monitor output. Samples
//! are seeded ISA observations; generation sampling is tested by the backend.

#[test]
fn fence_command_has_exact_selftest_only_grammar() {
    use crate::monitor::commands::parse;
    #[cfg(feature = "selftest")]
    assert_eq!(
        parse("vm fences alpha"),
        Ok(crate::monitor::commands::Command::Fences("alpha"))
    );
    #[cfg(not(feature = "selftest"))]
    assert!(parse("vm fences alpha").is_err());
    for bad in ["vm fences", "vm fences alpha extra"] {
        assert!(parse(bad).is_err());
    }
}

#[cfg(feature = "selftest")]
mod joined {
    use super::super::model::{Fixture, OWNER};
    use super::super::uart_capture::capture;
    use crate::{monitor::Monitor, runtime::Dispatcher};
    use std::{string::String, vec::Vec};
    fn command(f: &Fixture, monitor: &mut Monitor, bytes: &[u8]) -> String {
        use pauli_devices::console::{Decoder, Event, Source};
        f.runtime.board.input.lock().extend(bytes);
        let bytes = capture(|| monitor.poll(&f.runtime));
        let mut wire = Decoder::new();
        let mut host = Vec::new();
        for byte in bytes {
            if let Some(Event::Byte(src, b)) = wire.push(byte).unwrap() {
                assert_eq!(src, Source::Host);
                host.push(b);
            }
        }
        assert!(wire.complete());
        String::from_utf8(host).unwrap()
    }

    #[test]
    fn reports_only_the_selected_joined_epoch() {
        let f = Fixture::new(2, 2);
        let rt = &f.runtime;
        let mut monitor = Monitor::new();
        let alpha = rt.manager.vm(0).unwrap();
        let beta = rt.manager.vm(1).unwrap();
        rt.manager.start(rt.board, 0).unwrap();
        rt.manager.start(rt.board, 1).unwrap();
        let slot = alpha.cpu(1).unwrap();
        slot.mailbox.lock().observed = Some((false, 5, 0, 101));
        beta.cpu(0).unwrap().mailbox.lock().observed = Some((true, 5, 1, 202));
        let query = b"vm fences alpha\n";
        let busy = command(&f, &mut monitor, query);
        assert!(busy.contains("vm: Busy\n") && !busy.contains("fence epoch="));

        let owner = alpha.cpu(0).unwrap().owner;
        OWNER.with(|hart| hart.set(owner));
        let mut cpu = Dispatcher::new();
        rt.dispatch(&mut cpu).unwrap();
        assert!(!alpha.cpu(0).unwrap().retired());
        alpha.host_stop();
        beta.host_stop();
        OWNER.with(|hart| hart.set(0));
        let stopping = command(&f, &mut monitor, query);
        assert!(stopping.contains("vm: Busy\n") && !stopping.contains("fence epoch="));
        OWNER.with(|hart| hart.set(owner));
        rt.dispatch(&mut cpu).unwrap();
        OWNER.with(|hart| hart.set(0));
        let text = command(&f, &mut monitor, query);
        let row = std::format!(
            "fence epoch={} target=1 owner={} requester=0 op=vma gen=5 ticks=101 path=unobserved",
            alpha.epoch(),
            slot.owner
        );
        assert_eq!(
            text.lines()
                .filter(|s| s.starts_with("fence epoch="))
                .collect::<Vec<_>>(),
            [row.as_str()]
        );
        let peer = command(&f, &mut monitor, b"vm fences beta\n");
        assert!(peer.contains("requester=1 op=fencei gen=5 ticks=202 path=unobserved\n"));
        let first = alpha.epoch();
        rt.manager.start(rt.board, 0).unwrap();
        assert_ne!(alpha.epoch(), first);
        alpha.host_stop();
        let fresh = command(&f, &mut monitor, query);
        assert!(
            fresh.contains("hv> ") && !fresh.contains("fence epoch=") && !fresh.contains("vm: ")
        );
        assert!(
            command(&f, &mut monitor, b"vm fences beta\n")
                .contains("requester=1 op=fencei gen=5 ticks=202 path=unobserved\n")
        );
    }
}
