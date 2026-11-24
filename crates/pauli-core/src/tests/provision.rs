//! Exercise live publication, admission and recycling through the production manager.
//! Ownership/Lock Ordering: one authority owner, distinct physical CPU/range fixtures.
//! Invariants: no old grants, guest bytes, owner references or translations cross reuse.
use super::model::{Fixture, OWNER};
use crate::{HvError, monitor::backend::Target, runtime::Dispatcher};
use pauli_control::{Backend, Caller, ChildName, Error, Operation, Request, Rights, Service, VmId};
use pauli_devices::memory::GuestMemory;

fn request(vm: VmId, epoch: u32, operation: Operation) -> Request {
    Request {
        vm,
        epoch,
        operation,
    }
}

#[test]
fn failed_stage2_build_returns_its_slot_and_only_spare_extent() {
    let f = Fixture::with_pool(1, [1, 1], 66);
    let rt = &f.runtime;
    super::model::REJECT_NEXT_SPACE.with(|reject| reject.set(true));
    assert_eq!(
        rt.manager
            .provision(rt.board, None, 0, ChildName::new("retry").unwrap().into()),
        Err(HvError::Fault)
    );
    assert_eq!(rt.manager.find("retry"), Err(HvError::NotFound));
    let slot = rt
        .manager
        .provision(rt.board, None, 0, ChildName::new("retry").unwrap().into())
        .unwrap();
    assert_eq!(slot, 2);
    let epoch = rt.manager.vm(slot).unwrap().epoch();
    rt.manager.destroy(slot, epoch).unwrap();
}

#[test]
fn management_cannot_clone_a_boot_peers_network_endpoint() {
    let f = Fixture::with_config(1, [1, 1], 66, |slot, vm| {
        vm.net_peer = Some((1 - slot) as u8);
    });
    let rt = &f.runtime;
    for template in 0..2 {
        let source = rt.manager.vm(template).unwrap();
        assert_eq!(
            rt.manager.provision(
                rt.board,
                None,
                template,
                ChildName::new("work").unwrap().into()
            ),
            Err(HvError::Unsupported)
        );
        assert_eq!(source.config.net_peer, Some((1 - template) as u8));
    }
    assert_eq!(rt.manager.find("work"), Err(HvError::NotFound));
}

struct Tenant<'a> {
    backend: Target<'a, super::model::Arch, super::model::TestBoard>,
    service: Service,
    epoch: u32,
}
impl Tenant<'_> {
    fn call(&mut self, op: Request) -> Result<pauli_control::Info, Error> {
        self.service
            .execute(Caller::Guest(self.epoch), op, &mut self.backend)
            .map(|reply| reply.info)
    }
    fn grant(&mut self, source: u32, target: VmId, rights: Rights) {
        self.service
            .grant(Caller::Host, source, target, rights, &self.backend)
            .unwrap();
    }
}

fn create_authorized(
    tenant: &mut Tenant<'_>,
    template: pauli_control::Info,
) -> (Request, pauli_control::Info) {
    let create = request(
        template.vm,
        template.epoch,
        Operation::Create(ChildName::new("work").unwrap().into()),
    );
    tenant.grant(tenant.epoch, template.vm, Rights::ALL);
    assert_eq!(tenant.call(create), Err(Error::Denied));
    tenant.grant(tenant.epoch, template.vm, Rights::CREATE);
    assert_eq!(
        tenant.call(Request { epoch: 0, ..create }),
        Err(Error::StaleEpoch)
    );
    let child = tenant.call(create).unwrap();
    assert_eq!(
        (child.vm, child.vcpus, child.memory_bytes, child.active),
        (VmId(3), 2, 64 << 20, false)
    );
    let other = Request {
        operation: Operation::Create(ChildName::new("more").unwrap().into()),
        ..create
    };
    assert_eq!(tenant.call(other), Err(Error::NoSpace));
    assert_eq!(tenant.call(create), Err(Error::Busy));
    (create, child)
}

fn run_then_destroy(f: &Fixture, tenant: &mut Tenant<'_>, child: pauli_control::Info, entry: usize) {
    let rt = &f.runtime;
    let op = |operation| request(child.vm, child.epoch, operation);
    tenant.call(op(Operation::Start)).unwrap();
    rt.manager
        .vm(2)
        .unwrap()
        .cpu(1)
        .unwrap()
        .request_start(entry, 0)
        .unwrap();
    let mut owners = [Dispatcher::new(), Dispatcher::new()];
    for (index, owner) in owners.iter_mut().enumerate() {
        OWNER.with(|o| o.set(index));
        for _ in 0..4 {
            rt.dispatch(owner).unwrap();
        }
        assert!(!rt.manager.vm(2).unwrap().cpu(index).unwrap().retired());
    }
    assert_eq!(tenant.call(op(Operation::Destroy)), Err(Error::Busy));
    tenant.call(op(Operation::Stop)).unwrap();
    OWNER.with(|o| o.set(0));
    rt.dispatch(&mut owners[0]).unwrap();
    rt.manager.service(rt.board);
    assert_eq!(tenant.call(op(Operation::Destroy)), Err(Error::Busy));
    OWNER.with(|o| o.set(1));
    rt.dispatch(&mut owners[1]).unwrap();
    rt.manager.service(rt.board);
    let pin = rt.manager.vm(2).unwrap();
    assert!(pin.lifecycle.joined() && pin.retired());
    assert_eq!(tenant.call(op(Operation::Destroy)), Err(Error::Busy));
    drop(pin);
    tenant.call(op(Operation::Destroy)).unwrap();
    assert_eq!(tenant.backend.info(child.vm), Err(Error::NotFound));
    assert_eq!(rt.manager.find("work"), Err(HvError::NotFound));
}

#[test]
fn destroyed_children_recycle_only_after_join_and_last_reader() {
    let f = Fixture::with_pool(2, [1, 2], 66);
    let rt = &f.runtime;
    let mut backend = Target(rt);
    backend.start(VmId(1)).unwrap();
    backend.start(VmId(2)).unwrap();
    let manager = backend.info(VmId(1)).unwrap();
    let template = backend.info(VmId(2)).unwrap();
    let mut tenant = Tenant {
        backend,
        service: Service::new(),
        epoch: manager.epoch,
    };
    let source = rt.manager.vm(1).unwrap();
    let entry = source.config.guest_base + pauli_config::KERNEL_OFFSET;
    assert!(source.devices.io.lock().ram.write(entry, &[0xb1]));
    let (create, child) = create_authorized(&mut tenant, template);
    let scratch = entry + 4096;
    let first_range = {
        let vm = rt.manager.vm(2).unwrap();
        let mut byte = [0];
        let mut io = vm.devices.io.lock();
        assert!(io.ram.read(entry, &mut byte));
        assert_eq!(byte, [0x32]); // Boot template, not the live source's 0xb1.
        assert!(io.ram.write(scratch, &[0xe7]));
        io.ram.host_range()
    };
    tenant.grant(template.epoch, child.vm, Rights::INSPECT);
    run_then_destroy(&f, &mut tenant, child, entry as usize);
    let next = tenant.call(create).unwrap();
    assert_eq!(next.vm, child.vm);
    assert_ne!(next.epoch, child.epoch);
    assert_eq!(
        tenant.call(request(child.vm, child.epoch, Operation::Destroy)),
        Err(Error::StaleEpoch)
    );
    assert_eq!(
        tenant.service.execute(
            Caller::Guest(template.epoch),
            request(next.vm, 0, Operation::Inspect),
            &mut tenant.backend
        ),
        Err(Error::Denied)
    );
    let replacement = rt.manager.vm(2).unwrap();
    let mut byte = [0xff];
    {
        let io = replacement.devices.io.lock();
        assert_eq!(io.ram.host_range(), first_range);
        assert!(io.ram.read(scratch, &mut byte));
    }
    assert_eq!(byte, [0]);
    assert!(source.devices.io.lock().ram.read(entry, &mut byte));
    assert_eq!(byte, [0xb1]);
    assert_eq!(
        tenant.backend.info(manager.vm).unwrap().epoch,
        manager.epoch
    );
}

#[cfg(feature = "guest-control")]
pub(super) fn guest_call(
    f: &Fixture,
    service: &mut Service,
    local: &mut Dispatcher<super::model::Timer>,
    words: [u64; 6],
) -> crate::arch::ControlReply {
    use crate::arch::{Control, ExitKind};
    use crate::vm::slot::CpuState;
    let rt = &f.runtime;
    let issuer = rt.manager.vm(0).unwrap();
    let cpu = issuer.cpu(0).unwrap();
    cpu.data
        .lock()
        .context
        .as_mut()
        .unwrap()
        .script
        .push_back(ExitKind::Control(Control::Management { args: words }));
    rt.dispatch(local).unwrap();
    assert_eq!(cpu.data.lock().state, CpuState::Blocked);
    crate::monitor::rpc::pump(rt, service, true);
    rt.dispatch(local).unwrap();
    cpu.data.lock().context.as_ref().unwrap().reply.unwrap()
}

#[cfg(feature = "guest-control")]
#[test]
fn vcpu_rpc_creates_and_releases_a_previously_absent_resource() {
    use crate::arch::ControlReply;
    use pauli_control::abi;
    let f = Fixture::with_templates(1, [1, 2], 98, super::shape::template);
    let rt = &f.runtime;
    let mut backend = Target(rt);
    let mut service = Service::new();
    backend.start(VmId(1)).unwrap();
    let issuer = rt.manager.vm(0).unwrap();
    let template = backend.info(VmId(2)).unwrap();
    service
        .grant(
            Caller::Host,
            issuer.epoch(),
            template.vm,
            Rights::CREATE.union(Rights::SHAPE),
            &backend,
        )
        .unwrap();
    let mut local = Dispatcher::new();
    let mut call = |words| guest_call(&f, &mut service, &mut local, words);
    let name = u64::from_le_bytes(*b"child\0\0\0");
    for (op, shape, count, bytes) in [
        (3, 0, 2, 96 << 20),
        (abi::CREATE_SHAPED, (3 << 32) | (64 << 40), 3, 64 << 20),
    ] {
        let result = call([1, op, 2, shape | u64::from(template.epoch), name, 0]);
        let child = backend.info(VmId(3)).unwrap();
        let epoch = u64::from(child.epoch);
        assert_eq!(
            result,
            ControlReply::Transport {
                error: 0,
                value: (3u64 << 32) | epoch
            }
        );
        assert_eq!(child.vcpus, count);
        assert_eq!(
            call([1, abi::MEMORY_BYTES, 3, epoch, 0, 0]),
            ControlReply::Transport {
                error: 0,
                value: bytes
            }
        );
        assert!(matches!(
            call([1, 4, 3, epoch, 0, 0]),
            ControlReply::Transport { error: 0, .. }
        ));
        assert_eq!(backend.info(VmId(3)), Err(Error::NotFound));
    }
    assert_eq!(
        backend.destroy(template.vm, template.epoch),
        Err(Error::Denied)
    );
}
