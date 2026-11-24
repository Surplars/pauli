#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! OpenSBI-direct control for QEMU firmware latency: no VMs or native dispatcher.
//! One hart owns TIME/STIE. Only window summaries go to the firmware console.

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(target_os = "none")]
mod probe {
    use pauli_arch_riscv::{
        bits::SSTATUS_SIE,
        host::{irq, sbi, timer},
    };
    use pauli_hal::{fdt::Fdt, log};
    use qemu_virt_riscv::fdt;

    core::arch::global_asm!(include_str!("../startup.S"), sie = const SSTATUS_SIE);

    #[unsafe(no_mangle)]
    extern "C" fn pauli_boot(hart: usize, dtb: usize) -> ! {
        let _ = log::install(&sbi::FirmwareConsole);
        // SAFETY: a1 is the QEMU/OpenSBI handoff, not guest-supplied memory.
        let blob = unsafe { fdt::firmware_blob(dtb) }.expect("native DTB");
        let topo = fdt::discover(Fdt::new(blob).expect("invalid DTB"), hart).expect("topology");
        assert_eq!(topo.count, 1, "one hart required");
        assert!(sbi::probe(sbi::TIME) && sbi::probe(sbi::DBCN) && sbi::probe(sbi::SRST));
        // Startup cleared sie and global SIE. Only STIE participates in WFI;
        // this probe never enables global IRQ delivery.
        irq::unmask_timer();
        let hz = topo.hz;
        for round in 1..=4 {
            // Keep windows anchored to machine TIME, not the completion of the
            // previous window or a fixed sample count that grows in duration.
            measure(round, hz);
        }
        assert_eq!(timer::set_sbi_timer(u64::MAX).error, 0);
        log::print(format_args!("native-timer complete\n")).expect("console");
        shutdown()
    }

    fn measure(round: u64, hz: u64) {
        let until = round * 2 * hz;
        let start = timer::read_time();
        assert!(start < until, "missed native timer window");
        let mut end = start;
        let mut bins = [0_u64; 4];
        let (mut early, mut arm_ge10ms) = (0_u64, 0_u64);
        while end < until {
            let deadline = timer::read_time() + hz.div_ceil(1_000);
            let set = timer::set_sbi_timer(deadline);
            let armed = timer::read_time();
            assert_eq!(set.error, 0, "SBI TIME");
            // An early WFI is not a timer event. Keep the original compare:
            // rearming here could postpone expiry forever on a spurious wake.
            loop {
                // SAFETY: STIE is locally enabled, SIE clear, no locks held.
                unsafe { irq::wait() };
                let pending = irq::timer_pending();
                end = timer::read_time();
                if pending && end >= deadline {
                    break;
                }
                early += 1;
            }
            let late = end - deadline;
            // Same disjoint boundaries as HS idle; never add SBI and WFI delays.
            let bin = if late < hz.div_ceil(1_000) {
                0
            } else if late < hz.div_ceil(200) {
                1
            } else if late < hz.div_ceil(100) {
                2
            } else {
                arm_ge10ms += u64::from(armed >= deadline + hz.div_ceil(100));
                3
            };
            bins[bin] += 1;
        }
        let [lt1ms, lt5ms, lt10ms, ge10ms] = bins;
        let samples = early + bins.iter().sum::<u64>();
        log::print(format_args!(
            "native-timer round={round} hz={hz} start_ticks={start} end_ticks={end} samples={samples} early={early} arm_ge10ms={arm_ge10ms} lt1ms={lt1ms} lt5ms={lt5ms} lt10ms={lt10ms} ge10ms={ge10ms}\n"
        )).expect("console");
    }

    fn shutdown() -> ! {
        let _ = sbi::shutdown();
        // An unsupported reset must not turn failure into a successful report.
        loop {
            core::hint::spin_loop();
        }
    }

    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
        let _ = log::print(format_args!("native-timer error: {info}\n"));
        shutdown()
    }
}
