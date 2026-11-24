//! In-memory native DTBs: exercise the complete grant walk without dtc, guest
//! files or physical pointers. All bytes and reserves belong to each test.
use super::*;

type Props = Vec<(&'static str, Vec<u8>)>;

fn tuple(a: usize, b: usize) -> Vec<u8> {
    [(a as u64).to_be_bytes(), (b as u64).to_be_bytes()].concat()
}
fn cells() -> Props {
    vec![
        ("#address-cells", 2u32.to_be_bytes().to_vec()),
        ("#size-cells", 2u32.to_be_bytes().to_vec()),
    ]
}
fn ram_node() -> Props {
    vec![
        ("compatible", b"openion,wave8-guest-ram\0".to_vec()),
        ("no-map", vec![]),
        ("reg", tuple(GUEST_RAM_BASE, GUEST_RAM_SIZE)),
    ]
}

struct Fixture {
    chosen: Props,
    duplicate_chosen: bool,
    nodes: Vec<Props>,
    ddr: Vec<u8>,
    reserve_map: Vec<u8>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            chosen: vec![("openion,guest-image", tuple(IMAGE_BASE, 8192))],
            duplicate_chosen: false,
            nodes: vec![ram_node()],
            ddr: tuple(0x8000_0000, 1usize << 32),
            reserve_map: vec![],
        }
    }
}

// Only the FDT tokens this fixture needs. Structure words are big endian;
// property values remain raw so malformed cell counts are tested unchanged.
#[derive(Default)]
struct Tokens {
    data: Vec<u8>,
    strings: Vec<u8>,
}
impl Tokens {
    fn tag(&mut self, word: u32) {
        self.data.extend(word.to_be_bytes());
    }
    fn pad(&mut self) {
        self.data.resize(self.data.len().next_multiple_of(4), 0);
    }
    fn begin(&mut self, name: &str, props: &Props) {
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
}
impl Fixture {
    fn blob(&self) -> Vec<u8> {
        let mut w = Tokens::default();
        w.begin("", &cells());
        w.begin("chosen", &self.chosen);
        w.tag(2);
        if self.duplicate_chosen {
            w.begin("chosen", &self.chosen);
            w.tag(2);
        }
        w.begin(
            "memory@80000000",
            &vec![
                ("device_type", b"memory\0".to_vec()),
                ("reg", self.ddr.clone()),
            ],
        );
        w.tag(2);
        let mut bus = cells();
        bus.push(("ranges", vec![]));
        w.begin("reserved-memory", &bus);
        for (i, props) in self.nodes.iter().enumerate() {
            w.begin(&format!("region{i}"), props);
            w.tag(2);
        }
        w.tag(2);
        w.tag(2);
        w.tag(9);
        let structure = 40 + self.reserve_map.len() + 16;
        let strings = structure + w.data.len();
        let header = [
            0xd00d_feed,
            (strings + w.strings.len()) as u32,
            structure as u32,
            strings as u32,
            40,
            17,
            16,
            0,
            w.strings.len() as u32,
            w.data.len() as u32,
        ];
        let mut out: Vec<_> = header.into_iter().flat_map(u32::to_be_bytes).collect();
        out.extend(&self.reserve_map);
        out.extend([0; 16]);
        out.extend(w.data);
        out.extend(w.strings);
        out
    }
    fn accepted(&self) -> bool {
        let bytes = self.blob();
        granted(Fdt::new(&bytes).unwrap()).is_ok()
    }
    fn initrd(&mut self, base: usize, end: usize) {
        self.chosen
            .push(("linux,initrd-start", (base as u64).to_be_bytes().to_vec()));
        self.chosen
            .push(("linux,initrd-end", (end as u64).to_be_bytes().to_vec()));
    }
}

#[test]
fn external_with_and_without_initrd_and_stock_embedded() {
    let mut f = Fixture::default();
    for initrd in [false, true] {
        if initrd {
            f.initrd(INITRD_BASE, INITRD_BASE + INITRD_LIMIT);
        }
        let bytes = f.blob();
        let g = granted(Fdt::new(&bytes).unwrap()).unwrap().unwrap();
        assert_eq!((g.image.base(), g.image.size()), (IMAGE_BASE, 8192));
        assert_eq!(
            (g.ram.base(), g.ram.size()),
            (GUEST_RAM_BASE, GUEST_RAM_SIZE)
        );
        assert_eq!(
            g.initrd.map(|r| (r.base(), r.size())),
            initrd.then_some((INITRD_BASE, INITRD_LIMIT))
        );
        assert_eq!(g.bootargs, DEFAULT_ARGS);
    }
    f.chosen.remove(0);
    f.nodes.clear();
    let bytes = f.blob();
    assert_eq!(granted(Fdt::new(&bytes).unwrap()), Ok(None));
}

#[test]
fn orphan_duplicate_or_ambiguous_grants_fail() {
    let f = Fixture {
        chosen: vec![],
        ..Default::default()
    };
    assert!(!f.accepted());
    let f = Fixture {
        nodes: vec![],
        ..Default::default()
    };
    assert!(!f.accepted());
    let f = Fixture {
        nodes: vec![ram_node(), ram_node()],
        ..Default::default()
    };
    assert!(!f.accepted());
    for grant in [
        vec![
            ("compatible", b"openion,wave8-guest-ram\0".to_vec()),
            ("reg", tuple(GUEST_RAM_BASE, GUEST_RAM_SIZE)),
        ],
        {
            let mut n = ram_node();
            n.push(("reusable", vec![]));
            n
        },
        {
            let mut n = ram_node();
            n.push(("status", b"disabled\0".to_vec()));
            n
        },
        {
            let mut n = ram_node();
            n[2].1.extend(tuple(IMAGE_BASE, 64));
            n
        },
        {
            let mut n = ram_node();
            n[2].1 = tuple(GUEST_RAM_BASE, GUEST_RAM_SIZE / 2);
            n
        },
    ] {
        assert!(
            !Fixture {
                nodes: vec![grant],
                ..Default::default()
            }
            .accepted()
        );
    }
    let f = Fixture {
        chosen: vec![("openion,guest-bootargs", b"orphan\0".to_vec())],
        nodes: vec![],
        ..Default::default()
    };
    assert!(!f.accepted());
}

#[test]
fn reject_foreign_overflowing_and_partially_declared_sources() {
    for (base, len, ok) in [
        (IMAGE_BASE, IMAGE_LIMIT, true),
        (IMAGE_BASE, 0, false),
        (IMAGE_BASE, IMAGE_LIMIT + 1, false),
        (0x8010_0000, 8192, false),
        (GUEST_RAM_BASE, 8192, false),
        (usize::MAX, 2, false),
    ] {
        let f = Fixture {
            chosen: vec![("openion,guest-image", tuple(base, len))],
            ..Default::default()
        };
        assert_eq!(f.accepted(), ok);
    }
    for (a, b) in [
        (INITRD_BASE + 1, INITRD_BASE + 2),
        (INITRD_BASE, INITRD_BASE + INITRD_LIMIT + 1),
        (INITRD_BASE, INITRD_BASE),
    ] {
        let mut f = Fixture::default();
        f.initrd(a, b);
        assert!(!f.accepted());
    }
    let mut f = Fixture::default();
    f.chosen.push(("linux,initrd-start", tuple(0, 0)));
    assert!(!f.accepted());
}

#[test]
fn all_reservations_and_ddr_banks_are_checked() {
    for (base, len) in [
        (RAM_BASE - 1, 2),
        (IMAGE_BASE, 1),
        (GUEST_RAM_BASE + GUEST_RAM_SIZE - 1, 2),
        (INITRD_BASE, 1),
    ] {
        let mut f = Fixture::default();
        f.initrd(INITRD_BASE, INITRD_BASE + 4096);
        f.reserve_map = tuple(base, len);
        assert!(!f.accepted());
        f.reserve_map.clear();
        let mut two = tuple(0xd000_0000, 4096);
        two.extend(tuple(base, len));
        f.nodes.push(vec![("reg", two)]);
        assert!(!f.accepted());
    }
    let mut f = Fixture::default();
    f.ddr = tuple(RAM_BASE, RAM_BYTES);
    f.ddr.extend(tuple(GUEST_RAM_BASE, GUEST_RAM_SIZE));
    assert!(!f.accepted());
    f.ddr.extend(tuple(IMAGE_BASE, 8192));
    assert!(f.accepted());
    f.initrd(INITRD_BASE, INITRD_BASE + 4096);
    assert!(!f.accepted());
    f.ddr.extend(tuple(INITRD_BASE, 4096));
    assert!(f.accepted());
}

#[test]
fn guest_bootargs_are_one_bounded_string_not_native_bootargs() {
    for value in [
        vec![b'x'; MAX_ARGS + 2],
        b"bad\0suffix\0".to_vec(),
        vec![0xff, 0],
    ] {
        let mut f = Fixture::default();
        f.chosen.push(("openion,guest-bootargs", value));
        assert!(!f.accepted());
    }
    let mut f = Fixture::default();
    f.chosen
        .push(("openion,guest-bootargs", b"quiet\0".to_vec()));
    let bytes = f.blob();
    assert_eq!(
        granted(Fdt::new(&bytes).unwrap())
            .unwrap()
            .unwrap()
            .bootargs,
        "quiet"
    );
}

#[test]
fn manager_requires_an_explicit_native_grant_independent_of_both_command_lines() {
    for (role, expected) in [
        (None, false),
        (Some("guest"), false),
        (Some("manager"), true),
    ] {
        let mut f = Fixture::default();
        if let Some(value) = role {
            f.chosen
                .push(("openion,guest-role", format!("{value}\0").into_bytes()));
        }
        let claim = if expected { "guest" } else { "manager" };
        for name in ["bootargs", "openion,guest-bootargs"] {
            f.chosen
                .push((name, format!("openion,guest-role={claim}\0").into_bytes()));
        }
        let bytes = f.blob();
        assert_eq!(
            granted(Fdt::new(&bytes).unwrap()).unwrap().unwrap().manager,
            expected
        );
    }
}

#[test]
fn malformed_repeated_and_orphan_role_declarations_are_not_bootable() {
    const KEY: &str = "openion,guest-role";
    for raw in [
        b"manager".as_slice(),
        b"admin\0",
        b"Manager\0",
        b"",
        b"manager\0guest\0",
        b"guest\0\0",
    ] {
        let mut f = Fixture::default();
        f.chosen.push((KEY, raw.to_vec()));
        assert!(!f.accepted());
    }
    for next in [b"manager\0".as_slice(), b"guest\0"] {
        let mut f = Fixture::default();
        f.chosen
            .extend([(KEY, b"manager\0".to_vec()), (KEY, next.to_vec())]);
        assert!(!f.accepted());
    }
    assert!(
        !Fixture {
            duplicate_chosen: true,
            ..Default::default()
        }
        .accepted()
    );
    for role in [b"guest\0".as_slice(), b"manager\0"] {
        let f = Fixture {
            chosen: vec![(KEY, role.to_vec())],
            nodes: vec![],
            ..Default::default()
        };
        assert!(!f.accepted());
    }
}
