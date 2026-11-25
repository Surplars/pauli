//! Native worker launch and common dispatch loop; one-CPU mode starts no workers.
//! Each worker publishes its own readiness after local setup. Boot opens dispatch
//! and starts guests after every requested worker can receive a kick.
use super::*;

const _: () = assert!(arch::MAX_CPUS <= u64::BITS as usize);

#[derive(Debug, PartialEq, Eq)]
pub enum WorkerStartupError {
    HsmStart {
        owner: usize,
        failure: arch::SmpError,
    },
    ReadyTimeout {
        /// First missing logical owner.
        owner: usize,
        hart: Option<usize>,
        /// All requested owners that had not acknowledged when the wait expired.
        missing: u64,
    },
}

impl<A: HvArch, B: Board> VmRuntime<A, B> {
    pub fn start_workers(&'static self) -> core::result::Result<(), WorkerStartupError> {
        let info = self.board.info();
        let mut expected = 0u64;
        for owner in 0..info.host_cpus {
            if owner == info.boot_cpu {
                continue;
            }
            // SAFETY: self is static, fully initialized, and each arch worker
            // supplies its own retained stack and explicit native CPU identity.
            unsafe { A::Smp::start(owner, worker::<A, B>, self as *const Self as usize) }
                .map_err(|mut failure| {
                    failure.hart = failure.hart.or_else(|| A::Smp::hart_id(owner));
                    WorkerStartupError::HsmStart { owner, failure }
                })?;
            expected |= 1u64 << owner;
        }
        if expected != 0 {
            let start = A::Timer::now();
            let mut now = start;
            loop {
                let missing = expected & !self.online_workers.load(Ordering::Acquire);
                if missing == 0 {
                    break;
                }
                // Allow the cohort one second after the final HSM start returns.
                if now.wrapping_sub(start) >= info.timebase_hz {
                    let owner = missing.trailing_zeros() as usize;
                    return Err(WorkerStartupError::ReadyTimeout {
                        owner,
                        hart: A::Smp::hart_id(owner),
                        missing,
                    });
                }
                core::hint::spin_loop();
                now = A::Timer::now();
            }
            self.ready.store(true, Ordering::Release);
        }
        for vm in self.manager.vms() {
            if vm.config.autostart {
                // Start records its own failure on the VM, including the failed kick.
                let _ = self.manager.start(self.board, vm.slot);
            }
        }
        Ok(())
    }

    pub(crate) fn publish_worker_online(&self, owner: usize) {
        self.online_workers
            .fetch_or(1u64 << owner, Ordering::Release);
    }
}
extern "C" fn worker<A: HvArch, B: Board>(opaque: usize) -> ! {
    // SAFETY: only start_workers passes this retained, release-published runtime pointer.
    let runtime = unsafe { &*(opaque as *const VmRuntime<A, B>) };
    // ArchSmp enters here after this owner has its trap vector and wake sources.
    runtime.publish_worker_online(A::Smp::cpu_id());
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
