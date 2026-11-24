//! Reproduce a piped UART that silently drops writes while its RX slot is full.
//! Ownership/Lock Ordering: the controller and wire run on one test thread.
//! Invariants: only a fresh host echo advances input; a guest reply is epoch-bound.
use super::*;
use std::collections::VecDeque;

struct OneByteUart {
    pending: Option<u8>,
    input: Vec<u8>,
    output: VecDeque<u8>,
    reply: fn(&[u8]) -> Vec<u8>,
}
impl Serial for OneByteUart {
    fn send(&mut self, bytes: &[u8]) -> Result<()> {
        if self.pending.is_none() {
            self.pending = bytes.first().copied();
        }
        Ok(()) // the pipe accepts even the bytes that never reach the UART
    }
    fn receive(&mut self, _: Duration) -> Result<Output> {
        if self.output.is_empty()
            && let Some(key) = self.pending.take()
        {
            self.input.push(key);
            self.output.extend((self.reply)(&self.input));
        }
        Ok(match self.output.pop_front() {
            Some(byte) => Output::Data(vec![byte]), // split even attach/detach frames
            None => Output::Idle,
        })
    }
}
fn controller(reply: fn(&[u8]) -> Vec<u8>) -> Controller<OneByteUart> {
    let uart = OneByteUart {
        pending: None,
        input: Vec::new(),
        output: VecDeque::new(),
        reply,
    };
    Controller::new(uart, Duration::from_secs(5))
}
fn monitor(input: &[u8]) -> Vec<u8> {
    if input.ends_with(b"vm status alpha\n") {
        return b"\x1eNVM id=1 epoch=1 name=alpha state=Ready vcpus=2\x1eNhv> ".to_vec();
    }
    if input.ends_with(b"vm attach alpha\n") {
        return b"\x1eN\x1eA00000001;".to_vec();
    }
    if input.ends_with(b"\nq") {
        return b"\x1eG00000001;[probe] Poweroff\x1eN\x1eHVM 1 epoch=1 Stopped\x1eN\x1eDhv> "
            .to_vec();
    }
    if input.ends_with(b"shutdown\n") {
        return b"shutdown accepted".to_vec();
    }
    vec![*input.last().unwrap()]
}

#[test]
fn bulk_write_reproduces_first_letter_only() {
    let mut c = controller(monitor);
    c.process.send(b"vm status alpha\n").unwrap();
    assert!(c.wait(HOST, 0, PROMPT).is_err());
    assert_eq!(c.log(HOST), b"v");
}
#[test]
fn one_rx_slot_carries_status_attach_guest_poweroff_and_shutdown() -> Result<()> {
    let mut c = controller(monitor);
    let epoch = c.status("alpha", "Ready")?;
    c.attach("alpha", epoch)?;
    c.guest(epoch, b'q', "[probe] Poweroff")?;
    c.terminal(1, epoch, "Stopped")?;
    c.shutdown()?;
    c.wait(HOST, 0, "shutdown accepted")?;
    assert_eq!(
        c.process.input,
        b"vm status alpha\nvm attach alpha\nqshutdown\n"
    );
    Ok(())
}
#[test]
fn stale_host_and_live_guest_echo_do_not_release_the_next_key() {
    let mut c = controller(|_| b"\x1eG00000001;vm status alpha\x1eNhv> ".to_vec());
    c.logs.insert(HOST, b"vm status alpha\nhv> ".to_vec());
    let err = c.command("vm status alpha").unwrap_err();
    assert!(format!("{err:#}").contains("echo 1/15 ('v')"));
    assert_eq!(c.process.input, b"v");
}
#[test]
fn echoing_the_whole_line_is_not_command_completion() {
    let mut c = controller(|input| match input.last() {
        Some(b'\n') => b"\x1eG00000001;hv> ".to_vec(),
        _ => monitor(input),
    });
    c.logs.insert(HOST, PROMPT.as_bytes().to_vec());
    let err = c.command("vm status alpha").unwrap_err();
    assert!(format!("{err:#}").contains("did not return to the prompt"));
    assert_eq!(c.process.input, b"vm status alpha\n");
}

#[test]
fn destroy_retries_only_a_fresh_host_busy_reply() -> Result<()> {
    let mut c = controller(|input| {
        if input.ends_with(b"vm destroy child\n") {
            return if input.iter().filter(|&&b| b == b'\n').count() == 1 {
                b"\x1eNvm: Busy\x1eNhv> ".to_vec()
            } else {
                b"\x1eNVM 2 destroyed\x1eNhv> ".to_vec()
            };
        }
        monitor(input)
    });
    c.destroy(2, "child")?;
    assert_eq!(c.process.input, b"vm destroy child\nvm destroy child\n");
    let mut c = controller(|input| {
        if input.ends_with(b"\n") {
            b"\x1eNvm: Permission\x1eNhv> ".to_vec()
        } else {
            monitor(input)
        }
    });
    assert!(c.destroy(2, "child").is_err());
    assert_eq!(c.process.input, b"vm destroy child\n");
    Ok(())
}

#[test]
fn split_failure_markers_are_scoped_to_their_source_and_epoch() -> Result<()> {
    // The suffix from a different guest cannot finish this guest's failure marker.
    let mut c = controller(monitor);
    c.ingest(b"\x1eG00000001;[probe] FA\x1eG00000002;IL")?;
    c.ingest(b"[HV] ERROR\x1eH[probe] FAIL")?;
    let error = c
        .ingest(b"\x1eG00000001;IL\x1eHmore host bytes")
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Linux smoke fixture reported a failure")
    );
    let mut c = controller(monitor);
    c.ingest(b"\x1eH[irq] FAIL\n\x1eG00000001;[irq] FA\x1eG00000002;IL\n")?;
    c.ingest(b"\x1eG00000001;IL")?;
    assert!(
        c.ingest(b"\n")
            .unwrap_err()
            .to_string()
            .contains("IRQ firmware")
    );
    let mut c = controller(monitor);
    c.ingest(b"\x1eH[vt] FAIL\n\x1eG00000001;[vt] FA\x1eG00000002;IL\n")?;
    c.ingest(b"\x1eG00000001;IL")?;
    assert!(
        c.ingest(b"\n")
            .unwrap_err()
            .to_string()
            .contains("VS timer")
    );
    // Exercise every split, including the last byte arriving after an unrelated frame.
    for marker in [
        b"[HV] ERROR".as_slice(),
        b"provisioning failed",
        b"I/O join error=",
    ] {
        for split in 1..marker.len() {
            let mut c = controller(monitor);
            c.ingest(&marker[..split])?;
            c.ingest(b"\x1eG00000001;not host\x1eH")?;
            assert!(
                c.ingest(&marker[split..])
                    .unwrap_err()
                    .to_string()
                    .contains("hypervisor")
            );
        }
    }
    Ok(())
}

#[test]
fn fresh_marker_keeps_only_the_necessary_overlap() {
    let tag = b"[missing]";
    let mut bytes = vec![b'x'; 4096];
    let (mut old_bytes, mut new_bytes) = (0usize, 0usize);
    for seen in 0..bytes.len() {
        let data = &bytes[..=seen]; // Worst-case one-byte reads, no marker present.
        old_bytes += data.len();
        new_bytes += scan_window(data, 0, seen, tag.len()).len();
    }
    assert_eq!((old_bytes, new_bytes), (8_390_656, 36_828));
    assert!(scan_window(&bytes, 0, bytes.len(), tag.len()).is_empty());
    bytes[12..12 + tag.len()].copy_from_slice(tag);
    for seen in 12..12 + tag.len() {
        let data = &bytes[..12 + tag.len()];
        assert!(contains(scan_window(data, 12, seen, tag.len()), tag));
        assert!(!contains(
            scan_window(data, 13, seen.max(13), tag.len()),
            tag
        ));
    }
}
