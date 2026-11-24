#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]
//! Two-vCPU VS mailbox probe. The boot CPU issues one request at a time; only
//! its peer clears VSSIP and publishes receipts. No host tracer or device I/O
//! is used inside a measured interval. All UART output belongs to the boot CPU.

#[cfg(not(target_os = "none"))]
fn main() {}

#[cfg(any(test, target_os = "none"))]
fn guest_timebase(blob: &[u8]) -> Option<u64> {
    use pauli_hal::fdt::Fdt;
    let tree = Fdt::new(blob).ok()?;
    let hz = tree.find("/cpus").ok()??.u32("timebase-frequency").ok()??;
    // The guest board emits dense hart IDs. Check the requested pair, not the
    // physical host topology: the virtual CPUs intentionally do not expose H.
    for (path, id) in [("/cpus/cpu@0", 0), ("/cpus/cpu@1", 1)] {
        let cpu = tree.find(path).ok()??;
        if !cpu.available().ok()? || cpu.u32("reg").ok()?? != id {
            return None;
        }
    }
    (hz != 0 && tree.find("/cpus/cpu@2").ok()?.is_none()).then_some(u64::from(hz))
}

#[cfg(test)]
#[test]
fn reads_two_guest_cpus_without_requiring_host_extensions() {
    use pauli_config::{GuestBoardLayout, MAX_GUEST_DTB, VmConfig, guest_dtb};
    let mut vm = VmConfig::new("mailbox").unwrap();
    let mut blob = [0; MAX_GUEST_DTB];
    for (cores, expected) in [(1, None), (2, Some(10_000_000)), (3, None)] {
        vm.vcpus = cores;
        let len =
            guest_dtb::riscv(&vm, GuestBoardLayout::DEFAULT, 10_000_000, "", &mut blob).unwrap();
        assert_eq!(guest_timebase(&blob[..len]), expected);
    }
}

#[cfg(target_os = "none")]
mod vs_mailbox {
    mod handoff;
    use core::{
        arch::asm,
        fmt::{self, Write},
        sync::atomic::{
            AtomicBool, AtomicUsize,
            Ordering::{Acquire, Relaxed, Release},
        },
    };
    use pauli_arch_riscv::{
        bits,
        sbi::{HSM, IPI, RFENCE, SRST},
    };
    use qemu_virt_riscv::fdt;

    core::arch::global_asm!(include_str!("vs_mailbox/entry.S"),
        load_offset = const pauli_config::KERNEL_OFFSET, sie = const bits::SSTATUS_SIE);
    unsafe extern "C" {
        fn vs_mailbox_secondary_entry();
    }

    const WARM: usize = 4;
    const SAMPLES: usize = 24;
    const PEER: usize = 1;
    const ONE_HART_MASK: usize = 1;
    const STOPPED: usize = 1;
    const BAD: usize = usize::MAX;
    const NAMES: [&str; 3] = ["ipi", "vma", "fencei"];
    const HANDSHAKE_SECONDS: u64 = 45;
    const OP_SECONDS: u64 = 3;
    const UART: usize = pauli_config::GuestBoardLayout::DEFAULT.uart.base as usize;
    const LSR: usize = 5;
    const THRE: u8 = 1 << 5;
    const DATA_READY: u8 = 1;
    static LIVE: AtomicBool = AtomicBool::new(false);
    static COMMAND: AtomicUsize = AtomicUsize::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    static REQUEST: AtomicUsize = AtomicUsize::new(0);
    static RECEIPT: AtomicUsize = AtomicUsize::new(0);

    fn ticks() -> u64 {
        let value: u64;
        // SAFETY: VS TIME is enabled. Keep the compiler memory barrier so that
        // shared acknowledgements and SBI calls cannot move across a timestamp.
        unsafe {
            asm!("rdtime {}", out(reg) value, options(nostack));
        }
        value
    }
    fn call(eid: usize, fid: usize, args: [usize; 4]) -> (isize, usize) {
        let (err, value);
        // SAFETY: VS SBI call using only guest hart IDs and validated local addresses.
        unsafe {
            asm!("ecall", inlateout("a0") args[0] => err, inlateout("a1") args[1] => value,
                in("a2") args[2], in("a3") args[3], in("a6") fid, in("a7") eid, options(nostack));
        }
        (err, value)
    }
    fn ok(eid: usize, fid: usize, args: [usize; 4]) -> usize {
        let (err, value) = call(eid, fid, args);
        assert_eq!(err, 0, "guest SBI {eid:#x}:{fid}");
        value
    }
    fn until(deadline: u64, mut satisfied: impl FnMut() -> bool) {
        while !satisfied() {
            assert!(ticks() < deadline, "guest timeout");
            core::hint::spin_loop();
        }
    }

    struct Uart;
    impl Uart {
        fn read(offset: usize) -> u8 {
            // SAFETY: fixed byte-wide virtual UART of the QEMU guest layout.
            unsafe { core::ptr::read_volatile((UART + offset) as *const u8) }
        }
        fn key(hz: u64) -> u8 {
            until(ticks() + HANDSHAKE_SECONDS * hz, || {
                Self::read(LSR) & DATA_READY != 0
            });
            Self::read(0)
        }
    }
    impl Write for Uart {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            for byte in s.bytes() {
                while Self::read(LSR) & THRE == 0 {
                    core::hint::spin_loop();
                }
                // SAFETY: the boot vCPU is the only writer, and THRE granted queue space.
                unsafe {
                    core::ptr::write_volatile(UART as *mut u8, byte);
                }
            }
            Ok(())
        }
    }

    #[unsafe(no_mangle)]
    extern "C" fn vs_mailbox_secondary(hart: usize, _: usize) -> ! {
        if hart == PEER {
            LIVE.store(true, Release);
            // Select the receiver once: ordinary mailbox timings have no midpoints
            // or policy branches in their per-receipt path.
            let command = loop {
                let cmd = COMMAND.load(Acquire);
                if cmd != 0 {
                    break cmd;
                }
                core::hint::spin_loop();
            };
            let stopped = if command == usize::from(b'i') {
                handoff::receive()
            } else {
                receive()
            };
            if stopped {
                let _ = call(HSM, 1, [0; 4]); // Successful hart_stop never returns.
            }
        }
        RECEIPT.store(BAD, Release); // A duplicate SSIP or an unexpected HSM return.
        loop {
            core::hint::spin_loop();
        }
    }

    /// Global SIE stays clear. Only the peer consumes the delegated VS SSIP,
    /// and the sender never issues its next IPI before the current receipt.
    #[inline]
    fn take_ssip() -> bool {
        let pending: usize;
        // SAFETY: this vCPU owns its virtual sip and SSIP is guest-writable.
        unsafe {
            asm!("csrr {}, sip", out(reg) pending, options(nostack));
            if pending & bits::SSIP == 0 {
                return false;
            }
            asm!("csrci sip, {ssip}", ssip = const bits::SSIP, options(nostack));
        }
        true
    }

    fn receive() -> bool {
        let mut seen = 0;
        while !STOP.load(Acquire) {
            if take_ssip() {
                seen += 1;
                if REQUEST.load(Acquire) != seen {
                    return false;
                }
                RECEIPT.store(seen, Release);
            }
        }
        true
    }

    fn sample(op: usize, generation: usize, hz: u64) -> (u64, u64) {
        if op == 0 {
            REQUEST.store(generation, Release);
        }
        let begin = ticks();
        match op {
            0 => {
                ok(IPI, 0, [ONE_HART_MASK, PEER, 0, 0]);
                until(begin + OP_SECONDS * hz, || {
                    let seen = RECEIPT.load(Acquire);
                    assert_ne!(seen, BAD, "worker rejected IPI sequence");
                    seen == generation
                });
            }
            // SBI defines success only after the remote operation completes.
            1 => {
                ok(RFENCE, 1, [ONE_HART_MASK, PEER, 0, usize::MAX]);
            }
            _ => {
                ok(RFENCE, 0, [ONE_HART_MASK, PEER, 0, 0]);
            }
        }
        let elapsed = ticks() - begin;
        assert!(elapsed < OP_SECONDS * hz, "remote operation timed out");
        (begin, elapsed)
    }

    #[unsafe(no_mangle)]
    extern "C" fn vs_mailbox_primary(hart: usize, dtb: usize) -> ! {
        assert_eq!(hart, 0);
        // SAFETY: a1 is the immutable, bounded guest DTB supplied by the loader.
        let blob = unsafe { fdt::firmware_blob(dtb) }.expect("guest DTB header");
        let hz = super::guest_timebase(blob).expect("guest CPU pair and timebase");
        assert_eq!(ok(HSM, 2, [PEER, 0, 0, 0]), STOPPED);
        ok(
            HSM,
            0,
            [PEER, vs_mailbox_secondary_entry as *const () as usize, 0, 0],
        );
        until(ticks() + OP_SECONDS * hz, || LIVE.load(Acquire));
        writeln!(Uart, "[mb] ready cpus=2 hz={hz}").unwrap();
        let command = Uart::key(hz);
        assert!(
            matches!(command, b'g' | b'd' | b'i'),
            "unexpected start command"
        );
        COMMAND.store(usize::from(command), Release);
        if command == b'i' {
            handoff::run(hz);
        } else {
            run(hz, command == b'd');
        }
        writeln!(Uart, "[mb] done").unwrap();
        assert_eq!(Uart::key(hz), b'q', "unexpected shutdown command");
        writeln!(Uart, "[mb] poweroff").unwrap();
        shutdown()
    }

    fn stop_peer(hz: u64) -> usize {
        STOP.store(true, Release);
        until(ticks() + OP_SECONDS * hz, || {
            assert_ne!(RECEIPT.load(Acquire), BAD, "worker stop");
            ok(HSM, 2, [PEER, 0, 0, 0]) == STOPPED
        });
        RECEIPT.load(Acquire)
    }

    fn run(hz: u64, split: bool) {
        let mut results = [[(0, 0); 3]; SAMPLES];
        for round in 0..WARM + SAMPLES {
            for offset in 0..3 {
                let op = (round + offset) % 3;
                let window = sample(op, round + 1, hz);
                if round >= WARM {
                    results[round - WARM][op] = window;
                }
            }
        }
        let receipts = stop_peer(hz);
        assert_eq!(receipts, WARM + SAMPLES);
        for (row, samples) in results.iter().enumerate() {
            // Preserve the measured ordering; no console exits occurred while scoring.
            for offset in 0..3 {
                let op = (WARM + row + offset) % 3;
                let (start, ticks) = samples[op];
                write!(
                    Uart,
                    "[mb] sample op={} seq={} ticks={ticks}",
                    NAMES[op],
                    row + 1,
                )
                .unwrap();
                if split {
                    write!(Uart, " start={start}").unwrap();
                }
                writeln!(Uart).unwrap();
            }
        }
        writeln!(Uart, "[mb] receipts ipi={receipts}").unwrap();
    }

    fn shutdown() -> ! {
        let _ = call(SRST, 0, [0; 4]);
        loop {
            core::hint::spin_loop();
        }
    }
    #[panic_handler]
    fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
        let _ = writeln!(Uart, "[mb] FAIL: {info}");
        shutdown()
    }
}
