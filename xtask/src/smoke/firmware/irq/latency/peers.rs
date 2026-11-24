//! Keep VM slots and placement groups fixed; vary only the number of parked peers.
//! Ownership: one serial controller drives monitor transitions; there are no shared locks.
//! Invariants: a live peer is Waiting on the measured owner; its UART never receives input.
use super::super::{cpu, waiting};
use super::{Controller, READY, Serial, Source};
use anyhow::{Context, Result, ensure};

struct Peer {
    id: u32,
    name: String,
    epoch: u32,
    booted: bool,
}

pub(super) struct Peers {
    owner: u8,
    clones: Vec<Peer>,
    started: usize, // A prefix of clones; smaller plateaus retain the same live peers.
}

impl Peers {
    pub(super) fn reserve<T: Serial>(
        c: &mut Controller<T>,
        owner: u8,
        control: u32,
        service: u32,
        max: usize,
    ) -> Result<Self> {
        let mut clones: Vec<Peer> = Vec::with_capacity(max);
        for index in 0..max {
            let id = u32::try_from(index + 3)?; // The two boot templates already own IDs 1 and 2.
            let name = format!("idle{id}");
            c.command(&format!("vm create service {name}"))?;
            let created = format!("VM {id} created: {name} (stopped)");
            ensure!(reply(c)?.lines().any(|l| l == created), "missing {created}");
            let epoch = c.status(&name, "Stopped")?;
            ensure!(
                epoch != 0
                    && epoch != control
                    && epoch != service
                    && clones.iter().all(|p| p.epoch != epoch),
                "{name} has a missing or shared epoch {epoch}"
            );
            clones.push(Peer {
                id,
                name,
                epoch,
                booted: false,
            });
        }
        Ok(Self {
            owner,
            clones,
            started: 0,
        })
    }

    pub(super) fn resize<T: Serial>(&mut self, c: &mut Controller<T>, count: usize) -> Result<()> {
        ensure!(
            count <= self.clones.len(),
            "more waiting peers than reserved slots"
        );
        while self.started < count {
            let p = &mut self.clones[self.started];
            // The mirrored experiment starts each reserved VM exactly once; restarting it
            // would allocate a new epoch and no longer reproduce the original population.
            ensure!(!p.booted, "cannot restart {} in this sweep", p.name);
            c.command(&format!("vm start {}", p.name))?;
            c.wait(Source::Guest(p.epoch), 0, READY)?;
            loop {
                ensure!(
                    c.status(&p.name, "Ready")? == p.epoch,
                    "{} epoch changed",
                    p.name
                );
                if waiting(reply(c)?.as_bytes(), self.owner)? {
                    break;
                }
                // The guest prints its marker just before WFI; the controller deadline
                // bounds a racing Runnable observation without delaying the scored path.
            }
            p.booted = true;
            self.started += 1;
        }
        while self.started > count {
            let p = &self.clones[self.started - 1];
            c.command(&format!("vm stop {}", p.name))?;
            c.terminal(p.id, p.epoch, "Stopped")?;
            self.started -= 1;
        }
        Ok(())
    }

    pub(super) fn check<T: Serial>(&self, c: &mut Controller<T>) -> Result<()> {
        allocated(c, self.clones.len() + 2)?;
        c.command("vm status")?;
        let snapshot = reply(c)?;
        for (index, p) in self.clones.iter().enumerate() {
            let live = index < self.started;
            let state = if live { "Ready" } else { "Stopped" };
            let header = format!(
                "{} epoch={} name={} state={state} vcpus=1 memory=32KiB",
                p.id, p.epoch, p.name
            );
            let id = format!("{} ", p.id);
            let mut rows = snapshot
                .split("\nVM id=")
                .skip(1)
                .filter(|row| row.starts_with(&id));
            let row = rows
                .next()
                .with_context(|| format!("missing peer status: {header}"))?;
            ensure!(rows.next().is_none(), "duplicate status for {}", p.name);
            ensure!(
                row.lines().next() == Some(&header),
                "expected peer status: {header}"
            );
            if live {
                ensure!(
                    waiting(row.as_bytes(), self.owner)?,
                    "{} is not parked",
                    p.name
                );
            } else {
                // A never-entered boot CPU is prepared Runnable but retired; a joined
                // former owner is retired and Stopped. Neither is a waiting peer.
                let expected = if p.booted {
                    "Stopped retired=true run=Stopped"
                } else {
                    "Started retired=true run=Runnable"
                };
                ensure!(
                    cpu(row.as_bytes(), self.owner)? == expected,
                    "{} is not retired",
                    p.name
                );
            }
            let expected: &[u8] = if p.booted { READY.as_bytes() } else { &[] };
            ensure!(
                c.log(Source::Guest(p.epoch)) == expected,
                "unexpected guest transcript for {} epoch={}",
                p.name,
                p.epoch
            );
        }
        Ok(())
    }

    pub(super) fn remove<T: Serial>(self, c: &mut Controller<T>) -> Result<()> {
        ensure!(self.started == 0, "stop the peer population before removal");
        for p in self.clones.iter().rev() {
            c.destroy(p.id, &p.name)?;
        }
        allocated(c, 2)
    }
}

fn reply<T: Serial>(c: &Controller<T>) -> Result<&str> {
    Ok(std::str::from_utf8(
        &c.log(Source::Host)[c.last_command()..],
    )?)
}

fn allocated<T: Serial>(c: &mut Controller<T>, expected: usize) -> Result<()> {
    c.command("vm capacity")?;
    let mut rows = reply(c)?
        .lines()
        .filter_map(|l| l.strip_prefix("VM resources: allocated="));
    let row = rows.next().context("missing capacity reply")?;
    ensure!(
        row.split_once(" id-limit=")
            .is_some_and(|(n, _)| n == expected.to_string())
            && rows.next().is_none(),
        "expected {expected} allocated VMs"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
