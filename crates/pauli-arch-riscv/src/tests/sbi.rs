//! VM-local SBI decode/commit boundaries, without host firmware calls.
//! Ownership/Lock Ordering: private context per test; deferred requests retain it.
//! Invariants: invalid exits have no effects; reset never targets physical harts.
use crate::{
    VCpuContext, VmExit, VmResetKind,
    sbi::{self, SbiAction, SbiError},
};

fn call(eid: usize, fid: usize, args: [usize; 3]) -> VCpuContext {
    let mut ctx = VCpuContext::new();
    ctx.pc = 0x2400;
    ctx.x[10..13].copy_from_slice(&args);
    ctx.x[16] = fid;
    ctx.x[17] = eid;
    ctx
}
fn exit(ctx: &VCpuContext) -> VmExit {
    VmExit {
        cause: VmExit::VS_ECALL,
        from_guest: true,
        pc: ctx.pc,
        value: 0,
        gpa_shifted: 0,
        instruction: 0,
    }
}
#[test]
fn base_extensions_are_honest() {
    for smp in [false, true] {
        for eid in [
            sbi::BASE,
            sbi::TIME,
            sbi::SRST,
            sbi::HSM,
            sbi::IPI,
            sbi::RFENCE,
            0xdead,
        ] {
            let mut ctx = call(sbi::BASE, 3, [eid, 0, 0]);
            let trap = exit(&ctx);
            assert_eq!(
                sbi::handle_ecall_mode(&mut ctx, trap, smp),
                Ok(SbiAction::Resume)
            );
            assert_eq!(ctx.x[10], 0);
            let expected = matches!(eid, sbi::BASE | sbi::TIME | sbi::SRST)
                || (smp && matches!(eid, sbi::HSM | sbi::IPI | sbi::RFENCE));
            assert_eq!(ctx.x[11], usize::from(expected));
            assert_eq!(ctx.pc, 0x2404);
        }
    }
}

#[cfg(feature = "guest-control")]
#[test]
fn management_probe_defers_to_the_board_capacity_without_consuming_the_pc() {
    let mut ctx = call(pauli_core::arch::MANAGEMENT_EID as usize, 0, [0; 3]);
    let trap = exit(&ctx);
    assert_eq!(
        sbi::handle_ecall(&mut ctx, trap),
        Ok(SbiAction::ManagementProbe)
    );
    assert_eq!(ctx.pc, trap.pc);
    ctx.x[15] = 1; // Reserved words must still be zero even for discovery.
    assert_eq!(sbi::handle_ecall(&mut ctx, trap), Ok(SbiAction::Resume));
    assert_eq!(
        (ctx.pc, ctx.x[10]),
        (trap.pc + 4, sbi::INVALID_PARAM as usize)
    );
}
#[test]
fn management_features_reports_protocol_only_and_requires_six_zero_arguments() {
    for nonzero in [None, Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)] {
        let mut ctx = call(0x0800_4f49, 2, [0; 3]);
        if let Some(i) = nonzero {
            ctx.x[10 + i] = usize::MAX;
        }
        let trap = exit(&ctx);
        assert_eq!(sbi::handle_ecall(&mut ctx, trap), Ok(SbiAction::Resume));
        let expected = if !cfg!(feature = "guest-control") {
            (sbi::NOT_SUPPORTED as usize, 0)
        } else if nonzero.is_some() {
            (sbi::INVALID_PARAM as usize, 0)
        } else {
            (0, 3)
        };
        assert_eq!((ctx.x[10], ctx.x[11]), expected);
        assert_eq!(ctx.pc, trap.pc + 4);
    }
}

#[test]
fn direct_events_accept_only_self_take_or_mail_mask_and_commit_once() {
    #[cfg(feature = "guest-control")]
    use pauli_core::arch::EventControl;
    let mut cases = std::vec![[0; 6], [1, 0, 0, 0, 0, 0], [1, 1, 0, 0, 0, 0]];
    for (word, value) in [
        (0, 2),
        (0, usize::MAX),
        (1, 1),
        (2, 1),
        (3, 1),
        (4, 1),
        (5, 1),
    ] {
        let mut args = [0; 6];
        args[word] = value;
        cases.push(args);
    }
    for reserved in [2, 4, 8, 9, usize::MAX] {
        cases.push([1, reserved, 0, 0, 0, 0]);
    }
    for args in cases {
        let mut ctx = call(0x0800_4f49, 3, [0; 3]);
        ctx.x[10..16].copy_from_slice(&args);
        let trap = exit(&ctx);
        let action = sbi::handle_ecall_mode(&mut ctx, trap, true).unwrap();
        #[cfg(feature = "guest-control")]
        if let Some(command) = match args {
            [0, 0, 0, 0, 0, 0] => Some(EventControl::Take),
            [1, mask @ (0 | 1), 0, 0, 0, 0] => Some(EventControl::Mask(mask != 0)),
            _ => None,
        } {
            assert_eq!(action, SbiAction::Events(command));
            assert_eq!(ctx.pc, trap.pc);
            assert_eq!(ctx.x[10..16], args);
            continue;
        }
        assert_eq!(action, SbiAction::Resume);
        let error = if cfg!(feature = "guest-control") {
            sbi::INVALID_PARAM
        } else {
            sbi::NOT_SUPPORTED
        };
        assert_eq!(
            (ctx.x[10] as isize, ctx.x[11], ctx.pc),
            (error, 0, trap.pc + 4)
        );
    }
}

#[test]
fn time_and_rejection_commit_once() {
    let mut ctx = call(sbi::TIME, 0, [usize::MAX, 99, 0]);
    let trap = exit(&ctx);
    assert_eq!(
        sbi::handle_ecall(&mut ctx, trap),
        Ok(SbiAction::SetTimer { deadline: u64::MAX })
    );
    assert_eq!((ctx.pc, ctx.x[10], ctx.x[11]), (0x2404, 0, 0));
    assert_eq!(sbi::handle_ecall(&mut ctx, trap), Err(SbiError::NotVsEcall));
    for (eid, fid) in [(sbi::TIME, 1), (sbi::HSM, 0), (0, 0)] {
        let mut ctx = call(eid, fid, [11, 22, 33]);
        let trap = exit(&ctx);
        assert_eq!(sbi::handle_ecall(&mut ctx, trap), Ok(SbiAction::Resume));
        assert_eq!((ctx.pc, ctx.x[10]), (0x2404, sbi::NOT_SUPPORTED as usize));
    }
}
#[test]
fn invalid_origin_and_overflow_are_side_effect_free() {
    for kind in 0..4 {
        let mut ctx = call(sbi::TIME, 0, [10, 20, 30]);
        let mut trap = exit(&ctx);
        match kind {
            0 => trap.from_guest = false,
            1 => trap.cause = 9,
            2 => trap.pc += 4,
            _ => {
                ctx.pc = usize::MAX - 1;
                trap.pc = ctx.pc;
            }
        }
        let before = (ctx.pc, ctx.x);
        assert!(sbi::handle_ecall_mode(&mut ctx, trap, true).is_err());
        assert_eq!((ctx.pc, ctx.x), before);
    }
}
#[test]
fn deferred_smp_is_not_forwarded_or_consumed() {
    for (eid, fid) in [
        (sbi::HSM, 0),
        (sbi::HSM, 1),
        (sbi::HSM, 2),
        (sbi::IPI, 0),
        (sbi::RFENCE, 0),
        (sbi::RFENCE, 1),
        (sbi::RFENCE, 2),
    ] {
        let mut ctx = call(eid, fid, [3, 0x8000, 0x9000]);
        let trap = exit(&ctx);
        let before = (ctx.pc, ctx.x);
        assert_ne!(
            sbi::handle_ecall_mode(&mut ctx, trap, true),
            Ok(SbiAction::Resume)
        );
        assert_eq!((ctx.pc, ctx.x), before);
    }
    let mut ctx = call(sbi::HSM, 1, [0, 0, 0]);
    ctx.vsstatus = 2;
    let trap = exit(&ctx);
    assert_eq!(
        sbi::handle_ecall_mode(&mut ctx, trap, true),
        Ok(SbiAction::Resume)
    );
    assert_eq!(ctx.x[10], sbi::FAILED as usize);
}
#[test]
fn reset_arguments_are_u32_and_vm_local() {
    for (kind, expected) in [
        (0, SbiAction::Shutdown { reason: 1 }),
        (
            1,
            SbiAction::Reset {
                kind: VmResetKind::Cold,
                reason: 1,
            },
        ),
        (
            2,
            SbiAction::Reset {
                kind: VmResetKind::Warm,
                reason: 1,
            },
        ),
    ] {
        let mut ctx = call(sbi::SRST, 0, [(1 << 32) | kind, (1 << 32) | 1, 0]);
        let trap = exit(&ctx);
        let before = (ctx.pc, ctx.x);
        assert_eq!(sbi::handle_ecall(&mut ctx, trap), Ok(expected));
        assert_eq!((ctx.pc, ctx.x), before);
    }
    for args in [[3, 0, 0], [0, 2, 0]] {
        let mut ctx = call(sbi::SRST, 0, args);
        let trap = exit(&ctx);
        assert_eq!(sbi::handle_ecall(&mut ctx, trap), Ok(SbiAction::Resume));
        assert_eq!(ctx.x[10], sbi::INVALID_PARAM as usize);
    }
}
#[test]
fn masks_validate_all_targets_before_effects() {
    for count in 1..=crate::MAX_VCPUS {
        let all = (1 << count) - 1;
        assert_eq!(sbi::hart_mask(0, usize::MAX, count), Ok(all));
        assert_eq!(sbi::hart_mask(all as usize, 0, count), Ok(all));
        assert_eq!(
            sbi::hart_mask(1 | (1 << count), 0, count),
            Err(sbi::INVALID_PARAM)
        );
        assert_eq!(sbi::hart_mask(0, 0, count), Ok(0));
        assert_eq!(
            sbi::hart_mask(4, usize::MAX - 1, count),
            Err(sbi::INVALID_PARAM)
        );
    }
    assert!(sbi::hart_mask(0, usize::MAX, 0).is_err());
    assert!(sbi::hart_mask(0, usize::MAX, usize::MAX).is_err());
}
