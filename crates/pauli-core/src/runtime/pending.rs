//! Parked control replies, independent of the live context and guest RAM.
//! Ownership/Lock Ordering: read queue/target mailbox briefly, never keep a lock parked.
//! Invariants: cancellation revokes the exact ticket generation before cold replacement.
use super::*;
impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub(super) fn poll_pending<R: Tracer>(
        &self,
        vm: &Vm<A>,
        requester: u8,
        pending: &Pending<A::Vcpu>,
        _tracer: &mut R,
    ) -> Option<arch::ControlReply> {
        match pending {
            Pending::Fence(wait) => self
                .fences_complete(vm, wait, requester, true)
                .then_some(arch::ControlReply::Success(0)),
            #[cfg(feature = "guest-control")]
            Pending::Management(ticket) => {
                match crate::monitor::rpc::poll_traced(self, *ticket, _tracer) {
                    pauli_control::queue::Poll::Pending => None,
                    pauli_control::queue::Poll::Complete(result) => {
                        Some(arch::ControlReply::Transport {
                            error: result.error,
                            value: result.value,
                        })
                    }
                    pauli_control::queue::Poll::Cancelled => {
                        Some(arch::ControlReply::Error(arch::ControlError::Failed))
                    }
                }
            }
        }
    }
    pub(super) fn cancel_pending(
        &self,
        vm: &Vm<A>,
        requester: u8,
        pending: Option<Pending<A::Vcpu>>,
    ) {
        match pending {
            Some(Pending::Fence(wait)) => self.cancel_wait(vm, requester, &wait),
            #[cfg(feature = "guest-control")]
            Some(Pending::Management(ticket)) => self.rpc.lock().cancel(ticket),
            None => {}
        }
    }
}
