use super::*;
use crate::guest_dtb::writer::Writer;

#[test]
fn legacy_version_and_exact_extension_names() {
    for isa in [
        b"rv64gc_sstc\0".as_slice(),
        b"rv64imacsstc\0",
        b"rv64imafdc_zicsr_sstc1p0\0",
    ] {
        assert_eq!(legacy_isa(isa), Ok(true));
    }
    assert_eq!(legacy_isa(b"rv64gc_xsstc_sstcx\0"), Ok(false));
    assert_eq!(extensions(b"i\0xsstc\0sstcx\0"), Ok(false));
    for isa in [
        b"rv64gc_sstc1p\0".as_slice(),
        b"rv64gc_sstc_sstc\0",
        b"rv64gc_sstc",
    ] {
        assert!(legacy_isa(isa).is_err());
    }
    for ext in [
        b"sstc1p0\0".as_slice(),
        b"SSTC\0",
        b"sstc\0sstc\0",
        b"sstc",
        b"\0",
    ] {
        assert!(extensions(ext).is_err());
    }
}

type Properties<'a> = &'a [(&'a str, &'a [u8])];
fn guest(vcpus: u8, nodes: &[(u32, Properties<'_>)]) -> Result<bool, Error> {
    let mut out = [0; 2048];
    let mut dt = Writer::new(&mut out).unwrap();
    dt.begin(&[]).unwrap();
    dt.begin(&[b"cpus"]).unwrap();
    dt.u32("#address-cells", 1).unwrap();
    dt.u32("#size-cells", 0).unwrap();
    for &(id, props) in nodes {
        dt.at(b"cpu", u64::from(id)).unwrap();
        dt.u32("reg", id).unwrap();
        for &(name, bytes) in props {
            dt.prop(name, &[bytes]).unwrap();
        }
        dt.end().unwrap();
    }
    dt.end().unwrap();
    dt.end().unwrap();
    let size = dt.finish().unwrap();
    riscv_sstc(&out[..size], vcpus)
}

#[test]
fn each_configured_hart_has_one_unambiguous_timer_contract() {
    let modern: Properties<'_> = &[("riscv,isa-extensions", b"i\0sstc\0")];
    let legacy: Properties<'_> = &[("riscv,isa", b"rv64gc_sstc\0")];
    assert_eq!(guest(2, &[(1, modern), (0, legacy)]), Ok(true));
    assert_eq!(guest(1, &[(0, &[])]), Ok(false));
    assert_eq!(guest(2, &[(0, modern), (1, &[])]), Err(Error::Dtb));
    assert_eq!(guest(2, &[(0, modern)]), Err(Error::Dtb));
    assert_eq!(guest(1, &[(0, modern), (0, legacy)]), Err(Error::Dtb));
    assert_eq!(guest(1, &[(0, &[modern[0], modern[0]])]), Err(Error::Dtb));
    assert_eq!(
        guest(1, &[(0, &[modern[0], ("riscv,isa", b"rv64gc\0")])]),
        Err(Error::Dtb)
    );
    assert_eq!(
        guest(1, &[(0, modern), (1, &[("status", b"disabled\0")])]),
        Ok(true)
    );
    assert_eq!(
        guest(1, &[(0, modern), (0, &[("status", b"disabled\0")])]),
        Err(Error::Dtb)
    );
}
