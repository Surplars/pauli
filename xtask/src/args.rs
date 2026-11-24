//! Small exact CLI grammar, independent of host target or default current directory.
//! Ownership/Lock Ordering: one command borrows its parsed argument map.
//! Invariants: duplicate/irrelevant arguments fail before builds, files or processes.
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
const HELP: &str = "cargo xtask test
cargo xtask build --board <qemu-virt-riscv|valence> [--features LIST] [--release]
cargo xtask image --board valence [--output PATH]
cargo xtask run --vm PROFILE --kernel IMAGE [--initrd ARCHIVE] [--host-cpus N] [--host-memory-mib N]
    [--disk RAW] [--persistent] [--features LIST] [--aia-host] [--dry-run]
cargo xtask run --rtos-elf CONTROL --service-elf SERVICE [--host-cpus N] [--dry-run]
cargo xtask smoke --suite firmware|firmware-idle --rtos-elf CONTROL --service-elf SERVICE [--host-elf LOCAL-HYPERVISOR | --features selftest] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite firmware-timer --timer-elf PROBE --service-elf SERVICE [--host-elf LOCAL-HYPERVISOR] [--board qemu-virt-riscv] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite firmware-timer-multi --timer-elf PROBE --service-elf FAIR-WORKER [--host-elf LOCAL-HYPERVISOR] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite firmware-timer-cohort|firmware-timer-smp --timer-elf COHORT-PROBE --service-elf FAIR-WORKER [--host-elf LOCAL-HYPERVISOR] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite firmware-irq|firmware-irq-smp|firmware-irq-latency|firmware-irq-scale --rtos-elf CONTROL --service-elf SERVICE [--features selftest] [--timeout-secs N] [--dry-run]
    [--samples PATH (firmware-irq-latency / firmware-irq-scale only; CSV path relative to the invocation directory)]
cargo xtask smoke --suite native-timer|native-ipi [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite mailbox [--host-cpus 1|2] [--host-elf LOCAL-HYPERVISOR | --features selftest] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite ipi-handoff [--host-cpus 1|2] [--timeout-secs N] [--dry-run]
cargo xtask smoke --suite hv|linux|smp|smp-irq|instances|net|storage|provision|volumes [--kernel IMAGE]
    [--domains N (provision)] [--host-cpus N (smp/provision)] [--timeout-secs N] [--features LIST] [--aia-host] [--dry-run]";
#[cfg_attr(test, derive(Default))]
pub struct Options {
    pub command: String,
    values: BTreeMap<String, Option<String>>,
}
impl Options {
    pub fn parse() -> Result<Self> {
        Self::parse_from(std::env::args().skip(1))
    }
    pub(crate) fn parse_from(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let command = args.next().unwrap_or_else(|| "--help".into());
        if matches!(command.as_str(), "-h" | "--help" | "help") {
            println!("{HELP}");
            std::process::exit(0);
        }
        let mut values = BTreeMap::new();
        while let Some(option) = args.next() {
            if matches!(option.as_str(), "-h" | "--help") {
                println!("{HELP}");
                std::process::exit(0);
            }
            let name = option.strip_prefix("--").context("expected --option")?;
            let value = match name {
                "release" | "persistent" | "aia-host" | "dry-run" => None,
                "board" | "features" | "output" | "vm" | "kernel" | "initrd" | "disk"
                | "host-cpus" | "host-memory-mib" | "suite" | "timeout-secs" | "domains"
                | "rtos-elf" | "timer-elf" | "service-elf" | "samples" | "host-elf" => Some(
                    args.next()
                        .filter(|arg| !arg.starts_with("--"))
                        .with_context(|| format!("--{name} needs a value"))?,
                ),
                _ => bail!("unknown option --{name}"),
            };
            ensure!(
                values.insert(name.to_owned(), value).is_none(),
                "duplicate --{name}"
            );
        }
        Ok(Self { command, values })
    }
    pub fn value(&self, key: &str) -> Option<&str> {
        self.values.get(key)?.as_deref()
    }
    pub fn flag(&self, key: &str) -> bool {
        self.values.contains_key(key)
    }
    pub fn only(&self, allowed: &[&str]) -> Result<()> {
        for key in self.values.keys() {
            ensure!(
                allowed.contains(&key.as_str()),
                "option --{key} is not valid for {}",
                self.command
            );
        }
        Ok(())
    }
    pub fn number(&self, name: &str, default: u64, max: u64) -> Result<u64> {
        let number = self
            .value(name)
            .map(str::parse::<u64>)
            .transpose()
            .with_context(|| format!("invalid --{name}"))?
            .unwrap_or(default);
        ensure!(
            (1..=max).contains(&number),
            "--{name} must be between 1 and {max}"
        );
        Ok(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_takes_exactly_one_path() {
        let parse = |args: &[&str]| Options::parse_from(args.iter().map(|s| s.to_string()));
        let opts = parse(&["smoke", "--samples", "reports/irq samples.csv"]).unwrap();
        assert_eq!(opts.value("samples"), Some("reports/irq samples.csv"));
        assert!(parse(&["smoke", "--samples"]).is_err());
        assert!(parse(&["smoke", "--samples", "--dry-run"]).is_err());
        assert!(parse(&["smoke", "--samples", "a", "--samples", "b"]).is_err());
    }
}
