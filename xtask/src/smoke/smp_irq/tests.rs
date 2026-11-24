//! Reject reports that a boot banner, a timer interrupt or a software fallback could satisfy.
use super::*;

#[test]
fn rtc_gate_uses_a_separate_diskless_two_cpu_profile() -> Result<()> {
    use crate::{media::Scratch, smoke::Suite};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut scratch = Scratch::new(&root)?;
    for aia in [false, true] {
        let (cpus, vms) = Suite::parse("smp-irq", None)?.profile(&mut scratch, aia)?;
        let [vm] = vms.as_slice() else {
            panic!("one guest required")
        };
        assert_eq!(
            (cpus, vm.config.vcpus, vm.probe, vm.config.aia),
            (2, 2, Some(b'R'), aia)
        );
        assert!(vm.disk.is_none() && !vm.config.net_uplink && vm.config.net_peer.is_none());
    }
    Ok(())
}

const REPORTS: &str = "[probe] RTC IRQ irq=21 cpu=1 before=0,4 after=0,5\r\n[probe] RTC IRQ irq=21 cpu=0 before=0,5 after=1,5\n";
const JOIN: &str = "  vcpu=0 owner=1 state=Stopped retired=true run=Stopped\n  vcpu=1 owner=0 state=Stopped retired=true run=Stopped\n";
const NATIVE: &str =
    "irq-file epoch=7 loaded_vcpus=0x3 loads=24 puts=24 merges=30 injected=0 bound=0x0";
const SOFT: &str = "irq-file epoch=7 loaded_vcpus=0x0 loads=0 puts=0 merges=0 injected=0 bound=0x0";

#[test]
fn both_affinities_require_the_same_device_and_fresh_exclusive_counts() {
    assert!(reports(REPORTS.as_bytes()).is_ok());
    for invalid in [
        REPORTS.replace("after=0,5", "after=1,5"),
        REPORTS.replace("after=1,5", "after=0,5"),
        REPORTS.replace("after=1,5", "after=2,5"),
        REPORTS.replace("before=0,5 after=1,5", "before=0,6 after=1,6"),
        REPORTS.replace(
            "before=0,5 after=1,5",
            "before=18446744073709551615,5 after=0,5",
        ),
        REPORTS.replace("irq=21 cpu=0", "irq=22 cpu=0"),
        REPORTS.replace("before=0,5", "before=0,0"),
        REPORTS.replace("cpu=1", "cpu=0"),
        REPORTS.repeat(2),
        REPORTS.trim_end().to_owned(),
    ] {
        assert!(reports(invalid.as_bytes()).is_err(), "{invalid}");
    }
}

#[test]
fn joined_state_cannot_claim_native_files_from_another_epoch_or_one_cpu() {
    assert_eq!(retired_owners(JOIN).unwrap(), [1, 0]);
    assert!(retired_owners(&JOIN.replace("owner=1", "owner=0")).is_err());
    assert!(retired_owners(&JOIN.replace("retired=true", "retired=false")).is_err());
    assert!(files(NATIVE, 7, Backend::NativeImsic).is_ok());
    assert!(files(NATIVE, 8, Backend::NativeImsic).is_err());
    for (from, to) in [
        ("0x3", "0x1"),
        ("puts=24", "puts=23"),
        ("bound=0x0", "bound=0x2"),
    ] {
        assert!(files(&NATIVE.replace(from, to), 7, Backend::NativeImsic).is_err());
    }
    assert!(files(SOFT, 7, Backend::NativeImsic).is_err());
    assert!(files(SOFT, 7, Backend::SoftwareImsic).is_ok());
    assert!(files(NATIVE, 7, Backend::SoftwareImsic).is_err());
    assert!(files(SOFT, 7, Backend::Plic).is_err());
    assert!(files("", 7, Backend::Plic).is_ok());
}
