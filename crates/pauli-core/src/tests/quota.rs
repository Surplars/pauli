//! Sponsor identity across quota denial, authority renewal and third-party teardown.
//! Ownership/Lock Ordering: one host authority and one guest sender drive a real VM manager.
//! Invariants: quota covers all allocated children, even when a child or sponsor is stopped.
use super::model::Fixture;
use crate::{HvError, manager::Budget, monitor::backend::Target, runtime::Dispatcher};
use pauli_control::{
    Backend, Caller, ChildName, Error, Info, Operation, Request, Rights, Service, VmId,
};

fn clone_as(
    service: &mut Service,
    backend: &mut impl Backend,
    sender: Info,
    name: &str,
) -> Result<Info, Error> {
    let template = backend.info(VmId(2))?;
    let request = Request {
        vm: template.vm,
        epoch: template.epoch,
        operation: Operation::Create(ChildName::new(name).unwrap().into()),
    };
    service
        .execute(Caller::Guest(sender.epoch), request, backend)
        .map(|r| r.info)
}
fn grant(service: &mut Service, backend: &impl Backend, sender: Info) {
    service
        .grant(Caller::Host, sender.epoch, VmId(2), Rights::CREATE, backend)
        .unwrap();
}

#[test]
fn native_grant_pages_use_distinct_host_memory_and_require_a_pool() {
    let fixed = Fixture::new(1, 1);
    assert!(matches!(
        Target(&fixed.runtime).grant_page(),
        Err(Error::NoSpace)
    ));
    let pooled = Fixture::with_pool(1, [1, 1], 1);
    let target = Target(&pooled.runtime);
    let a = target.grant_page().unwrap();
    let b = target.grant_page().unwrap();
    assert!(!core::ptr::eq(a, b));
}

#[test]
fn firmware_children_share_one_mib_of_sponsor_quota() {
    use std::string::ToString;
    let f = Fixture::with_config(1, [1; 2], 8, |_, vm| {
        vm.memory_size = 512 << 10;
        vm.firmware = Some(pauli_config::FirmwareLayout {
            rx_pages: 1,
            ro_pages: 0,
        });
    });
    let m = &f.runtime.manager;
    let mut target = Target(&f.runtime);
    let mut service = Service::new();
    target.start(VmId(1)).unwrap();
    let sender = target.info(VmId(1)).unwrap();
    grant(&mut service, &target, sender);
    m.set_budget(0, Some(Budget::from_mib(3, 3, 0))).unwrap();
    assert_eq!(
        clone_as(&mut service, &mut target, sender, "one"),
        Err(Error::NoSpace)
    );
    m.set_budget(0, Some(Budget::from_mib(3, 3, 1))).unwrap();
    let one = clone_as(&mut service, &mut target, sender, "one").unwrap();
    assert_eq!(
        m.budget(0).unwrap().0.to_string(),
        "guests=1 vcpus=1 memory=512KiB"
    );
    clone_as(&mut service, &mut target, sender, "two").unwrap();
    assert_eq!(m.budget(0).unwrap().0, Budget::from_mib(2, 2, 1));
    assert_eq!(
        clone_as(&mut service, &mut target, sender, "three"),
        Err(Error::NoSpace)
    );
    service
        .execute(
            Caller::Guest(sender.epoch),
            Request {
                vm: one.vm,
                epoch: one.epoch,
                operation: Operation::Destroy,
            },
            &mut target,
        )
        .unwrap();
    clone_as(&mut service, &mut target, sender, "three").unwrap();
    assert_eq!(m.budget(0).unwrap().0, Budget::from_mib(2, 2, 1));
}

#[test]
fn sponsor_budgets_survive_restart_and_release_only_on_actual_destroy() {
    let f = Fixture::with_pool(1, [1, 2], 198);
    let rt = &f.runtime;
    let m = &rt.manager;
    let mut target = Target(rt);
    let mut service = Service::new();
    let manager = target
        .create(None, VmId(1), ChildName::new("admin").unwrap().into())
        .unwrap();
    let slot = m.find("admin").unwrap();
    target.start(manager.vm).unwrap();
    let manager = target.info(manager.vm).unwrap();
    grant(&mut service, &target, manager);
    let budget = Budget::from_mib;
    for limit in [budget(0, 8, 256), budget(4, 1, 256), budget(4, 8, 63)] {
        m.set_budget(slot, Some(limit)).unwrap();
        assert_eq!(
            clone_as(&mut service, &mut target, manager, "child"),
            Err(Error::NoSpace)
        );
        assert_eq!(m.budget(slot).unwrap(), (Budget::default(), Some(limit)));
    }
    let cap = budget(2, 3, 128);
    m.set_budget(slot, Some(cap)).unwrap();
    let child = clone_as(&mut service, &mut target, manager, "child").unwrap();
    assert!(!child.active);
    assert_eq!(
        m.set_budget(slot, Some(Budget::default())),
        Err(HvError::Busy)
    );
    target.stop(manager.vm).unwrap();
    let mut hart = Dispatcher::new();
    rt.dispatch(&mut hart).unwrap();
    m.service(rt.board);
    assert!(m.vm(slot).unwrap().lifecycle.joined());
    assert_eq!(target.destroy(manager.vm, manager.epoch), Err(Error::Busy));
    target.start(manager.vm).unwrap();
    let fresh = target.info(manager.vm).unwrap();
    assert_ne!(fresh.epoch, manager.epoch);
    grant(&mut service, &target, fresh);
    assert_eq!(m.budget(slot).unwrap(), (budget(1, 2, 64), Some(cap)));
    assert_eq!(
        clone_as(&mut service, &mut target, manager, "other"),
        Err(Error::Denied)
    );
    assert_eq!(
        clone_as(&mut service, &mut target, fresh, "other"),
        Err(Error::NoSpace)
    );
    let destroy = Request {
        vm: child.vm,
        epoch: child.epoch,
        operation: Operation::Destroy,
    };
    service.execute(Caller::Host, destroy, &mut target).unwrap();
    assert_eq!(m.budget(slot).unwrap(), (Budget::default(), Some(cap)));
    let replacement = clone_as(&mut service, &mut target, fresh, "other").unwrap();
    assert_eq!(replacement.vm, child.vm);
    assert_ne!(replacement.epoch, child.epoch);
    assert_eq!(m.budget(slot).unwrap().0, budget(1, 2, 64));
}
