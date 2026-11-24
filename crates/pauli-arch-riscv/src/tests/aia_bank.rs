//! Original independent-VSIE bank oracle against production transfer code.
//! Ownership/Lock Ordering: one test owns the modeled physical bank.
//! Invariants: HVIEN transition may destroy enables; restoration order matters.
use crate::{
    VCpuContext,
    aia::{
        self,
        bank::{self, Register, Registers},
    },
};
struct Modeled;
impl Registers for Modeled {
    fn read(&mut self, register: Register) -> usize {
        read(register as usize)
    }
    fn write(&mut self, register: Register, value: usize) {
        write(register as usize, value);
    }
}
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bank {
    words: [usize; 5],
    hie: usize,
    hideleg: usize,
    independent: usize,
}
const POISON: Bank = Bank {
    words: [1 << 13, (1 << 30) | (9 << 16), 0x88, 0x0700_0300, 0x0908],
    hie: 0x444,
    hideleg: 0x444,
    independent: 1 << 13,
};
static BANK: Mutex<Bank> = Mutex::new(POISON);
static READS: AtomicUsize = AtomicUsize::new(0);
static WRITES: AtomicUsize = AtomicUsize::new(0);
fn read(index: usize) -> usize {
    READS.fetch_add(1, Ordering::Relaxed);
    let b = BANK.lock().unwrap();
    if index == 5 {
        ((b.hie & b.hideleg & 0x444) >> 1) | (b.independent & b.words[0] & !b.hideleg)
    } else {
        b.words[index]
    }
}
fn write(index: usize, value: usize) {
    WRITES.fetch_add(1, Ordering::Relaxed);
    let mut b = BANK.lock().unwrap();
    if index == 5 {
        b.hie = (b.hie & !(b.hideleg & 0x444)) | ((value << 1) & b.hideleg & 0x444);
        b.independent = value & b.words[0] & !b.hideleg & !0x1fff;
    } else {
        if index == 0 {
            b.independent &= !(value & !b.words[0]);
        }
        b.words[index] = value;
    }
}
fn reset() {
    *BANK.lock().unwrap() = POISON;
    READS.store(0, Ordering::Relaxed);
    WRITES.store(0, Ordering::Relaxed);
}
fn counts() -> (usize, usize) {
    (
        READS.load(Ordering::Relaxed),
        WRITES.load(Ordering::Relaxed),
    )
}
fn guest_write(selector: usize) {
    // Fixture-only execution, not charged to the production host CSR counts.
    let mut b = BANK.lock().unwrap();
    assert_eq!(b.words[0..2], [0, aia::NEUTRAL_CONTROL]);
    assert_eq!(b.words[3..5], [0, 0]);
    b.words[2] = selector;
    b.hie = 0; // The ordinary guest entry writes VSIE through base-H aliases.
}
fn base_put() {
    BANK.lock().unwrap().hie = POISON.hie;
}
#[test]
fn host_bank_isolation() {
    let mut ctx = VCpuContext {
        aia: aia::GuestState::new(),
        ..VCpuContext::new()
    };

    assert!(bank::load(&mut Modeled, false, ctx.aia, false).is_none());
    assert_eq!(counts(), (0, 0));

    let saved = { bank::load(&mut Modeled, true, ctx.aia, false) }.unwrap();
    assert_eq!(BANK.lock().unwrap().words[2], 0);
    guest_write(0xa5);
    {
        {
            ctx.aia = bank::snapshot(&mut Modeled);
        }
    };
    base_put();
    {
        saved.restore(&mut Modeled)
    };
    assert_eq!(*BANK.lock().unwrap(), POISON);
    assert_eq!(counts(), (7, 11));
    let saved = { bank::load(&mut Modeled, true, ctx.aia, false) }.unwrap();
    assert_eq!(BANK.lock().unwrap().words[2], 0xa5);
    base_put();
    {
        saved.restore(&mut Modeled)
    };
    let peer = VCpuContext {
        aia: aia::GuestState::new(),
        ..VCpuContext::new()
    };
    let saved = { bank::load(&mut Modeled, true, peer.aia, false) }.unwrap();
    assert_eq!(BANK.lock().unwrap().words[2], 0);
    base_put();
    {
        saved.restore(&mut Modeled)
    };
    assert_eq!(*BANK.lock().unwrap(), POISON);
    ctx = VCpuContext {
        aia: aia::GuestState::new(),
        ..VCpuContext::new()
    };
    let saved = { bank::load(&mut Modeled, true, ctx.aia, false) }.unwrap();
    assert_eq!(BANK.lock().unwrap().words[2], 0);
    base_put();
    {
        saved.restore(&mut Modeled)
    };
    resident_exits(&mut ctx);
}
fn resident_exits(ctx: &mut VCpuContext) {
    for resident in [false, true] {
        reset();
        let mut saved = None;
        for _ in 0..16 {
            if !resident || saved.is_none() {
                saved = bank::load(&mut Modeled, true, ctx.aia, false);
            }
            guest_write(0x98);
            {
                {
                    ctx.aia = bank::snapshot(&mut Modeled);
                }
            };
            if !resident {
                base_put();
                {
                    saved.take().unwrap().restore(&mut Modeled)
                };
            }
        }
        if let Some(saved) = saved {
            base_put();
            {
                saved.restore(&mut Modeled)
            };
        }
        assert_eq!(*BANK.lock().unwrap(), POISON);
        assert_eq!(counts(), if resident { (22, 11) } else { (112, 176) });
    }
    reset();
    let saved = { bank::load(&mut Modeled, true, ctx.aia, true) }.unwrap();
    assert_eq!(BANK.lock().unwrap().words[1], aia::NEUTRAL_CONTROL | 1);
    {
        bank::set_external(&mut Modeled, false);
    }
    assert_eq!(BANK.lock().unwrap().words[1], aia::NEUTRAL_CONTROL);
    {
        bank::set_external(&mut Modeled, true);
    }
    assert_eq!(BANK.lock().unwrap().words[1], aia::NEUTRAL_CONTROL | 1);
    base_put();
    {
        saved.restore(&mut Modeled);
    }
    assert_eq!(*BANK.lock().unwrap(), POISON);
    std::println!(
        "AIA context PASS: absent hardware makes zero CSR accesses; host/peer/reset selection and independent VSIE restore; 16 resident exits use 33 vs 288 modeled CSR operations (not latency)"
    );
}
