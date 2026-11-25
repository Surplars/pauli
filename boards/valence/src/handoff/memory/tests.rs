//! In-memory native DTBs: exercise the complete grant walk without dtc, guest
//! files or physical pointers. All bytes and reserves belong to each test.
use super::*;
use crate::handoff::tests::blob::Tokens;

type Props = Vec<(&'static str, Vec<u8>)>;
mod pool;

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
    ddr_name: &'static str,
    ddr_extra: Props,
    reserve_map: Vec<u8>,
}
impl Default for Fixture {
    fn default() -> Self {
        Self {
            chosen: vec![("openion,guest-image", tuple(IMAGE_BASE, 8192))],
            duplicate_chosen: false,
            nodes: vec![ram_node()],
            ddr: tuple(0x8000_0000, 1usize << 32),
            ddr_name: "memory@80000000",
            ddr_extra: vec![],
            reserve_map: vec![],
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
        let mut ddr = vec![
            ("device_type", b"memory\0".to_vec()),
            ("reg", self.ddr.clone()),
        ];
        ddr.extend(self.ddr_extra.clone());
        w.begin(self.ddr_name, &ddr);
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
        w.finish(&self.reserve_map)
    }
    fn accepted(&self) -> bool {
        let bytes = self.blob();
        granted(Fdt::new(&bytes).unwrap()).is_ok()
    }
    fn aligned(base: usize, size: usize, manager: bool) -> Self {
        let mut f = Self::default();
        f.nodes[0][2].1 = tuple(base, size);
        if manager {
            f.chosen.push(("openion,guest-role", b"manager\0".to_vec()));
        }
        f
    }
    fn initrd(&mut self, base: usize, end: usize) {
        self.chosen
            .push(("linux,initrd-start", (base as u64).to_be_bytes().to_vec()));
        self.chosen
            .push(("linux,initrd-end", (end as u64).to_be_bytes().to_vec()));
    }
}

#[test]
fn peer_net_is_a_unique_boolean_for_an_external_manager() {
    const FLAG: &str = "openion,peer-net";
    let admitted = |f: &Fixture| {
        let blob = f.blob();
        granted(Fdt::new(&blob).unwrap()).map(|g| g.map(|g| g.peer_net))
    };
    let mut f = Fixture::default();
    assert_eq!(admitted(&f), Ok(Some(false)));
    f.chosen.push((FLAG, vec![]));
    assert_eq!(admitted(&f), Err(Error::Property));
    f.chosen.push(("openion,guest-role", b"manager\0".to_vec()));
    assert_eq!(admitted(&f), Ok(Some(true)));
    f.chosen.push((FLAG, vec![]));
    assert_eq!(admitted(&f), Err(Error::Property));
    f.chosen.pop();
    for malformed in [vec![0], vec![0; 4], b"true\0".to_vec()] {
        f.chosen[1].1 = malformed;
        assert_eq!(admitted(&f), Err(Error::Property));
    }
    f.chosen = vec![(FLAG, vec![])];
    f.nodes.clear();
    assert_eq!(admitted(&f), Err(Error::Property));
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
fn irq_policy_is_one_exact_native_string_for_an_external_guest() {
    const KEY: &str = "openion,guest-irq";
    for (raw, expected) in [
        (None, false),
        (Some(b"plic\0".as_slice()), false),
        (Some(b"aia\0".as_slice()), true),
    ] {
        let mut f = Fixture::default();
        f.chosen.push((
            "openion,guest-bootargs",
            b"openion,guest-irq=aia\0".to_vec(),
        ));
        if let Some(value) = raw {
            f.chosen.push((KEY, value.to_vec()));
        }
        let bytes = f.blob();
        assert_eq!(
            granted(Fdt::new(&bytes).unwrap()).unwrap().unwrap().aia,
            expected
        );
    }
    for values in [
        vec![b"aia".as_slice()],
        vec![b"AIA\0"],
        vec![b""],
        vec![b"plic\0aia\0"],
        vec![b"aia\0", b"aia\0"],
        vec![b"plic\0", b"aia\0"],
    ] {
        let mut f = Fixture::default();
        f.chosen
            .extend(values.into_iter().map(|raw| (KEY, raw.to_vec())));
        let bytes = f.blob();
        assert_eq!(granted(Fdt::new(&bytes).unwrap()), Err(Error::Property));
    }
    for raw in [b"aia\0".as_slice(), b"plic\0"] {
        let f = Fixture {
            chosen: vec![(KEY, raw.to_vec())],
            nodes: vec![],
            ..Default::default()
        };
        let bytes = f.blob();
        assert_eq!(granted(Fdt::new(&bytes).unwrap()), Err(Error::Property));
    }
}

#[test]
fn aligned_grant_requires_the_exact_role_geometry_and_a_single_covering_bank() {
    const BASE: usize = 0x1_0000_0000;
    const TAIL: usize = pauli_config::ALIGN as usize;
    for (manager, size) in [(false, GIGA), (true, GIGA + TAIL)] {
        for base in [ALIGNED_RAM_MIN, BASE] {
            let f = Fixture::aligned(base, size, manager);
            let bytes = f.blob();
            let guest = granted(Fdt::new(&bytes).unwrap()).unwrap().unwrap();
            assert_eq!(guest.ram, PhysRange::new(base, size).unwrap());
            assert_eq!(guest.manager, manager);
        }
        for (base, len) in [
            (BASE + TAIL, size),
            (GUEST_RAM_BASE, size),
            (BASE, size - 4096),
            (BASE, size + 4096),
            (BASE, if manager { GIGA } else { GIGA + TAIL }),
        ] {
            assert!(!Fixture::aligned(base, len, manager).accepted());
        }
        let mut f = Fixture::aligned(BASE, size, manager);
        f.ddr = tuple(0x8000_0000, GIGA);
        f.ddr.extend(tuple(BASE, size / 2));
        f.ddr.extend(tuple(BASE + size / 2, size - size / 2));
        assert!(!f.accepted());
        f.ddr.extend(tuple(BASE, size));
        assert!(f.accepted());
    }
    let mut unrepresentable = Fixture::aligned(1usize << 56, GIGA, false);
    unrepresentable.ddr.extend(tuple(1usize << 56, GIGA));
    assert!(!unrepresentable.accepted());
    let last_base = (1usize << 56) - GIGA;
    for manager in [false, true] {
        let size = GIGA + if manager { TAIL } else { 0 };
        let mut last = Fixture::aligned(last_base, size, manager);
        last.ddr.extend(tuple(last_base, size));
        assert_eq!(last.accepted(), !manager);
    }
    assert!(!Fixture::aligned(usize::MAX - GIGA + 1, GIGA, false).accepted());
}

#[test]
fn aligned_grant_checks_every_reservation_including_the_manager_tail() {
    let tail = pauli_config::ALIGN as usize;
    let size = GIGA + tail;
    let base = ALIGNED_RAM_MIN;
    for addr in [0xd000_0000, base + GIGA, base + size - 1] {
        let mut f = Fixture::aligned(base, size, true);
        f.reserve_map = tuple(addr, 1);
        assert!(!f.accepted());
        f.reserve_map.clear();
        let foreign = vec![("reg", tuple(addr, 1))];
        f.nodes.insert(0, foreign);
        assert!(!f.accepted());
        f.nodes.reverse();
        assert!(!f.accepted());
    }
    let mut f = Fixture::aligned(base, size, true);
    f.reserve_map = tuple(base + size, 4096);
    assert!(f.accepted());
    f.nodes.push(ram_node());
    assert!(!f.accepted());
    let mut f = Fixture::aligned(base, size, true);
    f.nodes[0].push(("reg", tuple(base, size)));
    assert!(!f.accepted());
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
