//! Scalar guest-management queue; only the resident manager executes effects.
//! Ownership/Lock Ordering: short queue lock, release before authorization/backends.
//! Invariants: source epoch/channel come from the actual owner, never guest arguments.
use crate::{
    arch::{ArchSmp, HvArch, MAX_VCPUS},
    board::Board,
    runtime::VmRuntime,
};
use pauli_control::{
    Caller, Service, abi,
    queue::{Poll, Ticket},
};
pub(crate) fn submit<A: HvArch, B: Board>(
    runtime: &VmRuntime<A, B>,
    slot: usize,
    index: u8,
    source: u32,
    args: [u64; 6],
) -> core::result::Result<Ticket, abi::Return> {
    let request = abi::decode(args).map_err(abi::Return::error)?;
    let ticket = runtime
        .rpc
        .lock()
        .submit(slot * MAX_VCPUS + index as usize, source, request)
        .map_err(|_| abi::Return::error(abi::FAILED))?;
    if A::Smp::kick(runtime.board.info().boot_cpu).is_err() {
        runtime.rpc.lock().cancel(ticket);
        return Err(abi::Return::error(abi::FAILED));
    }
    Ok(ticket)
}
pub(crate) fn pump<A: HvArch, B: Board>(
    runtime: &VmRuntime<A, B>,
    service: &mut Service,
    admitting: bool,
) {
    let work = runtime.rpc.lock().take();
    let Some(work) = work else {
        return;
    };
    // A request queued before native shutdown cannot reopen admission when
    // eventually serviced. Inspection and retirement still get normal replies.
    let result = if !admitting
        && matches!(
            work.request.operation,
            pauli_control::Operation::Create(_) | pauli_control::Operation::Start
        ) {
        Err(pauli_control::Error::Busy)
    } else {
        service.execute(
            Caller::Guest(work.source),
            work.request,
            &mut super::Target(runtime),
        )
    };
    let completed = runtime
        .rpc
        .lock()
        .complete(work, abi::encode_for(work.request.operation, result));
    if completed {
        let slot = work.ticket.channel() / MAX_VCPUS;
        let index = work.ticket.channel() % MAX_VCPUS;
        if let Ok(vm) = runtime.manager.vm(slot) {
            if vm.epoch() == work.source {
                if let Some(cpu) = vm.cpu(index) {
                    let _ = A::Smp::kick(cpu.owner);
                }
            }
        }
    }
}
pub(crate) fn poll<A: HvArch, B: Board>(runtime: &VmRuntime<A, B>, ticket: Ticket) -> Poll {
    runtime.rpc.lock().poll(ticket)
}
