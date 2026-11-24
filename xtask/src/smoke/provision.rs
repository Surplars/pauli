//! Scale the diskless provisioning gate without changing a board or boot manifest.
//! Ownership/Lock Ordering: the host controller owns every stopped reservation.
//! Invariants: only the parent and the high-ID child boot; every created ID is checked.
use super::{ALPHA, Controller, PONG, READY, Source, respond};
use anyhow::{Context, Result, ensure};

const DEFAULT: u32 = 5;
const EIGHT_ONLINE: &str = "[probe] shape 8 CPUs 128 MiB PASS";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Scale(u32);

impl Scale {
    pub(super) fn parse(raw: Option<&str>) -> Result<Self> {
        let n = raw
            .map(str::parse)
            .transpose()
            .context("invalid --domains")?
            .unwrap_or(DEFAULT);
        // This bounds the cost of the test, not the hypervisor's VM directory.
        ensure!(
            (DEFAULT..=128).contains(&n),
            "--domains must be between 5 and 128"
        );
        Ok(Self(n))
    }
    pub(super) fn extra_mib(self) -> u64 {
        // Keep the original three 96-MiB holds and 128-MiB child; subsequent
        // stopped guests use 64 MiB plus room for the 2-MiB extent alignment.
        448 + u64::from(self.0 - DEFAULT) * 66
    }
    pub(super) fn verify(self, c: &mut Controller, parent: u32) -> Result<()> {
        c.wait(Source::Guest(parent), 0, "[probe] shape 1 CPU 96 MiB PASS")?;
        for id in 2..self.0 {
            let shape = if id < DEFAULT { "" } else { " --memory-mib 64" };
            create(c, id, &format!("hold{id}"), shape)?;
        }
        let id = self.0;
        let shape = " --vcpus 8 --memory-mib 128";
        create(c, id, "child", shape)?;
        allocated(c, id)?;
        let created = c.status("child", "Stopped")?;
        c.command("vm start child")?;
        let child = c.status("child", "Ready")?;
        ensure!(
            child == created && child != parent,
            "unexpected first activation identity"
        );
        c.wait(Source::Guest(child), 0, EIGHT_ONLINE)?;
        c.wait(Source::Guest(child), 0, READY)?;
        respond(c, ALPHA, parent, b'p', PONG)?;
        c.attach("child", child)?;
        c.guest(child, b'q', "[probe] Poweroff")?;
        c.terminal(id, child, "Stopped")?;
        c.command("vm start child")?;
        let restarted = c.status("child", "Ready")?;
        ensure!(restarted != child, "cold restart retained the old epoch");
        c.wait(Source::Guest(restarted), 0, EIGHT_ONLINE)?;
        respond(c, ALPHA, parent, b'p', PONG)?;
        c.command("vm stop child")?;
        c.terminal(id, restarted, "Stopped")?;
        c.destroy(id, "child")?;
        create(c, id, "child", shape)?;
        let fresh = c.status("child", "Stopped")?;
        ensure!(
            fresh != restarted && fresh != child,
            "slot reuse retained old authority"
        );
        c.destroy(id, "child")?;
        for held in 2..id {
            c.destroy(held, &format!("hold{held}"))?;
        }
        allocated(c, 1)?;
        respond(c, ALPHA, parent, b'p', PONG)
    }
}

fn create(c: &mut Controller, id: u32, name: &str, shape: &str) -> Result<()> {
    c.command(&format!("vm create alpha {name}{shape}"))?;
    c.wait(
        Source::Host,
        c.last_command(),
        &format!("VM {id} created: {name} (stopped)"),
    )
}
fn allocated(c: &mut Controller, n: u32) -> Result<()> {
    c.command("vm capacity")?;
    c.wait(
        Source::Host,
        c.last_command(),
        &format!("allocated={n} id-limit="),
    )
}

#[cfg(test)]
mod tests {
    use super::{super::Suite, *};

    #[test]
    fn scaling_is_opt_in_and_budgets_each_extra_extent() {
        assert_eq!(Scale::parse(None).unwrap().extra_mib(), 448);
        let Suite::Provision(scale) = Suite::parse("provision", Some("70")).unwrap() else {
            panic!("wrong suite");
        };
        assert_eq!(scale.extra_mib(), 448 + 65 * 66);
        for invalid in ["0", "4", "129", "lots"] {
            assert!(Scale::parse(Some(invalid)).is_err());
        }
        assert!(Suite::parse("instances", Some("70")).is_err());
    }
}
