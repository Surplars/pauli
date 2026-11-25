//! Production algorithm acceptance, using private host fixtures.
//! Ownership/Lock Ordering: each test owns its state; no actual CSR access.
//! Invariants: retain the original protocol assertions and rejection cases.
use crate::aia::{emulation, imsic, virtualized};
use emulation::{CsrAccess, decode};
use virtualized::VirtualAia;
fn indirect(vm: &mut VirtualAia, cpu: usize, select: usize, value: Option<usize>) -> usize {
    vm.csr(
        cpu,
        CsrAccess {
            register: 0x151,
            select,
            clear: usize::MAX,
            set: value.unwrap_or(0),
            write: value.is_some(),
        },
    )
    .unwrap()
    .0
}
pub(super) fn top(vm: &mut VirtualAia, cpu: usize, write: bool) -> (usize, u32) {
    vm.csr(
        cpu,
        CsrAccess {
            register: 0x15c,
            select: 0,
            clear: 0,
            set: 0,
            write,
        },
    )
    .unwrap()
}
pub(super) fn config(vm: &mut VirtualAia, cpu: usize) {
    for word in 0..4 {
        indirect(vm, cpu, 0xc0 + word * 2, Some(usize::MAX));
    }
    indirect(vm, cpu, 0x70, Some(1));
}
pub(super) fn write(vm: &mut VirtualAia, offset: usize, value: usize) {
    assert_eq!(vm.aplic_access(offset, Some(value)), Some(0));
}
#[test]
fn controller_acceptance() {
    let mut a = VirtualAia::new(2);
    let mut peer = VirtualAia::new(2);
    for cpu in 0..2 {
        config(&mut a, cpu);
        config(&mut peer, cpu);
    }
    wired(&mut a, &mut peer);
    edge_modes();
    targets_and_reset(&mut a, &mut peer);
    all_ids();
    csr_operations(&mut a);
    std::println!(
        "Virtual AIA PASS: all 255 IDs, 64-bit banks, atomic CSR/TOPEI, threshold/delivery, level EOI without duplicate MSI, VM/hart/target/peer/reset isolation"
    );
}

#[test]
fn highest_interrupt_context_is_private_in_both_controllers() {
    use crate::{interrupt::VirtualInterruptController, plic::VirtualPlic};
    let mut plic = VirtualPlic::with_contexts(16);
    let high = 0x20_0004 + 15 * 0x1000;
    plic.write_word(10 * 4, 3).unwrap();
    plic.write_word(0x2000 + 15 * 0x80, 1 << 10).unwrap();
    plic.set_level(10, true);
    assert!(!plic.external_pending_for(0) && plic.external_pending_for(15));
    assert_eq!(plic.read_word(high), Some(10));
    plic.write_word(0x20_0004, 10).unwrap();
    assert_eq!(plic.read_word(high), Some(0)); // A different context did not rearm the level.
    plic.write_word(high, 10).unwrap();
    assert_eq!(plic.read_word(high), Some(10));
    assert_eq!(plic.read_word(high + 0x1000), None);

    let mut aia = VirtualAia::new(16);
    let mut peer = VirtualAia::new(16);
    config(&mut aia, 0);
    config(&mut aia, 15);
    config(&mut peer, 15);
    let file = aia.take_file(15);
    assert_eq!(aia.msi_access(15 * 4096, Some(63)), Some(0));
    assert_eq!(aia.msi_access(16 * 4096, Some(63)), None);
    assert!(!aia.line(0) && aia.line(15) && !peer.line(15));
    aia.put_file(15, file); // Restore must merge a delivery that raced the physical save.
    assert_eq!(top(&mut aia, 15, true).0, (63 << 16) | 63);
    aia.msi_access(15 * 4096, Some(42)).unwrap();
    peer.msi_access(15 * 4096, Some(55)).unwrap();
    aia.quiesce();
    assert_eq!(top(&mut aia, 15, true).0, 0);
    assert_eq!(top(&mut peer, 15, true).0, (55 << 16) | 55);
}

#[test]
fn uart_local_timeout() {
    use pauli_devices::uart16550::Uart16550;
    let mut uart = Uart16550::new(10);
    let mut a = VirtualAia::new(2);
    let mut peer = VirtualAia::new(2);
    for cpu in 0..2 {
        config(&mut a, cpu);
        config(&mut peer, cpu);
    }
    for vm in [&mut a, &mut peer] {
        write(vm, 40, 6);
        write(vm, 0x3028, (1 << 18) | 37);
        write(vm, 0x1edc, 10);
        write(vm, 44, 6);
        write(vm, 0x302c, 52);
        write(vm, 0x1edc, 11);
        write(vm, 0, 0x104);
    }
    a.set_level(11, true); // The RTC and other VM remain independent.
    peer.set_level(10, true);
    uart.write(2, 0xc1);
    uart.write(1, 1);
    assert!(uart.receive(b'x'));
    assert!(!uart.irq_pending());
    uart.advance(10);
    a.set_level(10, uart.irq_pending()); // Publish the timeout before RBR.
    assert_eq!(uart.read(0), Some(b'x'));
    a.set_level(10, uart.irq_pending());
    assert_eq!(top(&mut a, 1, true).0, (37 << 16) | 37);
    write(&mut a, 0x2000, 10); // RX is low: EOI must not re-inject it.
    assert_eq!(top(&mut a, 1, true).0, 0);
    assert_eq!(top(&mut a, 0, true).0, (52 << 16) | 52);
    assert_eq!(top(&mut peer, 1, true).0, (37 << 16) | 37);
}

fn wired(a: &mut VirtualAia, peer: &mut VirtualAia) {
    // A wired level routes only to this VM's virtual hart 1, identity 37.
    write(a, 40, 6);
    write(a, 0x3028, (1 << 18) | 37);
    write(a, 0x1edc, 10);
    write(a, 0, 0x104);
    a.set_level(10, true);
    assert!(!a.line(0) && a.line(1) && !peer.line(1));
    assert_eq!(top(a, 1, false), ((37 << 16) | 37, 0));
    assert_eq!(top(a, 1, true), ((37 << 16) | 37, 1 << 10));
    assert!(!a.line(1));
    // Repeated polls of an unchanged held wire MUST NOT send another MSI.
    for _ in 0..128 {
        a.set_level(10, true);
    }
    assert_eq!(a.msis(), 1);
    assert_eq!(top(a, 1, true).0, 0);
    write(a, 0x2000, 10); // Linux's explicit level EOI/re-pend.
    assert_eq!(top(a, 1, true).0, (37 << 16) | 37);
    a.set_level(10, false);
    write(a, 0x2000, 10);
    assert_eq!(top(a, 1, true).0, 0);
    // Inactive source is software-triggerable, while inactive/detached source
    // zero and unimplemented source/hart/guest indices cannot escape the VM.
    write(a, 44, 1);
    write(a, 0x302c, 52);
    write(a, 0x1edc, 11);
    write(a, 0x1cdc, 11);
    assert_eq!(top(a, 0, true).0, (52 << 16) | 52);
}

fn edge_modes() {
    for mode in [4, 5, 6, 7] {
        let mut vm = VirtualAia::new(1);
        config(&mut vm, 0);
        let inactive = mode == 5 || mode == 7;
        vm.set_level(10, inactive);
        write(&mut vm, 40, mode);
        write(&mut vm, 0x3028, 37);
        write(&mut vm, 0x1edc, 10);
        write(&mut vm, 0, 0x104);
        assert_eq!(top(&mut vm, 0, false).0, 0);
        vm.set_level(10, !inactive);
        assert_eq!(top(&mut vm, 0, true).0, 0x250025);
        let sent = vm.msis();
        for _ in 0..64 {
            vm.set_level(10, !inactive);
        }
        assert_eq!(vm.msis(), sent);
        vm.set_level(10, inactive);
        write(&mut vm, 0x2000, 10);
        assert_eq!(
            top(&mut vm, 0, true).0,
            if mode >= 6 { 0 } else { 0x250025 }
        );
        write(&mut vm, 0x1fdc, 10);
        vm.set_level(10, !inactive);
        assert_eq!(top(&mut vm, 0, false).0, 0);
        write(&mut vm, 0x1edc, 10);
        assert_eq!(top(&mut vm, 0, true).0, 0x250025);
    }
}

fn targets_and_reset(a: &mut VirtualAia, peer: &mut VirtualAia) {
    for target in [(2 << 18) | 52, (1 << 12) | 52, 256, usize::MAX] {
        write(a, 0x302c, target);
        write(a, 0x1cdc, 11);
        assert_eq!(top(a, 0, true).0, 0);
        assert_eq!(top(a, 1, true).0, 0);
    }
    assert_eq!(a.msi_access(2 * 4096, Some(1)), None);
    assert_eq!(a.msi_access(8, Some(1)), None);
    assert_eq!(a.aplic_access(3, Some(1)), None);
    assert_eq!(a.aplic_access(0x4000, None), None);
    // Software IPIs arrive via the addressed file, not an unrelated SBI lane.
    a.msi_access(4096, Some(1));
    assert!(a.line(1) && !peer.line(1));
    indirect(a, 1, 0x72, Some(1));
    assert!(!a.line(1));
    indirect(a, 1, 0x72, Some(0));
    assert!(a.line(1));
    indirect(a, 1, 0x70, Some(0));
    assert!(!a.line(1));
    assert_eq!(top(a, 1, true).0, 0x10001); // TOPEI ignores EIDELIVERY.
    assert_eq!(indirect(a, 1, 0xc0, None) & 1, 0);
    for invalid in [0x71, 0x81, 0xc1, 0x100] {
        assert!(
            a.csr(
                1,
                CsrAccess {
                    register: 0x151,
                    select: invalid,
                    clear: 0,
                    set: 0,
                    write: false
                }
            )
            .is_none()
        );
    }
    assert_eq!(indirect(a, 1, 0xfe, Some(usize::MAX)), 0);
    config(a, 0);
    a.msi_access(4, Some(63u32.swap_bytes() as usize));
    assert!(a.line(0));
    let (msis, claims, csrs) = (a.msis(), a.claims, a.csr_accesses);
    a.quiesce();
    assert!(!a.line(0) && !a.line(1));
    assert_eq!((a.msis(), a.claims, a.csr_accesses), (msis, claims, csrs));
    assert_eq!(top(a, 0, false).0, 0);
    // Reset is a fresh complete device owner, not clearing shared host files.
    *a = VirtualAia::new(2);
    assert_eq!(top(a, 1, false).0, 0);
    assert_eq!(a.msis(), 0);
    assert_eq!(a.claims, 0);
    peer.msi_access(0, Some(63));
    assert!(peer.line(0) && !a.line(0));
}

#[test]
fn virtual_geometry_controls_msi_aplic_csr_and_late_inbox() {
    use crate::aia::registers::{EIE, EIP, EITHRESHOLD, GENMSI, TARGET_FIRST};
    use pauli_config::ImsicIds;
    for ids in [ImsicIds::Ids127, ImsicIds::Ids255] {
        let mut vm = VirtualAia::with_ids(1, ids);
        let last = ids.max_id();
        let last_word = 2 * (last / 64);
        config(&mut vm, 0);
        assert_eq!(indirect(&mut vm, 0, EIE, None), usize::MAX & !1);
        assert_eq!(indirect(&mut vm, 0, EIE + last_word, None), usize::MAX);
        for offset in [EIP, EIE] {
            assert_eq!(
                indirect(&mut vm, 0, offset + last_word + 2, Some(usize::MAX)),
                0
            );
            assert_eq!(indirect(&mut vm, 0, offset + last_word + 2, None), 0);
        }
        vm.msi_access(0, Some(last + 1));
        write(&mut vm, GENMSI, last + 1);
        write(&mut vm, TARGET_FIRST, last + 1);
        assert_eq!(vm.aplic_access(TARGET_FIRST, None), Some(0));
        assert_eq!(vm.aplic_access(GENMSI, None), Some(0));
        assert_eq!(top(&mut vm, 0, false).0, 0);
        write(&mut vm, GENMSI, last);
        assert_eq!(indirect(&mut vm, 0, EITHRESHOLD, Some(usize::MAX)), 0);
        assert_eq!(indirect(&mut vm, 0, EITHRESHOLD, None), last);
        assert_eq!(top(&mut vm, 0, false).0, 0);
        indirect(&mut vm, 0, EITHRESHOLD, Some(0));
        assert_eq!(top(&mut vm, 0, true).0, (last << 16) | last);
        let file = vm.take_file(0);
        vm.msi_access(0, Some(last + 1));
        vm.msi_access(0, Some(last));
        vm.put_file(0, file);
        assert_eq!(top(&mut vm, 0, true).0, (last << 16) | last);
        assert_eq!(top(&mut vm, 0, false).0, 0);
        vm.quiesce();
        assert_eq!(vm.take_file(0), imsic::File::with_ids(ids));
    }
}

fn all_ids() {
    // Model every implemented EIP/EIE bit, masking, threshold and priority.
    for id in 1..=255 {
        let mut f = imsic::File::new();
        let select = 0xc0 + (id / 64) * 2;
        f.indirect(select, usize::MAX, 1 << (id % 64), true)
            .unwrap();
        f.inject(id);
        assert_eq!(f.top(), id);
        assert!(!f.line());
        f.indirect(0x70, usize::MAX, 1, true);
        assert!(f.line());
        f.indirect(0x72, usize::MAX, id, true);
        assert!(!f.line());
        f.indirect(0x72, usize::MAX, 0, true);
        assert!(f.line());
        assert_eq!(f.topei(true), (id << 16) | id);
        assert_eq!(f.top(), 0);
    }
}

fn csr_operations(a: &mut VirtualAia) {
    let mut regs = [0; 32];
    regs[0] = usize::MAX;
    regs[6] = 0x25;
    for funct in [1u32, 2, 3, 5, 6, 7] {
        for source in [0u32, 6] {
            for destination in [0u32, 7] {
                csr_case(a, &regs, funct, source, destination);
            }
        }
    }
    // A zero-valued nonzero rs1 still writes/claims, unlike rs1=x0.
    regs[6] = 0;
    assert!(
        decode((0x15c << 20) | (6 << 15) | (2 << 12) | 0x73, &regs, 0)
            .unwrap()
            .0
            .write
    );
    for instruction in [0x10500073, 0x15100073, 0x20401073, 0x15104073, 0] {
        assert!(decode(instruction, &regs, 0).is_none());
    }
}

fn csr_case(a: &mut VirtualAia, regs: &[usize; 32], funct: u32, source: u32, destination: u32) {
    let insn = (0x15cu32 << 20) | (source << 15) | (funct << 12) | (destination << 7) | 0x73;
    let (op, rd) = decode(insn, regs, 0xc0).unwrap();
    assert_eq!(rd, destination as usize);
    assert_eq!(op.write, matches!(funct, 1 | 5) || source != 0);
    let value = if funct & 4 != 0 {
        source as usize
    } else if source == 0 {
        0
    } else {
        0x25
    };
    let expected = match funct & 3 {
        1 => value,
        2 => 0x80 | value,
        _ => 0x80 & !value,
    };
    // Drive the real file, not a copied CSR RMW implementation.
    indirect(a, 0, 0x80, Some(0x80));
    let register_op = CsrAccess {
        register: 0x151,
        select: 0x80,
        ..op
    };
    assert_eq!(a.csr(0, register_op).unwrap().0, 0x80);
    assert_eq!(indirect(a, 0, 0x80, None), expected & !1);
    config(a, 0);
    indirect(a, 0, 0x80, Some(0x80));
    assert_eq!(a.csr(0, op).unwrap().0, 0x70007);
    assert_eq!(top(a, 0, false).0, if op.write { 0 } else { 0x70007 });
}
