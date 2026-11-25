//! Standalone boot and management owner, with no native task scheduler.
//! Ownership/Lock Ordering: board supplies permanent storage; publish before HSM.
//! Invariants: errors at boot do not admit partially initialized guest resources.
use crate::{
    arch::HvArch,
    board::Board,
    manager::VmStore,
    monitor::Monitor,
    runtime::{Dispatcher, VmRuntime},
    trace::{StatsTracer, Tracer},
};
use pauli_hal::{hv_error, sync::Once};

/// A board reserves one instance in its native image; no runtime arena copy
/// or allocator is needed on the initial boot stack.
pub struct BootStorage<A: HvArch, B: Board> {
    vms: VmStore<A>,
    runtime: Once<VmRuntime<A, B>>,
}
impl<A: HvArch, B: Board> BootStorage<A, B> {
    pub const fn new() -> Self {
        Self {
            vms: VmStore::new(),
            runtime: Once::new(),
        }
    }
}
pub fn boot<B: Board, A: HvArch>(board: &'static B, storage: &'static BootStorage<A, B>) -> ! {
    let runtime = match VmRuntime::create(board, &storage.vms) {
        Ok(runtime) => runtime,
        Err(error) => {
            hv_error!("boot configuration: {error:?}");
            board.shutdown();
        }
    };
    if storage.runtime.set(runtime).is_err() {
        hv_error!("runtime already published");
        board.shutdown();
    }
    let Some(runtime) = storage.runtime.get() else {
        board.shutdown();
    };
    if let Err(error) = runtime.start_workers() {
        hv_error!("native worker startup: {error:?}");
        board.shutdown();
    }
    let mut monitor = Monitor::new();
    monitor.welcome(runtime);
    if B::COLLECT_HS_STATS {
        run_boot_loop(
            runtime,
            &mut monitor,
            Dispatcher::with_tracer(StatsTracer::global()),
        );
    } else {
        run_boot_loop(runtime, &mut monitor, Dispatcher::new());
    }
}

fn run_boot_loop<B: Board, A: HvArch, R: Tracer>(
    runtime: &'static VmRuntime<A, B>,
    monitor: &mut Monitor,
    mut local: Dispatcher<A::Timer, R>,
) -> ! {
    loop {
        monitor.poll(runtime);
        let result = match runtime.dispatch(&mut local) {
            Ok(true) => Ok(()),
            Ok(false) => runtime.idle(&mut local),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            monitor
                .terminal
                .output
                .host(format_args!("[HV] ERROR boot CPU dispatch: {error:?}\n"));
            // Join and flush existing VMs before a requested shutdown. Worker
            // cores can continue retirement even if the boot timer failed.
            monitor.close(runtime);
        }
    }
}
