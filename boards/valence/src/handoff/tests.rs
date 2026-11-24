//! No native pointers or board devices in these tests. The optional release
//! preflight calls the same validator as HS with the packager's real DTB.
use super::*;

#[test]
fn extent_must_fit_even_for_an_aligned_header_near_the_end() {
    assert!(check_extent(HANDOFF_START, FDT_HEADER).is_ok());
    assert!(check_extent(HANDOFF_END - FDT_HEADER, FDT_HEADER).is_ok());
    for (address, size) in [
        (RAM_BASE, FDT_HEADER),
        (HANDOFF_START + 1, FDT_HEADER),
        (HANDOFF_START, FDT_HEADER - 1),
        (HANDOFF_END - 8, FDT_HEADER),
        (HANDOFF_START, usize::MAX),
    ] {
        assert_eq!(check_extent(address, size), Err(HvError::InvalidImage));
    }
}

#[test]
#[ignore = "set WAVE8_BUNDLE to a local Wave8 release directory"]
fn packaged_handoff() {
    let dir = std::path::PathBuf::from(std::env::var_os("WAVE8_BUNDLE").expect("WAVE8_BUNDLE"));
    for file in ["ooo-busybox.dtb", "ooo-systemd.dtb"] {
        let bytes = std::fs::read(dir.join(file)).unwrap();
        assert_eq!(validate_blob(0, &bytes), Ok(None), "{file}");
        assert_eq!(validate_blob(1, &bytes), Err(HvError::Unsupported));
    }
    let other = std::fs::read(dir.join("inorder-busybox.dtb")).unwrap();
    assert_eq!(validate_blob(0, &other), Err(HvError::Unsupported));

    // A matching compatible string alone must not grant the static RAM or
    // IRQ resources. Touch one field in a private copy, never the input file.
    let original = std::fs::read(dir.join("ooo-busybox.dtb")).unwrap();
    let tree = Fdt::new(&original).unwrap();
    for (path, key, bad, error) in [
        (
            "/cpus",
            "timebase-frequency",
            40_000_000u32.to_be_bytes().to_vec(),
            HvError::Unsupported,
        ),
        (
            "/chosen",
            "linux,initrd-start",
            (RAM_BASE as u64).to_be_bytes().to_vec(),
            HvError::InvalidMemory,
        ),
        (
            "/soc/serial@10000000",
            "interrupt-parent",
            0u32.to_be_bytes().to_vec(),
            HvError::InvalidDevice,
        ),
    ] {
        let field = node(tree, path).unwrap().property(key).unwrap().unwrap();
        let start = field.as_ptr() as usize - original.as_ptr() as usize;
        let mut changed = original.clone();
        changed[start..start + field.len()].copy_from_slice(&bad);
        assert_eq!(validate_blob(0, &changed), Err(error), "{key}");
    }
}
