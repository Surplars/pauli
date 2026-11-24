//! Bounded local command grammar; parsing never starts/stops a VM.
//! Ownership/Lock Ordering: borrows one console line on the sole manager.
//! Invariants: reject extra tokens and malformed rights before any side effect.
use crate::{HvError, Result, manager::Budget};
use pauli_control::{ChildName, ChildSpec, DiskSlot};
#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Empty,
    Help,
    List,
    Capacity,
    Disks,
    Quota(&'a str),
    SetQuota(&'a str, Option<Budget>),
    Status(Option<&'a str>),
    #[cfg(feature = "selftest")]
    Fences(&'a str),
    Create(&'a str, ChildSpec),
    Destroy(&'a str),
    Start(&'a str, bool),
    Attach(&'a str),
    Detach,
    Stop(&'a str),
    Shutdown,
    Grant(&'a str, &'a str, &'a str),
    Revoke(&'a str, &'a str),
}
pub fn parse(line: &str) -> Result<Command<'_>> {
    let mut fields = [""; 10]; // create with CPU, RAM and one volume override
    let mut count = 0;
    for word in line.split_ascii_whitespace() {
        let field = fields.get_mut(count).ok_or(HvError::InvalidCommand)?;
        *field = word;
        count += 1;
    }
    Ok(match &fields[..count] {
        [] => Command::Empty,
        ["help"] => Command::Help,
        ["shutdown"] => Command::Shutdown,
        ["vm", "list"] => Command::List,
        ["vm", "capacity"] => Command::Capacity,
        ["vm", "disks"] => Command::Disks,
        ["vm", "quota", name] => Command::Quota(name),
        ["vm", "quota", name, "clear"] => Command::SetQuota(name, None),
        ["vm", "quota", name, guests, vcpus, memory] => {
            let number = |s: &str| s.parse::<u32>().map_err(|_| HvError::InvalidCommand);
            Command::SetQuota(
                name,
                Some(Budget::from_mib(
                    number(guests)?,
                    number(vcpus)?,
                    number(memory)?,
                )),
            )
        }
        ["vm", "status"] => Command::Status(None),
        ["vm", "status", name] => Command::Status(Some(name)),
        #[cfg(feature = "selftest")]
        ["vm", "fences", name] => Command::Fences(name),
        ["vm", "create", template, name, options @ ..] => {
            Command::Create(template, child(name, options)?)
        }
        ["vm", "destroy", name] => Command::Destroy(name),
        ["vm", "start", name] => Command::Start(name, false),
        ["vm", "start", name, "--attach"] => Command::Start(name, true),
        ["vm", "attach", name] => Command::Attach(name),
        ["vm", "detach"] => Command::Detach,
        ["vm", "stop", name] => Command::Stop(name),
        ["vm", "grant", source, target, rights] if pauli_control::Rights::parse(rights).is_some() => {
            Command::Grant(source, target, rights)
        }
        ["vm", "revoke", source, target] => Command::Revoke(source, target),
        _ => return Err(HvError::InvalidCommand),
    })
}

fn child(name: &str, options: &[&str]) -> Result<ChildSpec> {
    let name = ChildName::new(name).ok_or(HvError::InvalidCommand)?;
    let mut spec = ChildSpec::from(name);
    for pair in options.chunks(2) {
        match pair {
            ["--vcpus", n] if spec.shape.vcpus.is_none() => {
                spec.shape.vcpus = Some(n.parse().map_err(|_| HvError::InvalidCommand)?);
            }
            ["--memory-mib", n] if spec.shape.memory_mib.is_none() => {
                spec.shape.memory_mib = Some(n.parse().map_err(|_| HvError::InvalidCommand)?);
            }
            ["--disk", n] if spec.disk.is_none() => {
                spec.disk = Some(
                    n.parse()
                        .ok()
                        .and_then(DiskSlot::new)
                        .ok_or(HvError::InvalidCommand)?,
                );
            }
            _ => return Err(HvError::InvalidCommand),
        }
    }
    Ok(spec)
}
