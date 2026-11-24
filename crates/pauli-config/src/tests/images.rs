//! Independent/shared image containment and name rejection.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use super::*;

#[test]
fn images_and_names() {
    let config = fixture();
    let mut bad;
    // Distinct templates, exact sharing, source containment and alias policy.
    let mut separate = config;
    separate.template_size = 64 * 1024 * 1024;
    for vm in separate.vms.iter_mut().flatten() {
        vm.host_base += 0x2000_0000;
    }
    separate.vms[1].as_mut().unwrap().image = VmImage {
        kernel_source: 0x9000_0000,
        kernel_size: 128,
        kernel_memory_size: 8192,
        initrd_source: 0x9000_1000,
        initrd_size: 4096,
    };
    let raw = separate.encode().unwrap();
    assert_eq!(BootConfig::decode(&raw).unwrap().encode().unwrap(), raw);
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.kernel_source = u64::MAX & !4095;
    assert_eq!(bad.validate(), Err(Error::Image));
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.initrd_source = 0x9200_0000;
    assert_eq!(bad.validate(), Err(Error::Image));
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.initrd_source = config.vms[0].unwrap().image.kernel_source;
    assert_eq!(bad.validate(), Err(Error::Image));
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.kernel_source = config.vms[0].unwrap().image.kernel_source;
    assert_eq!(bad.validate(), Err(Error::Image)); // Shared base, unequal extent.
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.kernel_size = 64;
    bad.vms[1].as_mut().unwrap().image.kernel_source = config.vms[0].unwrap().image.kernel_source;
    assert_eq!(bad.validate(), Err(Error::Image)); // Shared bytes, conflicting BSS.
    bad = separate;
    bad.vms[0].as_mut().unwrap().host_base = separate.template_base;
    assert_eq!(bad.validate(), Err(Error::Memory));
    bad = separate;
    bad.vms[1].as_mut().unwrap().image.kernel_memory_size = u64::MAX;
    assert_eq!(bad.validate(), Err(Error::Image));
    for name in [
        "",
        "1numeric",
        "with space",
        "a/b",
        "abc\0suffix",
        "\u{4e00}",
    ] {
        assert!(VmConfig::new(name).is_err());
    }
}
