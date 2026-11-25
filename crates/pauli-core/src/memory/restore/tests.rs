//! Byte-level oracle for cold restores, including misalignment and peer rejection.
//! Ownership/Lock Ordering: each closure holds one exclusive test allocation loan.
//! Invariants: no byte outside the assignment changes, even on a rejected image.
use super::*;
use pauli_devices::memory::GuestMemory;

type Parts = [(u64, &'static [u8]); 3];
const GPA: u64 = 0x4000;
const LEN: usize = 97;

fn with_ram(backing: &mut [u8], f: impl FnOnce(&mut GuestRam)) {
    let range = PhysRange::new(backing.as_mut_ptr() as usize, backing.len()).unwrap();
    // SAFETY: exclusive loan spans the callback; no native CPU or DMA sees this range.
    let mut ram = unsafe { GuestRam::new(range, GPA as usize) }.unwrap();
    f(&mut ram);
}

fn check_layout(skew: usize, parts: Parts) {
    let mut backing = [0xa5; LEN + 32];
    let mut expected = [0; LEN];
    let mut copied = 0;
    for (offset, data) in parts {
        let start = offset as usize;
        expected[start..start + data.len()].copy_from_slice(data);
        copied += data.len();
    }
    with_ram(&mut backing[skew..skew + LEN], |ram| {
        let image = ram.plan_image(parts).unwrap();
        let zeros = image.spans.iter().map(|(gap, _)| *gap).sum::<usize>();
        let copies = image
            .spans
            .iter()
            .map(|(_, data)| data.len())
            .sum::<usize>();
        assert_eq!((zeros, copies), (LEN - copied, copied));
        for dirty in [0x36, 0xe9] {
            assert!(ram.write(GPA, &[dirty; LEN]));
            // SAFETY: private fixture has no runners or device producers in either epoch.
            unsafe { ram.restore_retired(&image) }.unwrap();
            let mut actual = [0xff; LEN];
            assert!(ram.read(GPA, &mut actual));
            assert_eq!(actual, expected);
        }
    });
    assert!(backing[..skew].iter().all(|&b| b == 0xa5));
    assert!(backing[skew + LEN..].iter().all(|&b| b == 0xa5));
}

#[test]
fn every_gap_and_template_is_written_exactly_once() {
    for skew in 1..=16 {
        for parts in [
            [(0, &[][..]), (0, &[]), (0, &[])],
            [(0, &[0x87; LEN]), (0, &[]), (0, &[])],
            [(7, b"kernel"), (48, &[]), (94, b"dtb")],
            [(0, b"kernel"), (6, b"initrd"), (12, b"dtb")],
            [
                (3, b"kernel"),
                (37, &b"x-unaligned-template"[1..]),
                (86, b"dtb"),
            ],
        ] {
            check_layout(skew, parts);
        }
    }
}

#[test]
fn invalid_layout_never_changes_a_prefix() {
    let mut backing = [0x53; LEN];
    with_ram(&mut backing, |ram| {
        for parts in [
            [(12, &b"kernel"[..]), (17, b"overlap"), (40, b"dtb")],
            [(30, b"kernel"), (0, &[]), (8, b"backwards")],
            [(1, b"kernel"), (32, b"initrd"), (LEN as u64, b"dtb")],
            [(0, b"kernel"), (1, &[]), (u64::MAX, b"overflow")],
            [(0, b"kernel"), (LEN as u64 + 1, &[]), (80, b"dtb")],
        ] {
            assert!(matches!(ram.plan_image(parts), Err(HvError::InvalidImage)));
        }
    });
    assert_eq!(backing, [0x53; LEN]);
}

#[test]
fn same_size_peer_with_identical_gpa_rejects_foreign_plan() {
    let mut left = [0x51; LEN];
    let mut right = [0x72; LEN];
    with_ram(&mut left, |ram| {
        let image = ram
            .plan_image([(2, b"kernel"), (0, &[]), (0, &[])])
            .unwrap();
        with_ram(&mut right, |peer| {
            // SAFETY: peer too is retired; assignment identity is checked by restore.
            assert_eq!(
                unsafe { peer.restore_retired(&image) },
                Err(HvError::InvalidMemory)
            );
        });
    });
    assert_eq!(left, [0x51; LEN]);
    assert_eq!(right, [0x72; LEN]);
}

#[test]
fn aligned_and_unaligned_bulk_volatile_ops_match_byte_for_byte() {
    let mut dst_buf = [0xaa_u8; 4128];
    let mut src_buf = [0_u8; 4128];
    for (i, byte) in src_buf.iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    let lengths: [usize; 18] = [
        0, 1, 2, 7, 8, 9, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 129, 4096,
    ];
    for dst_align in 0..8 {
        for len in lengths {
            let start = 8 + dst_align;
            dst_buf.fill(0xaa);
            // SAFETY: exclusive test buffer with 8-byte guards on both sides.
            unsafe { zero_volatile(dst_buf.as_mut_ptr().add(start) as usize, len) };
            assert!(dst_buf[..start].iter().all(|&b| b == 0xaa));
            assert!(dst_buf[start..start + len].iter().all(|&b| b == 0));
            assert!(dst_buf[start + len..].iter().all(|&b| b == 0xaa));

            for src_align in 0..8 {
                let s_start = 8 + src_align;
                dst_buf.fill(0xaa);
                // SAFETY: exclusive disjoint test buffers with guards on both sides.
                unsafe {
                    copy_volatile(
                        dst_buf.as_mut_ptr().add(start) as usize,
                        src_buf.as_ptr().add(s_start),
                        len,
                    );
                }
                assert!(dst_buf[..start].iter().all(|&b| b == 0xaa));
                assert_eq!(
                    &dst_buf[start..start + len],
                    &src_buf[s_start..s_start + len]
                );
                assert!(dst_buf[start + len..].iter().all(|&b| b == 0xaa));
            }
        }
    }
}
