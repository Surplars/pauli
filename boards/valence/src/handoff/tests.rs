//! No native pointers or board devices in these tests. The optional release
//! preflight calls the same validator as HS with the packager's real DTB.
use super::*;

/// Small FDT writer shared with the memory-grant tests. Properties are supplied
/// as raw bytes so tests can represent duplicate names and malformed cells.
pub(super) mod blob {
    #[derive(Default)]
    pub(crate) struct Tokens {
        data: Vec<u8>,
        strings: Vec<u8>,
    }
    impl Tokens {
        pub(crate) fn tag(&mut self, word: u32) {
            self.data.extend(word.to_be_bytes());
        }
        fn pad(&mut self) {
            self.data.resize(self.data.len().next_multiple_of(4), 0);
        }
        pub(crate) fn begin(&mut self, name: &str, props: &[(&str, Vec<u8>)]) {
            self.tag(1);
            self.data.extend(name.as_bytes());
            self.data.push(0);
            self.pad();
            for (name, raw) in props {
                self.tag(3);
                self.tag(raw.len() as u32);
                self.tag(self.strings.len() as u32);
                self.strings.extend(name.as_bytes());
                self.strings.push(0);
                self.data.extend(raw);
                self.pad();
            }
        }
        pub(crate) fn finish(self, reserve_map: &[u8]) -> Vec<u8> {
            let structure = 40 + reserve_map.len() + 16;
            let strings = structure + self.data.len();
            let header = [
                0xd00d_feed,
                (strings + self.strings.len()) as u32,
                structure as u32,
                strings as u32,
                40,
                17,
                16,
                0,
                self.strings.len() as u32,
                self.data.len() as u32,
            ];
            let mut out: Vec<_> = header.into_iter().flat_map(u32::to_be_bytes).collect();
            out.extend(reserve_map);
            out.extend([0; 16]);
            out.extend(self.data);
            out.extend(self.strings);
            out
        }
    }
}

const BITS: &str = "riscv,guest-index-bits";
const IDS: &str = "riscv,num-guest-ids";
fn word(value: u32) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}
fn window(base: u64, size: u64) -> Vec<u8> {
    [base.to_be_bytes(), size.to_be_bytes()].concat()
}
fn geometry(reg: Vec<u8>, extra: &[(&str, Vec<u8>)]) -> fdt::Result<Option<usize>> {
    let mut w = blob::Tokens::default();
    w.begin("", &[]);
    w.begin(
        "soc",
        &[("#address-cells", word(2)), ("#size-cells", word(2))],
    );
    let mut props = vec![("reg", reg), ("riscv,num-ids", word(127))];
    props.extend_from_slice(extra);
    w.begin("imsic", &props);
    for tag in [2, 2, 2, 9] {
        w.tag(tag);
    }
    let dtb = w.finish(&[]);
    let fdt = Fdt::new(&dtb)?;
    imsic_guest_ids(&node(fdt, "/soc")?, &node(fdt, "/soc/imsic")?)
}

#[test]
fn guest_file_geometry_and_virtual_layout_agree() {
    for (size, props, expected, virtual_ids) in [
        (4096, vec![], None, ImsicIds::Ids127),
        (4096, vec![(BITS, word(0))], None, ImsicIds::Ids127),
        (16384, vec![(BITS, word(2))], Some(127), ImsicIds::Ids127),
        (
            16384,
            vec![(BITS, word(2)), (IDS, word(255))],
            Some(255),
            ImsicIds::Ids255,
        ),
        (
            4096 << 7,
            vec![(BITS, word(7)), (IDS, word(127))],
            Some(127),
            ImsicIds::Ids127,
        ),
    ] {
        let guest_ids = geometry(window(S_IMSIC, size), &props).unwrap();
        assert_eq!(guest_ids, expected);
        assert_eq!(
            Handoff {
                external: None,
                guest_ids
            }
            .layout()
            .imsic_ids,
            virtual_ids
        );
    }
}

#[test]
fn guest_file_rejects_ambiguous_cells_and_insufficient_apertures() {
    let reg = window(S_IMSIC, 16384);
    for props in [
        vec![(BITS, word(8))],
        vec![(BITS, word(u32::MAX))],
        vec![(IDS, word(127))],
        vec![(BITS, word(0)), (IDS, word(255))],
        vec![(BITS, word(2)), (IDS, word(63))],
        vec![(BITS, word(2)), (IDS, word(256))],
        vec![(BITS, vec![2])],
        vec![(BITS, word(2)), (IDS, vec![])],
        vec![(BITS, word(2)), (BITS, word(2))],
        vec![(BITS, word(2)), (IDS, word(127)), (IDS, word(255))],
        vec![("reg", reg.clone())],
        vec![("riscv,num-ids", word(255))],
        vec![("riscv,group-index-bits", word(1))],
    ] {
        assert!(geometry(reg.clone(), &props).is_err(), "{props:?}");
    }
    for reg in [
        window(S_IMSIC, 16383),
        window(S_IMSIC, u64::MAX),
        window(S_IMSIC + 4096, 16384),
        vec![0; 15],
        [reg.clone(), reg].concat(),
    ] {
        assert!(geometry(reg, &[(BITS, word(2))]).is_err());
    }
}

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
        assert_eq!(validate_blob(0, &bytes).unwrap().external, None, "{file}");
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
