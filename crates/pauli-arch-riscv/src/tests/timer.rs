//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::timer;
use std::cell::RefCell;
use std::thread_local;
use timer::{PhysicalTimer, VirtualTimer};

#[derive(Default)]
struct Hardware {
    now: u64,
    compare: Option<u64>,
    calls: u64,
    software_pending: bool,
    // A failed firmware call may already have changed its endpoint.
    fail: bool,
}
impl Hardware {
    fn pending(&self) -> bool {
        self.compare
            .is_some_and(|deadline| deadline != u64::MAX && self.now >= deadline)
    }
}
thread_local! {
    static HW: RefCell<Hardware> = RefCell::new(Hardware::default());
}

// Only the firmware boundary is replaced; the production PhysicalTimer and
// its Cache are compiled unchanged, including error propagation/commit rules.
fn endpoint(deadline: u64) -> Result<(), ()> {
    HW.with_borrow_mut(|hw| {
        hw.calls += 1;
        hw.compare = Some(deadline);
        if core::mem::take(&mut hw.fail) {
            Err(())
        } else {
            Ok(())
        }
    })
}
fn reset() {
    HW.with_borrow_mut(|hw| *hw = Hardware::default());
}
fn time(now: u64) {
    HW.with_borrow_mut(|hw| hw.now = now);
}
fn pending() -> bool {
    HW.with_borrow(Hardware::pending)
}
fn program(owner: &mut PhysicalTimer, deadline: u64) {
    owner.program_with(deadline, endpoint).unwrap();
    HW.with_borrow(|hw| {
        assert_eq!(hw.compare, Some(deadline));
        assert_eq!(owner.statistics().writes, hw.calls);
    });
}
#[test]
fn absolute_budget() {
    reset();
    let mut owner = PhysicalTimer::new();
    let guest = VirtualTimer::new();
    for now in 0..=101 {
        time(now);
        program(&mut owner, guest.physical_deadline(100));
        assert_eq!(pending(), now >= 100);
    }
    assert_eq!(owner.statistics().writes, 1);
    assert_eq!(owner.statistics().reuses, 101);
    owner.disarm_with(endpoint).unwrap();
    assert!(!pending());
}
#[test]
fn guests_and_idle() {
    reset();
    let mut owner = PhysicalTimer::new();
    let mut a = VirtualTimer::new();
    let mut b = VirtualTimer::new();
    a.set_deadline(20, 0);
    program(&mut owner, a.physical_deadline(100));
    time(20);
    assert!(pending());
    // Reusing an expired compare must not clear or renew its pending STIP.
    program(&mut owner, 20);
    assert!(pending());
    a.refresh(20);
    assert!(a.pending());
    program(&mut owner, a.physical_deadline(100));
    assert!(!pending());
    assert!(a.pending(), "host arm must not acknowledge virtual STIP");
    b.set_deadline(60, 20);
    program(&mut owner, b.physical_deadline(100));
    a.set_deadline(80, 20);
    // Earliest enabled waiter, not the compare left by the last selected VM.
    let idle = a.deadline().unwrap().min(b.deadline().unwrap());
    for phase in 0..3 {
        HW.with_borrow_mut(|hw| hw.software_pending = phase == 0);
        if phase == 1 {
            HW.with_borrow_mut(|hw| hw.software_pending = true);
        }
        program(&mut owner, idle);
        if phase == 2 {
            HW.with_borrow_mut(|hw| hw.software_pending = true);
        }
        assert!(
            HW.with_borrow(|hw| hw.software_pending),
            "TIME must not clear SSIP"
        );
    }
    time(60);
    b.refresh(60);
    assert!(b.pending() && !a.pending());
    // Cold VM restart discards A's virtual deadline, not B's pending state.
    a = VirtualTimer::new();
    program(&mut owner, a.physical_deadline(120));
    assert!(!pending() && !a.pending() && b.pending());
    owner.disarm_with(endpoint).unwrap();
    assert!(!pending());
    assert_eq!(timer::VSTIP, 1 << 6);
    assert_eq!(timer::VS_STIP, 1 << 5);
}
#[test]
fn failures_and_handoff() {
    reset();
    let mut owner = PhysicalTimer::new();
    program(&mut owner, 80);
    HW.with_borrow_mut(|hw| hw.fail = true);
    assert!(owner.program_with(40, endpoint).is_err());
    // Failure may already have changed the hardware. Neither the old deadline
    // nor the failed new deadline may be treated as a known cached success.
    program(&mut owner, 80);
    assert_eq!(owner.statistics().writes, 3);
    HW.with_borrow_mut(|hw| hw.fail = true);
    assert!(owner.program_with(40, endpoint).is_err());
    program(&mut owner, 40);
    assert_eq!(owner.statistics().writes, 5);
    owner.disarm_with(endpoint).unwrap();
    let calls = HW.with_borrow(|hw| hw.calls);
    // A new physical ownership lifetime starts unknown, including disarm.
    let next = PhysicalTimer::new();
    let statistics = next.disarm_with(endpoint).unwrap();
    assert_eq!(HW.with_borrow(|hw| hw.calls), calls + 1);
    assert_eq!(statistics.writes, 1);
}
#[test]
fn exhaustive() {
    let sequences = 6usize.pow(6);
    for sequence in 0..sequences {
        reset();
        let mut owner = PhysicalTimer::new();
        let mut known = None;
        let mut encoded = sequence;
        for _ in 0..6 {
            let action = encoded % 6;
            encoded /= 6;
            if action == 4 {
                HW.with_borrow_mut(|hw| hw.now += 2);
                continue;
            }
            let deadline = match action {
                0 => 0,
                1 => 3,
                2 => 9,
                3 => u64::MAX,
                5 => 7,
                _ => unreachable!(),
            };
            let reuse = known == Some(deadline);
            HW.with_borrow_mut(|hw| hw.fail = action == 5);
            let calls = HW.with_borrow(|hw| hw.calls);
            let was_pending = pending();
            let result = owner.program_with(deadline, endpoint);
            if reuse {
                assert!(result.is_ok());
                assert_eq!(HW.with_borrow(|hw| hw.calls), calls);
                assert_eq!(pending(), was_pending);
            } else if action == 5 {
                assert!(result.is_err());
                known = None;
            } else {
                assert!(result.is_ok());
                known = Some(deadline);
            }
            HW.with_borrow(|hw| {
                assert_eq!(owner.statistics().writes, hw.calls);
                if let Some(expected) = known {
                    assert_eq!(hw.compare, Some(expected));
                }
            });
        }
    }
    std::println!(
        "HV physical timer PASS: {sequences} sequences, absolute budget, expired STIP, guest/idle isolation, failure and ownership handoff"
    );
}

#[test]
fn sstc_and_firmware_waiters_follow_their_own_compare() {
    use crate::{VCpuContext, bits};

    let mut ctx = VCpuContext::new();
    let mut timer = VirtualTimer::new();
    timer.set_deadline(9, 0);
    ctx.vsie = bits::STIP;
    assert_eq!(ctx.wait_timer_deadline(&timer), Some(9));
    assert!(ctx.refresh_timer(&mut timer, 9));
    ctx.sstc_enabled = 1;
    assert_eq!(ctx.wait_timer_deadline(&timer), None);
    assert!(!ctx.refresh_timer(&mut timer, 9));
    ctx.vstimecmp = 19;
    assert_eq!(ctx.wait_timer_deadline(&timer), Some(19));
    assert!(!ctx.refresh_timer(&mut timer, 18));
    assert!(ctx.refresh_timer(&mut timer, 19));
    ctx.vsie = 0;
    assert_eq!(ctx.wait_timer_deadline(&timer), None);
    ctx.vstimecmp = u64::MAX;
    assert!(!ctx.refresh_timer(&mut timer, u64::MAX - 1));
    assert!(ctx.refresh_timer(&mut timer, u64::MAX));
}

#[test]
fn sstc_context_capability_gating_and_initial_deadline() {
    use crate::{Error, VCpuContext, init_sstc, sstc_available};

    init_sstc(false);
    assert!(!sstc_available());
    let mut ctx = VCpuContext::new();
    assert!(!ctx.sstc_enabled());
    assert_eq!(ctx.vstimecmp, u64::MAX);
    assert_eq!(ctx.enable_sstc(), Err(Error::Unsupported));
    assert!(!ctx.sstc_enabled());

    init_sstc(true);
    assert!(sstc_available());
    assert_eq!(ctx.enable_sstc(), Ok(()));
    assert!(ctx.sstc_enabled());
    assert_eq!(ctx.vstimecmp, u64::MAX);
    init_sstc(false);
}
