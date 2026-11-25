//! Host telemetry uses its own request and grant, scoped to the actual caller's epoch.
use crate::{Backend, Caller, Error, Operation, Request, Service, abi, grants::Entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatsWord {
    section: u8,
    owner: u8,
    word: u8,
}
impl StatsWord {
    /// Static v2 wire bounds. The backend checks the configured number of owners.
    pub const fn new(section: u8, owner: u8, word: u8) -> Option<Self> {
        match section {
            0 if owner == 0 && word < 8 => Some(Self {
                section,
                owner,
                word,
            }),
            1 if owner < 64 && word < 16 => Some(Self {
                section,
                owner,
                word,
            }),
            _ => None,
        }
    }
    pub const fn section(self) -> u8 {
        self.section
    }
    pub const fn owner(self) -> u8 {
        self.owner
    }
    pub const fn word(self) -> u8 {
        self.word
    }

    /// Supervisor trap CSRs are present in halted debugger dumps only.
    pub const fn is_redacted(self) -> bool {
        self.section == 1 && self.word >= 9 && self.word <= 11
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Vm(Request),
    StatsRead(StatsWord),
}
impl From<Request> for Call {
    fn from(request: Request) -> Self {
        Self::Vm(request)
    }
}
impl Call {
    pub const fn vm_operation(self) -> Option<Operation> {
        match self {
            Self::Vm(request) => Some(request.operation),
            Self::StatsRead(_) => None,
        }
    }
}

impl Service {
    pub fn grant_stats(
        &mut self,
        caller: Caller,
        source: u32,
        backend: &impl Backend,
    ) -> Result<(), Error> {
        if caller != Caller::Host {
            return Err(Error::Denied);
        }
        if source == 0 || !backend.snapshot().any(|vm| vm.epoch == source && vm.active) {
            return Err(Error::NotFound);
        }
        self.grants.set(Entry::HostStats(source), backend)
    }

    pub fn revoke_stats(&mut self, caller: Caller, source: u32) -> Result<(), Error> {
        if caller != Caller::Host {
            return Err(Error::Denied);
        }
        self.grants.remove(|entry| entry.key() == (source, None));
        Ok(())
    }

    pub fn execute_call(
        &mut self,
        caller: Caller,
        call: Call,
        backend: &mut impl Backend,
    ) -> abi::Return {
        match call {
            Call::Vm(request) => {
                abi::encode_for(request.operation, self.execute(caller, request, backend))
            }
            Call::StatsRead(word) => match self.read_stats(caller, word, backend) {
                Ok(value) => abi::Return { error: 0, value },
                Err(error) => abi::encode(Err(error)),
            },
        }
    }

    fn read_stats(
        &self,
        caller: Caller,
        word: StatsWord,
        backend: &impl Backend,
    ) -> Result<u64, Error> {
        let Caller::Guest(source) = caller else {
            return Err(Error::Denied);
        };
        if self.grants.get(source, None).is_none() {
            return Err(Error::Denied);
        }
        Self::sender(caller, backend)?;
        // Availability and runtime bounds apply even to redacted words.
        let value = backend.stats_read(word)?;
        Ok(if word.is_redacted() { 0 } else { value })
    }
}
