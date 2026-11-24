//! Parse records from one fresh framed reply, never from another VM's output.
//! The controller owns the transcript; optional native ACKs must match the same input.
use super::{
    Mode,
    timeline::{Ack, AckPath},
};
use anyhow::{Context, Result, ensure};

pub(super) fn rise_ticks(reply: &[u8], epoch: u32, seq: u64, key: u8) -> Result<u64> {
    status_tick(
        reply,
        "uart-rise ",
        &format!("epoch={epoch} seq={seq} byte={key} ticks="),
    )
}
pub(super) fn owner_ticks(reply: &[u8], epoch: u32, seq: u64, owner: u8) -> Result<u64> {
    status_tick(
        reply,
        "uart-owner ",
        &format!("epoch={epoch} seq={seq} vcpu=0 owner={owner} ticks="),
    )
}
fn status_tick(reply: &[u8], tag: &str, expected: &str) -> Result<u64> {
    let mut rows = std::str::from_utf8(reply)?
        .lines()
        .filter_map(|r| r.strip_prefix(tag));
    let row = rows
        .next()
        .with_context(|| format!("missing {tag}record"))?;
    ensure!(rows.next().is_none(), "ambiguous {tag}record");
    Ok(row
        .strip_prefix(expected)
        .with_context(|| format!("unexpected {tag}record: {row}"))?
        .parse()?)
}
pub(super) fn trap_ticks(row: &str, seq: u64, mode: Mode) -> Result<u64> {
    let prefix = format!("[lat] sample seq={seq:016x} mode={} ticks=", mode.name());
    let hex = row
        .strip_prefix(&prefix)
        .context("VS IRQ sequence or interrupted loop mismatch")?;
    ensure!(
        hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid VS TIME record"
    );
    Ok(u64::from_str_radix(hex, 16)?)
}

pub(super) fn native_ack(reply: &[u8], epoch: u32, seq: u64, owner: u8) -> Result<Option<Ack>> {
    let mut rows = std::str::from_utf8(reply)?
        .lines()
        .filter_map(|r| r.strip_prefix("uart-ack "));
    let Some(row) = rows.next() else {
        return Ok(None);
    };
    ensure!(rows.next().is_none(), "ambiguous uart-ack record");
    // An absent observation is legitimate. A present record for a different
    // owner/epoch/input is not a missing observation: reject that transcript.
    let prefix = format!("epoch={epoch} seq={seq} vcpu=0 owner={owner} path=");
    let body = row
        .strip_prefix(&prefix)
        .context("unexpected uart-ack identity")?;
    let (path, times) = body.split_once(" send=").context("missing uart-ack send")?;
    let (send, ticks) = times
        .split_once(" ticks=")
        .context("missing uart-ack ticks")?;
    let (ticks, returned) = ticks
        .split_once(" returned=")
        .context("missing uart-ack sender return")?;
    let path = match path {
        "guest-exit" => AckPath::GuestExit,
        "dispatch-ack" => AckPath::DispatchAck,
        _ => anyhow::bail!("unknown physical SSIP ACK path: {path}"),
    };
    Ok(Some(Ack {
        path,
        send: send.parse()?,
        ticks: ticks.parse()?,
        returned: returned.parse()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_is_valid_but_stale_ambiguous_and_malformed_are_not() -> Result<()> {
        let parse = |raw: &str| native_ack(raw.as_bytes(), 13, 5, 0);
        assert!(parse("uart-owner epoch=13 seq=5 vcpu=0 owner=0 ticks=104\nhv> ")?.is_none());
        let row = "uart-ack epoch=13 seq=5 vcpu=0 owner=0 path=guest-exit send=101 ticks=103 returned=109\n";
        for (wire, path) in [
            ("guest-exit", AckPath::GuestExit),
            ("dispatch-ack", AckPath::DispatchAck),
        ] {
            let ack = parse(&row.replace("guest-exit", wire))?.unwrap();
            assert_eq!(
                (ack.path, ack.send, ack.ticks, ack.returned),
                (path, 101, 103, 109)
            );
        }
        for wrong in [
            row.replace("epoch=13", "epoch=12"),
            row.replace("seq=5", "seq=4"),
            row.replace("owner=0", "owner=1"),
            row.replace("vcpu=0", "vcpu=1"),
            row.replace("guest-exit", "wfi"),
            row.replace("ticks=103", "ticks=103 extra"),
            row.replace(" returned=109", ""),
            row.replace("returned=109", "returned=109 returned=110"),
            row.replace("send=101", "send=x"),
            row.repeat(2),
        ] {
            assert!(parse(&wrong).is_err(), "{wrong}");
        }
        Ok(())
    }
}
