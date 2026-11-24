//! Standalone FDT fixture builder and hostile blob checks; no host FDT library.
//! Ownership/Lock Ordering: local byte vectors only, no shared state or locks.
//! Invariants: malformed external tags/strings/ranges return errors, never panic.

use crate::fdt::{Error, Fdt};
use std::vec::Vec;

struct Blob {
    structure: Vec<u8>,
    strings: Vec<u8>,
}
impl Blob {
    fn new() -> Self {
        Self {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }
    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }
    fn pad(&mut self) {
        while self.structure.len() % 4 != 0 {
            self.structure.push(0);
        }
    }
    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad();
    }
    fn property(&mut self, name: &str, value: &[u8]) {
        self.word(3);
        self.word(value.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(value);
        self.pad();
    }
    fn u32(&mut self, name: &str, value: u32) {
        self.property(name, &value.to_be_bytes());
    }
    fn end(&mut self) {
        self.word(2);
    }
    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let strings_offset = 56 + self.structure.len();
        let total = strings_offset + self.strings.len();
        let mut out = Vec::new();
        for word in [
            0xd00d_feed,
            total as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out.extend_from_slice(&[0; 16]);
        out.extend(self.structure);
        out.extend(self.strings);
        out
    }
}

fn fixture() -> Vec<u8> {
    let mut blob = Blob::new();
    blob.begin("");
    blob.u32("#address-cells", 2);
    blob.u32("#size-cells", 2);
    blob.begin("memory@40000000");
    blob.property("device_type", b"memory\0");
    blob.property(
        "reg",
        &[0x4000_0000u64.to_be_bytes(), 0x1000_0000u64.to_be_bytes()].concat(),
    );
    blob.end();
    blob.begin("cpus");
    blob.u32("timebase-frequency", 40_000_000);
    blob.u32("#address-cells", 1);
    blob.u32("#size-cells", 0);
    blob.begin("cpu@1");
    blob.u32("reg", 1);
    blob.property("device_type", b"cpu\0");
    blob.end();
    blob.end();
    blob.begin("chosen");
    blob.property("bootargs", b"console=ttyS0\0");
    blob.end();
    blob.begin("uart@20000000");
    blob.property("compatible", b"vendor,uart\0ns16550a\0");
    blob.property(
        "reg",
        &[0x2000_0000u64.to_be_bytes(), 0x1000u64.to_be_bytes()].concat(),
    );
    blob.u32("interrupts", 10);
    blob.end();
    blob.end();
    blob.finish()
}

#[test]
fn unique_property_distinguishes_absent_single_and_repeated_even_if_identical() {
    let mut b = Blob::new();
    b.begin("");
    b.property("ordinary", b"one\0");
    b.property("grant", b"manager\0");
    b.property("grant", b"manager\0");
    b.end();
    let bytes = b.finish();
    let root = Fdt::new(&bytes).unwrap().find("/").unwrap().unwrap();
    assert_eq!(root.unique_property("missing"), Ok(None));
    assert_eq!(
        root.unique_property("ordinary"),
        Ok(Some(b"one\0".as_slice()))
    );
    assert_eq!(root.unique_property("grant"), Err(Error::Property));
    assert_eq!(root.property("grant"), Ok(Some(b"manager\0".as_slice())));
}

#[test]
fn discover_memory_cpu_timebase_uart_and_chosen() {
    let bytes = fixture();
    let tree = Fdt::new(&bytes).unwrap();
    assert_eq!(
        tree.find("/cpus")
            .unwrap()
            .unwrap()
            .u32("timebase-frequency"),
        Ok(Some(40_000_000))
    );
    assert_eq!(
        tree.find("/cpus/cpu@1").unwrap().unwrap().reg(),
        Ok(Some((1, 0)))
    );
    assert_eq!(
        tree.find("/memory@40000000").unwrap().unwrap().reg(),
        Ok(Some((0x4000_0000, 0x1000_0000)))
    );
    assert_eq!(
        tree.find("/chosen").unwrap().unwrap().text("bootargs"),
        Ok(Some("console=ttyS0"))
    );
    let uart = tree.find("/uart@20000000").unwrap().unwrap();
    assert_eq!(uart.compatible("ns16550a"), Ok(true));
    assert_eq!(uart.u32("interrupts"), Ok(Some(10)));
    assert_eq!(tree.nodes().count(), 6);
    assert!(tree.find("/missing").unwrap().is_none());
}

#[test]
fn mutations_and_truncation_never_panic() {
    let bytes = fixture();
    for end in 0..bytes.len() {
        assert!(Fdt::new(&bytes[..end]).is_err());
    }
    for (offset, bit) in (0..bytes.len()).flat_map(|offset| (0..8).map(move |bit| (offset, bit))) {
        let mut corrupted = bytes.clone();
        corrupted[offset] ^= 1 << bit;
        if let Ok(tree) = Fdt::new(&corrupted) {
            for node in tree.nodes() {
                if let Ok(node) = node {
                    let _ = node.reg();
                    let _ = node.text("compatible");
                }
            }
        }
    }
    let mut bad = bytes.clone();
    bad[4..8].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(Fdt::new(&bad), Err(Error::Bounds)));
}

#[test]
fn bounded_depth_and_bad_reservations() {
    let mut blob = Blob::new();
    for _ in 0..17 {
        blob.begin("");
    }
    for _ in 0..17 {
        blob.end();
    }
    assert!(matches!(Fdt::new(&blob.finish()), Err(Error::Depth)));
    let mut bytes = fixture();
    bytes[40..48].copy_from_slice(&1u64.to_be_bytes());
    assert!(matches!(Fdt::new(&bytes), Err(Error::Reservation)));
    let mut bytes = fixture();
    bytes[8..12].copy_from_slice(&41u32.to_be_bytes());
    assert!(Fdt::new(&bytes).is_err());
}

#[test]
fn unrelated_wide_bus_does_not_break_boot_discovery() {
    let mut blob = Blob::new();
    blob.begin("");
    blob.begin("pci");
    blob.u32("#address-cells", 3);
    blob.u32("#size-cells", 2);
    blob.begin("device");
    blob.property("reg", &[0; 20]);
    blob.end();
    blob.end();
    blob.begin("chosen");
    blob.property("bootargs", b"test\0");
    blob.end();
    blob.end();
    let bytes = blob.finish();
    let fdt = Fdt::new(&bytes).unwrap();
    assert!(fdt.nodes().all(|node| node.is_ok()));
    assert_eq!(
        fdt.find("/pci/device").unwrap().unwrap().reg(),
        Err(Error::Cells)
    );
    assert_eq!(
        fdt.find("/chosen").unwrap().unwrap().text("bootargs"),
        Ok(Some("test"))
    );
}
