//! Drive console reads through the same decoder, service and encoder as guest RPC.
use super::*;

const FIRST: u64 = 1 << 36;
const DATA: [u8; 6] = [0, 0xff, 0x1b, 0x1d, 0x80, 0x7f];

pub(super) fn read_sample(cursor: u64) -> Result<ConsoleChunk, Error> {
    if cursor < FIRST {
        return Ok(ConsoleChunk::Overrun { resume: FIRST });
    }
    let offset = usize::try_from(cursor - FIRST).map_err(|_| Error::Invalid)?;
    let remaining = DATA.get(offset..).ok_or(Error::Invalid)?;
    let mut data = [0; 6];
    data[..remaining.len()].copy_from_slice(remaining);
    Ok(ConsoleChunk::Bytes {
        data,
        len: remaining.len() as u8,
    })
}

fn read(s: &mut Service, b: &mut Runtime, source: u32, epoch: u32, cursor: u64) -> abi::Return {
    let req = abi::decode([1, abi::CONSOLE_READ, 2, epoch.into(), cursor, 0]).unwrap();
    abi::encode_for(req.operation, s.execute(Caller::Guest(source), req, b))
}

#[test]
fn log_read_request_has_one_full_width_cursor_and_one_reserved_word() {
    assert_eq!(abi::CONSOLE_READ, 8);
    for cursor in [0, 1, FIRST, (1 << 48) + 42, u64::MAX] {
        assert_eq!(
            abi::decode([1, 8, u32::MAX.into(), u32::MAX.into(), cursor, 0]),
            Ok(request(u32::MAX, u32::MAX, Operation::ConsoleRead(cursor)))
        );
    }
    for bad in [
        [1, 8, 0, 1, 0, 0],
        [1, 8, 1 << 32, 1, 0, 0],
        [1, 8, 1, 0, 0, 0],
        [1, 8, 1, (1 << 32) + 1, 0, 0],
        [1, 8, 1, 1, 0, 1],
        [1, 8, 1, 1, 0, 1 << 63],
    ] {
        assert_eq!(abi::decode(bad), Err(abi::INVALID_PARAM));
    }
    assert_eq!(abi::decode([2, 8, 2, 11, 0, 0]), Err(abi::NOT_SUPPORTED));
}

#[test]
fn log_read_packs_binary_eof_and_exact_overrun_without_exposing_padding() {
    let mut b = Runtime::new();
    let mut s = Service::new();
    s.grant(Caller::Host, 10, VmId(2), Rights::LOG, &b).unwrap();
    assert_eq!(
        read(&mut s, &mut b, 10, 11, 0),
        abi::Return {
            error: 0,
            value: (1 << 51) | FIRST
        }
    );
    for offset in 0..=6 {
        let reply = read(&mut s, &mut b, 10, 11, FIRST + offset as u64);
        let mut words = [0u8; 8];
        let bytes = &DATA[offset..];
        words[..bytes.len()].copy_from_slice(bytes);
        words[6] = bytes.len() as u8;
        assert_eq!(
            reply,
            abi::Return {
                error: 0,
                value: u64::from_le_bytes(words)
            }
        );
        assert_eq!(b.log_read.get(), Some((VmId(2), 11, FIRST + offset as u64)));
    }
    assert_eq!(
        read(&mut s, &mut b, 10, 11, u64::MAX),
        abi::Return::error(abi::INVALID_PARAM)
    );
    let info = b.info(VmId(2)).unwrap();
    let encode = |chunk| {
        abi::encode_for(
            Operation::ConsoleRead(0),
            Ok(Reply {
                info,
                changed: false,
                log: Some(chunk),
                mail: None,
            }),
        )
    };
    let limit = 1 << 48;
    assert_eq!(
        encode(ConsoleChunk::Overrun { resume: limit - 1 }).value,
        (1 << 51) | (limit - 1)
    );
    for resume in [limit, limit + 1, u64::MAX] {
        assert_eq!(
            encode(ConsoleChunk::Overrun { resume }),
            abi::Return::error(abi::INVALID_PARAM)
        );
    }
    assert_eq!(
        encode(ConsoleChunk::Bytes {
            data: [0xff; 6],
            len: 1
        })
        .value,
        1 << 48 | 255
    );
    assert_eq!(
        encode(ConsoleChunk::Bytes {
            data: [0xff; 6],
            len: 7
        }),
        abi::Return::error(abi::INVALID_PARAM)
    );
}

#[test]
fn log_grant_is_independent_of_inspection_input_and_lifecycle() {
    assert_eq!(Rights::LOG.0, 128);
    assert_eq!(Rights::ALL.0, 15);
    assert_eq!(
        Rights::parse("inspect,log"),
        Some(Rights::INSPECT.union(Rights::LOG))
    );
    let mut b = Runtime::new();
    let mut s = Service::new();
    for right in [Rights::ALL, Rights::INSPECT, Rights::CONSOLE] {
        s.grant(Caller::Host, 10, VmId(2), right, &b).unwrap();
        b.reads.set(0);
        assert_eq!(
            read(&mut s, &mut b, 10, 11, FIRST),
            abi::Return::error(abi::DENIED)
        );
        assert_eq!(b.reads.get(), 0);
        assert_eq!(b.log_read.get(), None);
    }
    s.grant(Caller::Host, 10, VmId(2), Rights::LOG, &b).unwrap();
    assert_eq!(s.list(Caller::Guest(10), &b).count(), 0);
    for operation in [
        Operation::Inspect,
        Operation::MemoryBytes,
        Operation::Console,
        Operation::Input(0),
        Operation::Stop,
        Operation::Start,
    ] {
        assert_eq!(
            s.execute(Caller::Guest(10), request(2, 11, operation), &mut b),
            Err(Error::Denied)
        );
    }
    for state in [
        State::Ready,
        State::Running,
        State::Waiting,
        State::Stopping,
        State::Stopped,
        State::Faulted,
    ] {
        let target = b.row(VmId(2));
        target.state = state;
        target.active = matches!(state, State::Ready | State::Running | State::Waiting);
        assert_eq!(read(&mut s, &mut b, 10, 11, FIRST).error, 0);
    }
}

#[test]
fn both_epochs_authorize_a_read_even_before_retirement_notifications() {
    let mut b = Runtime::new();
    let mut s = Service::new();
    let rights = Rights::LOG.union(Rights::INSPECT);
    s.grant(Caller::Host, 10, VmId(2), rights, &b).unwrap();
    assert_eq!(
        read(&mut s, &mut b, 10, 13, 0),
        abi::Return::error(abi::INVALID_STATE)
    );
    b.row(VmId(2)).epoch = 20;
    assert_eq!(
        read(&mut s, &mut b, 10, 11, 0),
        abi::Return::error(abi::INVALID_STATE)
    );
    // Knowing or guessing the new identity cannot transfer the old log grant.
    for guessed in [19, 20, 21] {
        assert_eq!(
            read(&mut s, &mut b, 10, guessed, 0),
            abi::Return::error(abi::DENIED)
        );
    }
    assert_eq!(b.log_read.get(), None);
    let inspect = s
        .execute(Caller::Guest(10), request(2, 0, Operation::Inspect), &mut b)
        .unwrap();
    assert_eq!(inspect.info.epoch, 20);
    s.grant(Caller::Host, 10, VmId(2), rights, &b).unwrap();
    assert_eq!(read(&mut s, &mut b, 10, 20, FIRST).error, 0);
    b.log_read.set(None);
    b.row(VmId(1)).active = false;
    assert_eq!(
        read(&mut s, &mut b, 10, 20, 0),
        abi::Return::error(abi::DENIED)
    );
    let manager = b.row(VmId(1));
    manager.epoch = 30;
    manager.active = true;
    for source in [10, 30] {
        assert_eq!(
            read(&mut s, &mut b, source, 20, 0),
            abi::Return::error(abi::DENIED)
        );
    }
    assert_eq!(b.log_read.get(), None);
    s.grant(Caller::Host, 30, VmId(2), rights, &b).unwrap();
    s.revoke(Caller::Host, 30, VmId(2), &b).unwrap();
    assert_eq!(
        read(&mut s, &mut b, 30, 20, 0),
        abi::Return::error(abi::DENIED)
    );
}
