//! Tiny synthetic RV64 program headers exercise the host ELF input boundary.
//! Ownership/Lock Ordering: each test owns its byte array; no filesystem or locks.
//! Invariants: malformed permissions/geometry cannot reach staging.
use super::*;
pub(crate) const BASE: u64 = 0x8020_0000;
const PH: usize = 64;
const STRIDE: usize = 56;
pub(crate) fn example() -> Vec<u8> {
    let mut elf = vec![0; 256];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&243u16.to_le_bytes());
    elf[24..32].copy_from_slice(&BASE.to_le_bytes());
    elf[32..40].copy_from_slice(&(PH as u64).to_le_bytes());
    elf[54..56].copy_from_slice(&(STRIDE as u16).to_le_bytes());
    elf[56..58].copy_from_slice(&3u16.to_le_bytes());
    for (index, flags) in [5u32, 4, 6].into_iter().enumerate() {
        let address = BASE + index as u64 * FIRMWARE_PAGE;
        let h = &mut elf[PH + index * STRIDE..][..STRIDE];
        h[..4].copy_from_slice(&1u32.to_le_bytes());
        h[4..8].copy_from_slice(&flags.to_le_bytes());
        for (pos, value) in [
            (8, 240 + index as u64),
            (16, address),
            (24, address),
            (32, 1),
            (40, 128),
        ] {
            h[pos..pos + 8].copy_from_slice(&value.to_le_bytes());
        }
        elf[240 + index] = 0xa0 + index as u8;
    }
    elf
}
#[test]
fn flat_template_omits_bss_and_keeps_permissions() {
    let (bytes, memory, kind) = load(&example(), BASE).unwrap();
    assert_eq!(
        kind,
        FirmwareLayout {
            rx_pages: 1,
            ro_pages: 1
        }
    );
    assert_eq!(memory, 8192 + 128);
    assert_eq!(bytes.len(), 8193);
    assert_eq!([bytes[0], bytes[4096], bytes[8192]], [0xa0, 0xa1, 0xa2]);
    assert!(bytes[1..4096].iter().all(|&b| b == 0));
}
#[test]
fn reject_permissions_overlap_dynamic_images_and_truncation() {
    let elf = example();
    assert!(load(&elf, BASE + 4).is_err());
    for prefix in 0..243 {
        assert!(load(&elf[..prefix], BASE).is_err());
    }
    for (offset, value) in [
        (PH + 4, 7u32),
        (48, 4),
        (PH, 2),
        (PH, 3),
        (PH + 2 * STRIDE + 4, 5),
        (PH + STRIDE + 16, BASE as u32),
        (PH + STRIDE + 32, 255),
    ] {
        let mut bad = elf.clone();
        bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert!(load(&bad, BASE).is_err());
    }
    for address in [BASE + 16, BASE + 256, BASE + MAX_FIRMWARE_RAM] {
        let mut bad = elf.clone();
        for offset in [16, 24] {
            let i = PH + STRIDE + offset;
            bad[i..i + 8].copy_from_slice(&address.to_le_bytes());
        }
        assert!(load(&bad, BASE).is_err());
    }
}
