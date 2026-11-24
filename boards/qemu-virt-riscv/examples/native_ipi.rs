#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! OpenSBI-direct, two-hart IPI timing reference. No VM, timer or dispatcher.
//! The boot hart sends; the stackless worker owns its trap vector and all result fields.
//! A new IPI cannot arrive until the sender has consumed the previous sequence.

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
mod probe {
    use core::{
        mem::offset_of,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use pauli_arch_riscv::{
        bits,
        host::{sbi, timer},
    };
    use pauli_hal::{fdt::Fdt, log};
    use qemu_virt_riscv::fdt;

    const ROUND: usize = 8;
    const SAMPLES: usize = 4 * ROUND;
    const STOPPED: usize = 1; // SBI HSM hart status.
    const FAILED: usize = usize::MAX;
    const WAIT_ARMED: usize = 1;
    const WAIT_RESUME: usize = 2;
    const BUSY: usize = 3;

    #[repr(C)]
    struct Mail {
        start: AtomicUsize,
        ready: AtomicUsize,
        done: AtomicUsize,
        hart: AtomicUsize,
        ticks: AtomicUsize,
        site: AtomicUsize,
        work: AtomicUsize,
    }
    static MAIL: Mail = Mail {
        start: AtomicUsize::new(0),
        ready: AtomicUsize::new(0),
        done: AtomicUsize::new(0),
        hart: AtomicUsize::new(0),
        ticks: AtomicUsize::new(0),
        site: AtomicUsize::new(0),
        work: AtomicUsize::new(0),
    };
    core::arch::global_asm!(include_str!("../startup.S"), sie = const bits::SSTATUS_SIE);
    core::arch::global_asm!(
        include_str!("native_ipi/worker.S"),
        start = const offset_of!(Mail, start), ready = const offset_of!(Mail, ready),
        done = const offset_of!(Mail, done), hart = const offset_of!(Mail, hart),
        ticks = const offset_of!(Mail, ticks), site = const offset_of!(Mail, site),
        work = const offset_of!(Mail, work), round = const ROUND, samples = const SAMPLES,
        sie = const bits::SSTATUS_SIE, ssip = const bits::SSIP, hsm = const sbi::HSM,
        wait_armed = const WAIT_ARMED, wait_resume = const WAIT_RESUME, busy = const BUSY,
    );
    unsafe extern "C" {
        fn native_ipi_worker();
    }

    #[unsafe(no_mangle)]
    extern "C" fn pauli_boot(hart: usize, dtb: usize) -> ! {
        let _ = log::install(&sbi::FirmwareConsole);
        // SAFETY: a1 is the retained native firmware DTB, not guest memory.
        let blob = unsafe { fdt::firmware_blob(dtb) }.expect("native DTB");
        let topo = fdt::discover(Fdt::new(blob).expect("invalid DTB"), hart).expect("topology");
        assert_eq!(topo.count, 2, "two harts required");
        assert!(
            [sbi::IPI, sbi::HSM, sbi::DBCN, sbi::SRST]
                .into_iter()
                .all(sbi::probe)
        );
        let target = topo.harts[1 - topo.boot];
        let status = sbi::hart_status(target);
        assert_eq!(
            (status.error, status.value),
            (0, STOPPED),
            "worker not stopped"
        );
        MAIL.start.store(1, Ordering::Release);
        // SAFETY: target is the other stopped hart; entry is identity mapped and stackless.
        // The mailbox and worker text remain valid until the HSM stop is confirmed.
        let started = unsafe {
            sbi::hart_start(
                target,
                native_ipi_worker as *const () as usize,
                &raw const MAIL as usize,
            )
        };
        assert_eq!(started.error, 0, "HSM start");
        await_seq(&MAIL.ready, 1);
        assert_eq!(
            MAIL.hart.load(Ordering::Relaxed),
            target,
            "wrong receiving hart"
        );
        log::print(format_args!(
            "native-ipi ready sender={hart} receiver={target} hz={}\n",
            topo.hz
        ))
        .expect("console");
        for seq in 1..=SAMPLES {
            sample(seq, target);
        }
        loop {
            let status = sbi::hart_status(target);
            assert_eq!(status.error, 0, "HSM status");
            if status.value == STOPPED {
                break;
            }
            assert_ne!(
                MAIL.done.load(Ordering::Acquire),
                FAILED,
                "HSM stop returned"
            );
            core::hint::spin_loop();
        }
        log::print(format_args!("native-ipi complete\n")).expect("console");
        shutdown()
    }

    fn await_seq(field: &AtomicUsize, seq: usize) {
        loop {
            match field.load(Ordering::Acquire) {
                n if n == seq => return,
                FAILED => panic!("receiver rejected SSIP or interrupted PC"),
                _ => core::hint::spin_loop(),
            }
        }
    }

    fn sample(seq: usize, target: usize) {
        await_seq(&MAIL.ready, seq);
        // Measure before the SBI call: the target can trap before the ecall returns.
        let send = timer::read_time();
        let result = sbi::send_ipi(target);
        let returned = timer::read_time();
        assert_eq!(result.error, 0, "SBI IPI");
        await_seq(&MAIL.done, seq);
        let trap = MAIL.ticks.load(Ordering::Relaxed);
        let work = MAIL.work.load(Ordering::Relaxed);
        let (mode, site) = match (MAIL.site.load(Ordering::Relaxed), (seq - 1) / ROUND) {
            (WAIT_ARMED, 0 | 3) if work == 0 => ("wait", "wait-armed"),
            (WAIT_RESUME, 0 | 3) if work == 0 => ("wait", "wait-resume"),
            (BUSY, 1 | 2) if work > 0 => ("busy", "busy"),
            _ => panic!("unexpected SSIP sample {seq}"),
        };
        assert!(trap as u64 >= send && returned >= send, "TIME regressed");
        log::print(format_args!("native-ipi seq={seq} mode={mode} send={send} returned={returned} trap={trap} site={site} work={work}\n")).expect("console");
    }

    fn shutdown() -> ! {
        let _ = sbi::shutdown();
        loop {
            core::hint::spin_loop();
        }
    }
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
        let _ = log::print(format_args!("native-ipi error: {info}\n"));
        shutdown()
    }
}
