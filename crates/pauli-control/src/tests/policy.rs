//! Authorization, stale epochs and authority retirement.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn permissions() {
    let rights = [
        Rights::INSPECT,
        Rights::START,
        Rights::STOP,
        Rights::CONSOLE,
    ];
    let operations = [
        Operation::Inspect,
        Operation::Start,
        Operation::Stop,
        Operation::Console,
        Operation::Input(0x1d),
    ];
    for mask in 0u8..16 {
        for (index, operation) in operations.into_iter().enumerate() {
            let mut service = Service::new();
            let mut backend = Runtime::new();
            let selected = rights
                .into_iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, right)| right)
                .reduce(Rights::union);
            if let Some(selected) = selected {
                service
                    .grant(Caller::Host, 10, VmId(2), selected, &backend)
                    .unwrap();
            }
            let visible: Vec<_> = service
                .list(Caller::Guest(10), &backend)
                .map(|info| info.vm)
                .collect();
            assert_eq!(visible.len(), usize::from(mask & 1 != 0));
            assert!(visible.iter().all(|id| *id == VmId(2)));
            backend.reads.set(0);
            let result =
                service.execute(Caller::Guest(10), request(2, 11, operation), &mut backend);
            let permitted = mask & (1 << index.min(3)) != 0;
            assert_eq!(
                result.is_ok(),
                permitted,
                "mask={mask} operation={operation:?}"
            );
            if !permitted {
                assert_eq!(result, Err(Error::Denied));
                assert_eq!(backend.reads.get(), 0, "denial must precede target lookup");
                assert!(backend.effects.is_empty());
            }
        }
    }
    let mut service = Service::new();
    let mut backend = Runtime::new();
    for source in [0, 10, 14, u32::MAX] {
        for vm in [0, 1, 2, 4, 5, u32::MAX] {
            for op in operations {
                assert_eq!(
                    service.execute(Caller::Guest(source), request(vm, 0, op), &mut backend),
                    Err(Error::Denied)
                );
            }
        }
    }
    assert!(backend.effects.is_empty());
    assert_eq!(backend.reads.get(), 0);
    assert_eq!(
        service.execute(Caller::Host, request(0, 0, Operation::Start), &mut backend),
        Err(Error::NotFound)
    );
    assert_eq!(
        service.grant(Caller::Host, 10, VmId(u32::MAX), Rights::ALL, &backend),
        Err(Error::NotFound)
    );
    assert_eq!(
        service.grant(Caller::Host, 0, VmId(1), Rights::ALL, &backend),
        Err(Error::NotFound)
    );
}
#[test]
fn epochs() {
    let mut service = Service::new();
    let mut backend = Runtime::new();
    service
        .grant(Caller::Host, 10, VmId(2), Rights::ALL, &backend)
        .unwrap();
    service
        .grant(Caller::Host, 11, VmId(3), Rights::ALL, &backend)
        .unwrap();
    service
        .execute(
            Caller::Guest(10),
            request(2, 11, Operation::Stop),
            &mut backend,
        )
        .unwrap();
    assert_eq!(
        service.execute(
            Caller::Guest(10),
            request(2, 11, Operation::Input(1)),
            &mut backend
        ),
        Err(Error::NotActive)
    );
    let reply = service
        .execute(
            Caller::Guest(10),
            request(2, 11, Operation::Start),
            &mut backend,
        )
        .unwrap();
    assert!(reply.changed && reply.info.epoch == 14 && reply.info.vm == VmId(2));
    let effects = backend.effects.len();
    for op in [
        Operation::Start,
        Operation::Stop,
        Operation::Console,
        Operation::Input(1),
    ] {
        assert_eq!(
            service.execute(Caller::Guest(10), request(2, 11, op), &mut backend),
            Err(Error::StaleEpoch)
        );
    }
    assert_eq!(backend.effects.len(), effects);
    assert_eq!(
        service.execute(
            Caller::Guest(11),
            request(3, 12, Operation::Stop),
            &mut backend
        ),
        Err(Error::Denied)
    );
    assert_eq!(
        service.execute(
            Caller::Guest(14),
            request(3, 12, Operation::Stop),
            &mut backend
        ),
        Err(Error::Denied)
    );
    assert!(
        service
            .execute(
                Caller::Guest(10),
                request(2, 0, Operation::Inspect),
                &mut backend
            )
            .is_ok()
    );

    replacement_manager(service, backend);
}

fn replacement_manager(mut service: Service, mut backend: Runtime) {
    // Both stale and replacement manager identities lack the old authority.
    service
        .execute(Caller::Host, request(1, 10, Operation::Stop), &mut backend)
        .unwrap();
    let reply = service
        .execute(Caller::Host, request(1, 10, Operation::Start), &mut backend)
        .unwrap();
    assert_eq!(reply.info.epoch, 15);
    for source in [10, 15] {
        assert_eq!(
            service.execute(
                Caller::Guest(source),
                request(2, 14, Operation::Stop),
                &mut backend
            ),
            Err(Error::Denied)
        );
    }
    service
        .grant(Caller::Host, 15, VmId(2), Rights::INSPECT, &backend)
        .unwrap();
    assert!(
        service
            .execute(
                Caller::Guest(15),
                request(2, 0, Operation::Inspect),
                &mut backend
            )
            .is_ok()
    );

    // A failed activation which replaced an epoch still retires its grants.
    backend.row(VmId(1)).active = false;
    backend.row(VmId(1)).state = State::Faulted;
    backend.fail_boot = true;
    assert_eq!(
        service.execute(Caller::Host, request(1, 15, Operation::Start), &mut backend),
        Err(Error::Backend("boot failed"))
    );
    backend.row(VmId(1)).epoch = 15; // Test a stale credential independently of liveness checking.
    backend.row(VmId(1)).active = true;
    assert!(service.list(Caller::Guest(15), &backend).next().is_none());
}
#[test]
fn revocation() {
    let mut service = Service::new();
    let mut backend = Runtime::new();
    service
        .grant(Caller::Host, 10, VmId(2), Rights::ALL, &backend)
        .unwrap();
    service
        .grant(Caller::Host, 10, VmId(2), Rights::INSPECT, &backend)
        .unwrap();
    assert_eq!(
        service.execute(
            Caller::Guest(10),
            request(2, 11, Operation::Stop),
            &mut backend
        ),
        Err(Error::Denied)
    );
    assert_eq!(
        service.grant(Caller::Guest(10), 11, VmId(2), Rights::ALL, &backend),
        Err(Error::Denied)
    );
    assert_eq!(
        service.revoke(Caller::Guest(10), 10, VmId(2)),
        Err(Error::Denied)
    );
    service.revoke(Caller::Host, 10, VmId(2)).unwrap();
    assert_eq!(
        service.execute(
            Caller::Guest(10),
            request(2, 0, Operation::Inspect),
            &mut backend
        ),
        Err(Error::Denied)
    );
    // Retain grants across unreported epoch changes to fill the authority ledger.
    // Its capacity is independent of how many VM IDs this fixture exposes.
    for source in 10..10 + (grants::PER_PAGE / backend.vms.len()) as u32 {
        backend.row(VmId(1)).epoch = source;
        for vm in 1..=4 {
            service
                .grant(Caller::Host, source, VmId(vm), Rights::ALL, &backend)
                .unwrap();
        }
    }
    backend.row(VmId(1)).epoch = 99;
    assert!(service.list(Caller::Guest(10), &backend).next().is_none());
    assert_eq!(
        service.execute(
            Caller::Guest(10),
            request(2, 11, Operation::Stop),
            &mut backend
        ),
        Err(Error::Denied)
    );
    assert_eq!(
        service.grant(Caller::Host, 99, VmId(1), Rights::ALL, &backend),
        Err(Error::NoSpace)
    );
    service.retire(10);
    for vm in 1..=4 {
        service
            .grant(Caller::Host, 99, VmId(vm), Rights::ALL, &backend)
            .unwrap();
    }
    backend.row(VmId(1)).active = false;
    assert!(service.list(Caller::Guest(99), &backend).next().is_none());
}
