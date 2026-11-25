use super::*;

fn words(section: u64, owner: u64, word: u64) -> [u64; 6] {
    [
        1,
        abi::STATS_READ,
        0,
        section | owner << 8 | word << 16,
        0,
        0,
    ]
}

#[test]
fn host_selectors_are_disjoint_from_vm_requests_and_validate_every_word() {
    assert_eq!((abi::STATS_READ, abi::FEAT_STATS), (11, 1));
    for (section, owners, length) in [(0, 1, 8), (1, 64, 16)] {
        for owner in 0..owners {
            for word in 0..length {
                let args = words(section, owner, word);
                let Ok(Call::StatsRead(selector)) = abi::decode_call(args) else {
                    panic!("valid selector rejected")
                };
                assert_eq!(
                    (selector.section(), selector.owner(), selector.word()),
                    (section as u8, owner as u8, word as u8)
                );
                assert_eq!(
                    selector.is_redacted(),
                    section == 1 && (9..=11).contains(&word)
                );
                assert_eq!(abi::decode(args), Err(abi::INVALID_PARAM)); // VM-only decoder.
            }
        }
    }
    for args in [
        words(2, 0, 0),
        words(255, 0, 0),
        words(0, 1, 0),
        words(0, 0, 8),
        words(1, 64, 0),
        words(1, 0, 16),
        words(1, 255, 255),
        [1, 12, 0, 0, 0, 0],
    ] {
        assert_eq!(abi::decode_call(args), Err(abi::INVALID_PARAM));
    }
    for bit in 0..64 {
        for field in [2, 4, 5] {
            let mut args = words(1, 63, 15);
            args[field] = 1u64 << bit;
            assert_eq!(abi::decode_call(args), Err(abi::INVALID_PARAM));
        }
        if bit >= 24 {
            let mut args = words(0, 0, 7);
            args[3] |= 1u64 << bit;
            assert_eq!(abi::decode_call(args), Err(abi::INVALID_PARAM));
        }
    }
    assert_eq!(
        abi::decode_call([2, 11, 0, 0, 0, 0]),
        Err(abi::NOT_SUPPORTED)
    );
    for op in 0..=10 {
        assert_eq!(
            abi::decode_call([1, op, 0, 0, 0, 0]),
            Err(abi::INVALID_PARAM)
        );
    }
    let inspect = request(3, 0, Operation::Inspect);
    assert_eq!(abi::decode_call([1, 0, 3, 0, 0, 0]), Ok(Call::Vm(inspect)));
}

#[test]
fn host_and_vm_authority_are_independent_and_revocation_is_epoch_scoped() {
    let mut backend = Runtime::new();
    let mut service = Service::new();
    let host = Caller::Host;
    let source = Caller::Guest(10);
    let peer = Caller::Guest(11);
    let deny = abi::Return::error(abi::DENIED);
    let stats = abi::decode_call(words(0, 0, 0)).unwrap();
    let inspect = Call::Vm(request(1, 0, Operation::Inspect));
    assert_eq!(service.execute_call(host, stats, &mut backend), deny);
    assert_eq!(backend.stats_word.get(), None);
    assert_eq!(Rights::parse("stats"), None);
    assert_eq!(
        service.grant_stats(source, 10, &backend),
        Err(Error::Denied)
    );
    assert_eq!(service.grant_stats(host, 0, &backend), Err(Error::NotFound));
    assert_eq!(
        service.grant_stats(host, 99, &backend),
        Err(Error::NotFound)
    );
    // Rights even on the caller's own VM and a creator's full child bundle are VM-scoped.
    let rights = Rights::ALL
        .union(Rights::CREATE)
        .union(Rights::DESTROY)
        .union(Rights::LOG)
        .union(Rights::MAIL);
    service.grant(host, 10, VmId(1), rights, &backend).unwrap();
    assert_eq!(service.execute_call(source, inspect, &mut backend).error, 0);
    assert_eq!(service.execute_call(source, stats, &mut backend), deny);
    assert_eq!(backend.stats_word.get(), None);
    service.grant_stats(host, 10, &backend).unwrap();
    service.grant_stats(host, 11, &backend).unwrap();
    service.revoke(host, 10, VmId(1), &backend).unwrap();
    assert_eq!(service.execute_call(source, inspect, &mut backend), deny);
    assert_eq!(
        service.execute_call(source, stats, &mut backend).value,
        u64::MAX
    );
    assert_eq!(service.revoke_stats(peer, 10), Err(Error::Denied));
    assert_eq!(service.execute_call(source, stats, &mut backend).error, 0);
    service.revoke_stats(host, 10).unwrap();
    assert_eq!(service.execute_call(source, stats, &mut backend), deny);
    assert_eq!(service.execute_call(peer, stats, &mut backend).error, 0);
    service.grant_stats(host, 10, &backend).unwrap();
    let owner = backend.row(VmId(1));
    owner.active = false;
    owner.state = State::Stopped;
    backend.stats_word.set(None);
    assert_eq!(
        service.grant_stats(host, 10, &backend),
        Err(Error::NotFound)
    );
    assert_eq!(service.execute_call(source, stats, &mut backend), deny);
    let owner = backend.row(VmId(1));
    owner.active = true;
    owner.state = State::Ready;
    owner.epoch = 20;
    for caller in [source, Caller::Guest(20)] {
        assert_eq!(service.execute_call(caller, stats, &mut backend), deny);
    }
    assert_eq!(backend.stats_word.get(), None);
    service.grant_stats(host, 20, &backend).unwrap();
    service.retire(20, &backend);
    assert_eq!(
        service.execute_call(Caller::Guest(20), stats, &mut backend),
        deny
    );
    assert_eq!(service.execute_call(peer, stats, &mut backend).error, 0);
}

#[test]
fn disabled_or_out_of_range_reads_keep_errors_and_online_csrs_are_always_zero() {
    let mut backend = Runtime::new();
    let mut service = Service::new();
    let caller = Caller::Guest(10);
    service.grant_stats(Caller::Host, 10, &backend).unwrap();
    for idx in 0..16 {
        let selector = StatsWord::new(1, 3, idx).unwrap();
        let call = Call::StatsRead(selector);
        for (result, code) in [
            (Err(Error::Unsupported), abi::NOT_SUPPORTED),
            (Err(Error::Invalid), abi::INVALID_PARAM),
        ] {
            backend.stats_result.set(result);
            backend.stats_word.set(None);
            assert_eq!(
                service.execute_call(Caller::Guest(11), call, &mut backend),
                abi::Return::error(abi::DENIED)
            );
            assert_eq!(backend.stats_word.get(), None);
            assert_eq!(
                service.execute_call(caller, call, &mut backend),
                abi::Return::error(code)
            );
            assert_eq!(backend.stats_word.get(), Some(selector));
        }
        for raw in [0, (1 << 53) + 1, u64::MAX] {
            backend.stats_result.set(Ok(raw));
            let expected = if (9..=11).contains(&idx) { 0 } else { raw };
            assert_eq!(
                service.execute_call(caller, call, &mut backend),
                abi::Return {
                    error: 0,
                    value: expected
                }
            );
        }
    }
    assert_eq!(backend.reads.get(), 0); // No caller-controlled target VM lookup.
}

#[test]
fn stats_requests_share_queue_cancellation_without_a_vm_operation() {
    let mut q = queue::Queue::new();
    let call = abi::decode_call(words(1, 63, 15)).unwrap();
    let ticket = q.submit(0, 10, call).unwrap();
    let work = q.take().unwrap();
    assert_eq!(
        (work.request, work.source, work.request.vm_operation()),
        (call, 10, None)
    );
    q.cancel(ticket);
    let current = q.submit(0, 20, call).unwrap();
    let value = abi::Return {
        error: 0,
        value: u64::MAX,
    };
    assert!(!q.complete(work, value));
    let fresh = q.take().unwrap();
    assert!(q.complete(fresh, value));
    assert_eq!(
        q.poll_operation(current),
        (queue::Poll::Complete(value), None)
    );
}
