//! Generate a minimal Linux newc initramfs, without root tools or an external archive.
//! Ownership/Lock Ordering: build one checked-size archive in host memory.
//! Invariants: names and permissions are fixed; fixture executable and probe mode only.
const DIRECTORY: u32 = 0o040755;
const REGULAR: u32 = 0o100000;
const CONSOLE: u32 = 0o020600;
pub fn archive(elf: &[u8], mode: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for (ino, (name, permissions, data, major, minor)) in [
        ("dev", DIRECTORY, &[][..], 0, 0),
        ("dev/console", CONSOLE, &[], 5, 1),
        ("etc", DIRECTORY, &[], 0, 0),
        ("proc", DIRECTORY, &[], 0, 0),
        ("sys", DIRECTORY, &[], 0, 0),
        ("etc/pauli-probe", REGULAR | 0o644, &[mode], 0, 0),
        ("init", REGULAR | 0o755, elf, 0, 0),
        ("TRAILER!!!", 0, &[], 0, 0),
    ]
    .into_iter()
    .enumerate()
    {
        out.extend_from_slice(b"070701");
        for field in [
            ino as u32 + 1,
            permissions,
            0,
            0,
            1,
            0,
            data.len() as u32,
            0,
            0,
            major,
            minor,
            name.len() as u32 + 1,
            0,
        ] {
            out.extend_from_slice(format!("{field:08x}").as_bytes());
        }
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.resize(out.len().next_multiple_of(4), 0);
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    out
}

#[cfg(test)]
#[test]
fn rtc_initramfs_contains_the_mountpoint_and_the_selected_pid1_mode() {
    let bytes = archive(b"test ELF", b'R');
    let mut cursor = 0;
    let mut entries = std::collections::BTreeMap::new();
    loop {
        let header = &bytes[cursor..cursor + 110];
        assert_eq!(&header[..6], b"070701");
        let field = |at| {
            usize::from_str_radix(std::str::from_utf8(&header[at..at + 8]).unwrap(), 16).unwrap()
        };
        let (mode, size, namesize) = (field(14), field(54), field(94));
        let start = cursor + 110;
        let name = std::ffi::CStr::from_bytes_with_nul(&bytes[start..start + namesize])
            .unwrap()
            .to_str()
            .unwrap();
        let data = (start + namesize).next_multiple_of(4);
        cursor = (data + size).next_multiple_of(4);
        if name == "TRAILER!!!" {
            break;
        }
        assert!(
            entries
                .insert(name, (mode, &bytes[data..data + size]))
                .is_none()
        );
    }
    assert_eq!(cursor, bytes.len());
    assert_eq!(entries["proc"], (DIRECTORY as usize, &[][..]));
    assert_eq!(entries["etc/pauli-probe"].1, b"R");
    assert_eq!(entries["init"], (0o100755, b"test ELF".as_slice()));
}
