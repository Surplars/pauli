//! Native worker launch and common dispatch loop; one-CPU mode starts no workers.
//! Ownership/Lock Ordering: static runtime publishes before HSM; ready is release/acquire.
//! Invariants: worker addresses/stacks outlive hardware execution; errors stay VM-local.
use super::*;
impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub fn start_workers(&'static self) -> Result<()> {
        let info = self.board.info();
        for owner in 0..info.host_cpus {
            if owner == info.boot_cpu {
                continue;
            }
            // SAFETY: self is static, fully initialized, and each arch worker
            // supplies its own retained stack and explicit native CPU identity.
            unsafe { A::Smp::start(owner, worker::<A, B>, self as *const Self as usize) }?;
        }
        self.ready.store(true, Ordering::Release);
        for vm in self.manager.vms() {
            if vm.config.autostart {
                if let Err(error) = self.manager.start(self.board, vm.slot) {
                    vm.fail_join(error);
                }
            }
        }
        Ok(())
    }
}
extern "C" fn worker<A: HvArch, B: Board>(opaque: usize) -> ! {
    // SAFETY: only start_workers passes this retained, release-published runtime pointer.
    let runtime = unsafe { &*(opaque as *const VmRuntime<A, B>) };
    while !runtime.ready.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let mut local = Dispatcher::new();
    loop {
        match runtime.dispatch(&mut local) {
            Ok(false) => {
                let _ = runtime.idle(&mut local);
            }
            _ => {}
        }
    }
}
