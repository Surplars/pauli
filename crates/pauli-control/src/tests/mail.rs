//! Cross the same decode, grant and reply boundaries as a guest mailbox call.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Exchange {
    pub(super) local: (VmId, u32),
    pub(super) peer: (VmId, u32),
    pub(super) operation: Operation,
}

const SAMPLE: [u8; 6] = [0x00, 0xff, 0x80, 0x2e, 0x00, 0xe1];
const EMPTY: MailMessage = MailMessage {
    tag: 0,
    len: 0,
    data: [0; 6],
};

#[test]
fn wire_round_trip_preserves_binary_messages_and_distinguishes_empty_from_present() {
    assert_eq!((abi::MAIL_SEND, abi::MAIL_RECV), (9, 10));
    let mut service = Service::new();
    let mut backend = Runtime::new();
    service
        .grant(Caller::Host, 10, VmId(2), Rights::MAIL, &backend)
        .unwrap();
    let mut call = |words, response| {
        backend.mail_result.set(Ok(response));
        let req = abi::decode(words).unwrap();
        let reply = service.execute(Caller::Guest(10), req, &mut backend);
        assert_eq!(
            backend.mail_call.take(),
            Some(Exchange {
                local: (VmId(1), 10),
                peer: (VmId(2), 11),
                operation: req.operation,
            })
        );
        abi::encode_for(req.operation, reply)
    };
    let inbox_empty = MailChunk {
        message: None,
        more: false,
    };
    assert_eq!(
        call([1, 10, 2, 0, 0, 0], inbox_empty),
        abi::Return { error: 0, value: 0 }
    );
    for len in 0..=6u8 {
        let mut data = SAMPLE;
        data[usize::from(len)..].fill(0);
        let mut bytes = [0u8; 8];
        bytes[..6].copy_from_slice(&data);
        let payload = u64::from_le_bytes(bytes);
        for tag in [0u8, 1, 128, 255] {
            let message = MailMessage { tag, len, data };
            let metadata = (u64::from(tag) << 40) | (u64::from(len) << 32);
            for epoch in [0, 11] {
                let words = [1, 9, 2, metadata | u64::from(epoch), payload, 0];
                assert_eq!(
                    abi::decode(words),
                    Ok(request(2, epoch, Operation::MailSend(message)))
                );
                assert_eq!(call(words, inbox_empty), abi::Return { error: 0, value: 0 });
                for more in [false, true] {
                    let chunk = MailChunk {
                        message: Some(message),
                        more,
                    };
                    let expected = payload
                        | (u64::from(len) << 48)
                        | (1 << 51)
                        | (u64::from(more) << 52)
                        | (u64::from(tag) << 53);
                    assert_eq!(
                        call([1, 10, 2, u64::from(epoch), 0, 0], chunk),
                        abi::Return {
                            error: 0,
                            value: expected
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn wire_rejects_reserved_fields_and_unused_bytes_for_every_payload_length() {
    for op in [9, 10] {
        assert_eq!(abi::decode([2, op, 2, 0, 0, 0]), Err(abi::NOT_SUPPORTED));
        for id in [0, 1 << 32, u64::MAX] {
            assert_eq!(abi::decode([1, op, id, 0, 0, 0]), Err(abi::INVALID_PARAM));
        }
        let max = u64::from(u32::MAX);
        let decoded = abi::decode([1, op, max, max, 0, 0]).unwrap();
        assert_eq!((decoded.vm, decoded.epoch), (VmId(u32::MAX), u32::MAX));
        for bit in 0..64 {
            assert_eq!(
                abi::decode([1, op, 2, 11, 0, 1 << bit]),
                Err(abi::INVALID_PARAM)
            );
        }
    }
    for bit in (35..40).chain(48..64) {
        assert_eq!(
            abi::decode([1, 9, 2, 11 | (1 << bit), 0, 0]),
            Err(abi::INVALID_PARAM)
        );
    }
    assert_eq!(
        abi::decode([1, 9, 2, 7 << 32, 0, 0]),
        Err(abi::INVALID_PARAM)
    );
    for len in 0..=6 {
        for bit in (8 * len)..64 {
            assert_eq!(
                abi::decode([1, 9, 2, len << 32, 1 << bit, 0]),
                Err(abi::INVALID_PARAM)
            );
        }
    }
    for bit in 0..64 {
        assert_eq!(
            abi::decode([1, 10, 2, 0, 1 << bit, 0]),
            Err(abi::INVALID_PARAM)
        );
        if bit >= 32 {
            assert_eq!(
                abi::decode([1, 10, 2, 1 << bit, 0, 0]),
                Err(abi::INVALID_PARAM)
            );
        }
    }
}

#[test]
fn explicit_mail_grants_confer_only_send_and_receive() {
    assert_eq!((Rights::MAIL.0, Rights::ALL.0), (4096, 15));
    assert_eq!(Rights::parse("mail"), Some(Rights::MAIL));
    assert_eq!(
        Rights::parse("mail,log"),
        Some(Rights::MAIL.union(Rights::LOG))
    );
    let mut s = Service::new();
    let mut b = Runtime::new();
    let source = Caller::Guest(10);
    let mail = [Operation::MailSend(EMPTY), Operation::MailRecv];
    for op in mail {
        assert_eq!(
            s.execute(Caller::Host, request(99, 0, op), &mut b),
            Err(Error::Unsupported)
        );
    }
    for rights in [
        Rights::ALL,
        Rights::INSPECT,
        Rights::CONSOLE,
        Rights::LOG,
        Rights::disk(DiskSlot::new(3).unwrap()),
    ] {
        s.grant(Caller::Host, 10, VmId(2), rights, &b).unwrap();
        b.reads.set(0);
        for op in mail {
            assert_eq!(
                s.execute(source, request(2, 0, op), &mut b),
                Err(Error::Denied)
            );
        }
        assert_eq!(b.reads.get(), 0);
    }
    s.grant(Caller::Host, 10, VmId(2), Rights::MAIL, &b)
        .unwrap();
    assert_eq!(s.list(source, &b).count(), 0);
    for op in [
        Operation::Inspect,
        Operation::MemoryBytes,
        Operation::Start,
        Operation::Stop,
        Operation::Console,
        Operation::Input(0),
        Operation::ConsoleRead(0),
        Operation::Create(ChildName::new("child").unwrap().into()),
        Operation::Destroy,
    ] {
        assert_eq!(
            s.execute(source, request(2, 11, op), &mut b),
            Err(Error::Denied)
        );
    }
    for op in mail {
        assert_eq!(
            s.execute(Caller::Guest(11), request(1, 0, op), &mut b),
            Err(Error::Denied)
        );
    }
    assert_eq!(b.mail_call.get(), None);
}

#[test]
fn grants_bind_peer_and_caller_incarnations_for_both_mail_operations() {
    let mut b = Runtime::new();
    let mut s = Service::new();
    let mail = [Operation::MailRecv, Operation::MailSend(EMPTY)];
    let caller = Caller::Guest(10);
    s.grant(Caller::Host, 10, VmId(2), Rights::MAIL, &b)
        .unwrap();
    for op in mail {
        assert_eq!(
            s.execute(caller, request(2, 12, op), &mut b),
            Err(Error::StaleEpoch)
        );
    }
    b.row(VmId(2)).epoch = 20;
    for op in mail {
        for epoch in [0, 11] {
            assert_eq!(
                s.execute(caller, request(2, epoch, op), &mut b),
                Err(Error::StaleEpoch)
            );
        }
        for guess in [19, 20, 21] {
            assert_eq!(
                s.execute(caller, request(2, guess, op), &mut b),
                Err(Error::Denied)
            );
        }
    }
    assert_eq!(b.mail_call.get(), None);
    s.grant(Caller::Host, 10, VmId(2), Rights::MAIL, &b)
        .unwrap();
    for op in mail {
        for epoch in [0, 20] {
            s.execute(caller, request(2, epoch, op), &mut b).unwrap();
            assert_eq!(b.mail_call.take().unwrap().peer, (VmId(2), 20));
        }
    }
    b.row(VmId(1)).active = false;
    for op in mail {
        assert_eq!(
            s.execute(caller, request(2, 0, op), &mut b),
            Err(Error::Denied)
        );
    }
    b.row(VmId(1)).epoch = 30;
    b.row(VmId(1)).active = true;
    for issuer in [10, 30] {
        for op in mail {
            assert_eq!(
                s.execute(Caller::Guest(issuer), request(2, 0, op), &mut b),
                Err(Error::Denied)
            );
        }
    }
    assert_eq!(b.mail_call.get(), None);
}

#[test]
fn mailbox_errors_cross_the_wire_and_malformed_typed_messages_fail_closed() {
    let mut b = Runtime::new();
    let mut s = Service::new();
    s.grant(Caller::Host, 10, VmId(2), Rights::MAIL, &b)
        .unwrap();
    for message in [
        MailMessage { len: 7, ..EMPTY },
        MailMessage {
            data: [0, 0, 0, 0, 0, 1],
            len: 5,
            ..EMPTY
        },
    ] {
        let send = Operation::MailSend(message);
        assert_eq!(
            s.execute(Caller::Guest(10), request(2, 0, send), &mut b),
            Err(Error::Invalid)
        );
        assert_eq!(b.mail_call.get(), None);
    }
    for op in [Operation::MailSend(EMPTY), Operation::MailRecv] {
        for error in [Error::Busy, Error::StaleEpoch, Error::NotActive] {
            b.mail_result.set(Err(error));
            let result = s.execute(Caller::Guest(10), request(2, 0, op), &mut b);
            assert_eq!(result, Err(error));
            assert_eq!(
                abi::encode_for(op, result),
                abi::Return::error(abi::INVALID_STATE)
            );
        }
    }
    let reply = Reply {
        info: b.info(VmId(2)).unwrap(),
        changed: false,
        log: None,
        mail: None,
    };
    for mail in [
        None,
        Some(MailChunk {
            message: None,
            more: true,
        }),
        Some(MailChunk {
            message: Some(MailMessage { len: 7, ..EMPTY }),
            more: false,
        }),
        Some(MailChunk {
            message: Some(MailMessage {
                data: [1; 6],
                ..EMPTY
            }),
            more: false,
        }),
    ] {
        assert_eq!(
            abi::encode_for(Operation::MailRecv, Ok(Reply { mail, ..reply })),
            abi::Return::error(abi::FAILED)
        );
    }
}
