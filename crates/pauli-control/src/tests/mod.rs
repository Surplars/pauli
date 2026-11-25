//! Observable lifecycle backend; policy checks do not execute guests.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use crate::*;
use std::cell::Cell;
use std::vec::Vec;

struct Runtime {
    vms: [Option<Info>; 4],
    next_epoch: u32,
    reads: Cell<usize>,
    log_read: Cell<Option<(VmId, u32, u64)>>,
    effects: Vec<(VmId, Operation)>,
    fail_boot: bool,
}
impl Runtime {
    fn new() -> Self {
        Self {
            vms: std::array::from_fn(|index| {
                Some(Info {
                    vm: VmId(index as u32 + 1),
                    epoch: 10 + index as u32,
                    state: State::Ready,
                    active: true,
                    vcpus: 2,
                    memory_bytes: 96 << 20,
                })
            }),
            next_epoch: 14,
            reads: Cell::new(0),
            log_read: Cell::new(None),
            effects: Vec::new(),
            fail_boot: false,
        }
    }
    fn row(&mut self, vm: VmId) -> &mut Info {
        self.vms[vm.0 as usize - 1].as_mut().unwrap()
    }
}
impl Backend for Runtime {
    fn snapshot(&self) -> impl Iterator<Item = Info> + '_ {
        self.vms.iter().flatten().copied()
    }
    fn info(&self, vm: VmId) -> Result<Info, Error> {
        self.reads.set(self.reads.get() + 1);
        let index = vm.0.checked_sub(1).ok_or(Error::NotFound)? as usize;
        self.vms
            .get(index)
            .copied()
            .flatten()
            .ok_or(Error::NotFound)
    }
    fn start(&mut self, vm: VmId) -> Result<bool, Error> {
        self.effects.push((vm, Operation::Start));
        if self.row(vm).active {
            return Ok(false);
        }
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        let fail = self.fail_boot;
        let info = self.row(vm);
        info.epoch = epoch;
        info.active = !fail;
        info.state = if fail { State::Faulted } else { State::Ready };
        if fail {
            Err(Error::Backend("boot failed"))
        } else {
            Ok(true)
        }
    }
    fn stop(&mut self, vm: VmId) -> Result<(), Error> {
        self.effects.push((vm, Operation::Stop));
        self.row(vm).active = false;
        self.row(vm).state = State::Stopped;
        Ok(())
    }
    fn input(&mut self, vm: VmId, byte: u8) -> Result<(), Error> {
        self.effects.push((vm, Operation::Input(byte)));
        Ok(())
    }
    fn console_read(&self, vm: VmId, epoch: u32, cursor: u64) -> Result<ConsoleChunk, Error> {
        self.log_read.set(Some((vm, epoch, cursor)));
        logs::read_sample(cursor)
    }
}
fn request(vm: u32, epoch: u32, operation: Operation) -> Request {
    Request {
        vm: VmId(vm),
        epoch,
        operation,
    }
}

mod logs;
mod policy;
mod scale;
mod transport;
