//! Native pool ownership is separate from boot RAM, the role string and DT section order.
use super::*;

const BASE: usize = 0xc000_0000;
const SIZE: usize = 8 << 20;

fn pool_node(base: usize, bytes: usize) -> Props {
    vec![
        ("compatible", b"openion,wave8-domain-pool\0".to_vec()),
        ("no-map", vec![]),
        ("reg", tuple(base, bytes)),
    ]
}

fn manager_pool(base: usize, bytes: usize) -> Fixture {
    let mut f = Fixture::default();
    f.chosen.push(("openion,guest-role", b"manager\0".to_vec()));
    f.nodes.push(pool_node(base, bytes));
    f
}

fn pool(f: &Fixture) -> Result<Option<PhysRange>> {
    let bytes = f.blob();
    Ok(granted(Fdt::new(&bytes)?)?.and_then(|g| g.pool))
}

#[test]
fn dedicated_pool_and_guest_ram_are_independent_optional_grants() {
    for (base, size) in [
        (BASE, POOL_ALIGN),
        (BASE, SIZE),
        (BASE, GIGA),
        (INITRD_BASE, POOL_ALIGN),
    ] {
        let mut f = manager_pool(base, size);
        let expected = Some(PhysRange::new(base, size).unwrap());
        assert_eq!(pool(&f), Ok(expected));
        f.nodes.reverse();
        assert_eq!(pool(&f), Ok(expected));
    }
    let mut f = manager_pool(BASE, SIZE);
    f.nodes.pop();
    assert_eq!(pool(&f), Ok(None));
    let mut disabled = pool_node(BASE, SIZE);
    disabled.push(("status", b"disabled\0".to_vec()));
    f.nodes.push(disabled);
    assert_eq!(pool(&f), Ok(None));
}

#[test]
fn optional_pool_needs_a_live_external_manager_and_a_unique_role() {
    let mut f = manager_pool(BASE, SIZE);
    f.chosen.pop();
    assert_eq!(pool(&f), Err(Error::Property));
    f.chosen.push(("openion,guest-role", b"guest\0".to_vec()));
    assert_eq!(pool(&f), Err(Error::Property));
    f.chosen.clear();
    f.nodes.remove(0);
    assert_eq!(pool(&f), Err(Error::Property));

    for status in [None, Some(b"disabled\0".as_slice())] {
        let mut f = manager_pool(BASE, SIZE);
        let mut second = pool_node(BASE + SIZE, SIZE);
        if let Some(status) = status {
            second.push(("status", status.to_vec()));
        }
        f.nodes.push(second);
        assert!(pool(&f).is_err());
        f.nodes.reverse();
        assert!(pool(&f).is_err());
    }
    for other in [
        b"openion,wave8-guest-ram\0".as_slice(),
        b"shared-dma-pool\0",
    ] {
        let mut f = manager_pool(BASE, SIZE);
        f.nodes[1][0].1.extend_from_slice(other);
        assert!(pool(&f).is_err());
    }
}

#[test]
fn grant_properties_and_pool_reg_must_be_unique_and_exact() {
    let mut f = manager_pool(BASE, SIZE);
    f.nodes[1].remove(1);
    assert_eq!(pool(&f), Err(Error::Reservation));
    for property in [
        ("reusable", vec![]),
        ("reusable", vec![1]),
        ("no-map", vec![]),
        ("compatible", b"openion,wave8-domain-pool\0".to_vec()),
        ("reg", tuple(BASE, SIZE)),
    ] {
        let mut f = manager_pool(BASE, SIZE);
        f.nodes[1].push(property);
        assert!(pool(&f).is_err());
    }
    let mut f = manager_pool(BASE, SIZE);
    f.nodes[1][1].1.push(0);
    assert!(pool(&f).is_err());
    for raw in [
        vec![],
        vec![0; 12],
        [tuple(BASE, SIZE), tuple(BASE + SIZE, SIZE)].concat(),
    ] {
        let mut f = manager_pool(BASE, SIZE);
        f.nodes[1][2].1 = raw;
        assert_eq!(pool(&f), Err(Error::Cells));
    }
    let mut f = manager_pool(BASE, SIZE);
    f.nodes[1].push(("status", b"okay\0".to_vec()));
    f.nodes[1].push(("status", b"disabled\0".to_vec()));
    assert_eq!(pool(&f), Err(Error::Property));
}

#[test]
fn pool_geometry_is_bounded_and_fits_one_ddr_tuple() {
    const CEILING: usize = 1usize << 56;
    for (base, size) in [
        (0x7fe0_0000, POOL_ALIGN),
        (BASE + 4096, SIZE),
        (BASE, 0),
        (BASE, POOL_ALIGN - 4096),
        (BASE, SIZE + 4096),
        (BASE, GIGA + POOL_ALIGN),
        (CEILING - POOL_ALIGN, 2 * POOL_ALIGN),
        (CEILING, POOL_ALIGN),
        (usize::MAX - POOL_ALIGN + 1, POOL_ALIGN),
    ] {
        assert!(
            pool(&manager_pool(base, size)).is_err(),
            "base={base:#x} size={size:#x}"
        );
    }
    let mut last = manager_pool(CEILING - POOL_ALIGN, POOL_ALIGN);
    last.ddr.extend(tuple(CEILING - POOL_ALIGN, POOL_ALIGN));
    assert!(pool(&last).is_ok());
    let mut f = manager_pool(BASE, SIZE);
    f.ddr = tuple(0x8000_0000, BASE - 0x8000_0000);
    assert!(pool(&f).is_err());
    f.ddr.extend(tuple(BASE, SIZE / 2));
    f.ddr.extend(tuple(BASE + SIZE / 2, SIZE / 2));
    assert!(pool(&f).is_err());
    f.ddr.extend(tuple(BASE, SIZE));
    assert!(pool(&f).is_ok());
    f.ddr_name = "memory";
    assert!(pool(&f).is_ok());
    f.ddr_name = "buffer@80000000";
    assert_eq!(pool(&f), Err(Error::Reservation));
    f.ddr_name = "memory@80000000";
    f.ddr_extra.push(("status", b"disabled\0".to_vec()));
    assert_eq!(pool(&f), Err(Error::Reservation));
    f.ddr_extra[0].1 = b"okay\0".to_vec();
    f.ddr_extra.push(("status", b"disabled\0".to_vec()));
    assert_eq!(pool(&f), Err(Error::Property));
}

#[test]
fn reserved_and_retained_bytes_never_enter_the_allocator() {
    for base in [
        DDR_BASE,
        RAM_BASE,
        INITRD_BASE,
        IMAGE_BASE,
        HANDOFF_START,
        GUEST_RAM_BASE,
        GUEST_RAM_BASE + GUEST_RAM_SIZE - POOL_ALIGN,
    ] {
        let mut f = manager_pool(base, POOL_ALIGN);
        f.initrd(INITRD_BASE, INITRD_BASE + 1);
        assert_eq!(pool(&f), Err(Error::Reservation), "base={base:#x}");
    }
    let mut f = manager_pool(BASE, SIZE);
    f.initrd(INITRD_BASE, INITRD_BASE + 1);
    f.chosen.push((
        "linux,initrd-end",
        (BASE as u64 + 4096).to_be_bytes().to_vec(),
    ));
    assert_eq!(pool(&f), Err(Error::Property));
    // An aligned manager grant reserves another 2 MiB after the 1 GiB guest leaf.
    let mut f = Fixture::aligned(BASE, GIGA + POOL_ALIGN, true);
    f.nodes.push(pool_node(BASE + GIGA, POOL_ALIGN));
    assert_eq!(pool(&f), Err(Error::Reservation));

    for (base, size, valid) in [
        (BASE - POOL_ALIGN, POOL_ALIGN, true),
        (BASE + SIZE, 1, true),
        (BASE - 1, 2, false),
        (BASE + SIZE - 1, 2, false),
        (BASE + 1, 1, false),
        (BASE - 1, SIZE + 2, false),
        (BASE, SIZE, false),
    ] {
        // The boot guest ends at BASE, so place the pool further into DDR for this table.
        let offset = 2 * SIZE;
        let mut f = manager_pool(BASE + offset, SIZE);
        let other = tuple(base + offset, size);
        f.reserve_map = other.clone();
        assert_eq!(pool(&f).is_ok(), valid);
        f.reserve_map.clear();
        let mut regs = tuple(BASE + 4 * SIZE, 4096);
        regs.extend(other);
        f.nodes.push(vec![("reg", regs)]);
        assert_eq!(pool(&f).is_ok(), valid);
        f.nodes.reverse();
        assert_eq!(pool(&f).is_ok(), valid);
        f.nodes[0].push(("status", b"disabled\0".to_vec()));
        assert_eq!(pool(&f).is_ok(), valid);
    }
}
