//! Native block seam for deterministic retirement/flush tests, no disk is opened.
//! Ownership/Lock Ordering: private fixture; bridge owns every submitted token.
//! Invariants: replacement is forbidden before ordered durable completion.
use crate::{
    backends::block::{BUFFER_BYTES, Block},
    memory::Arena,
};
use core::sync::atomic::{AtomicBool, Ordering};
use pauli_hal::{device::*, sync::SpinLock};
use std::{boxed::Box, vec, vec::Vec};
pub(super) struct Native {
    work: SpinLock<Option<(BlockToken, BlockSubmission)>>,
    submitted: SpinLock<Vec<BlockOperation>>,
    done: AtomicBool,
    fail: AtomicBool,
    cached: SpinLock<[u8; 512]>,
    pub(super) durable: SpinLock<[u8; 512]>,
}
impl Native {
    pub(super) fn new() -> &'static Self {
        Box::leak(Box::new(Self {
            work: SpinLock::new(None),
            submitted: SpinLock::new(Vec::new()),
            done: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            cached: SpinLock::new([0; 512]),
            durable: SpinLock::new([0; 512]),
        }))
    }
    pub(super) fn pending(&self) -> Option<BlockOperation> {
        self.work.lock().as_ref().map(|(_, req)| req.operation)
    }
    pub(super) fn finish(&self, operation: BlockOperation, ok: bool) {
        let request = self.work.lock().unwrap().1;
        assert_eq!(request.operation, operation);
        self.fail.store(!ok, Ordering::Relaxed);
        self.done.store(true, Ordering::Release);
        if let Some(event) = request.event {
            event.notify();
        }
    }
}
impl HostBlockDevice for Native {
    fn sectors(&self) -> u64 {
        64
    }
    fn writable(&self) -> bool {
        true
    }
    fn supports_flush(&self) -> bool {
        true
    }
    fn max_transfer_bytes(&self) -> usize {
        4096
    }
    fn submit(&self, request: BlockSubmission, input: &[u8]) -> Result<BlockToken> {
        let mut work = self.work.lock();
        if work.is_some() {
            return Err(DeviceError::Busy);
        }
        let mut submitted = self.submitted.lock();
        submitted.push(request.operation);
        let token = BlockToken::new(1, submitted.len() as u64, 0);
        if request.operation == BlockOperation::Write {
            assert_eq!(request.sector, 0);
            self.cached.lock().copy_from_slice(input);
        }
        *work = Some((token, request));
        self.done.store(false, Ordering::Release);
        Ok(token)
    }
    fn try_poll(&self, token: BlockToken) -> Result<Option<usize>> {
        let work = self.work.lock();
        let (_, request) = work
            .filter(|(id, _)| *id == token)
            .ok_or(DeviceError::Stale)?;
        if !self.done.load(Ordering::Acquire) {
            return Ok(None);
        }
        if self.fail.load(Ordering::Relaxed) {
            return Err(DeviceError::Fault);
        }
        Ok(Some(request.bytes))
    }
    fn collect(&self, token: BlockToken, out: &mut [u8]) -> Result<usize> {
        let size = self.try_poll(token)?.ok_or(DeviceError::NotReady)?;
        match self.work.lock().take().unwrap().1.operation {
            BlockOperation::Flush => *self.durable.lock() = *self.cached.lock(),
            BlockOperation::Read => out.copy_from_slice(&*self.cached.lock()),
            BlockOperation::Write => {}
        }
        Ok(size)
    }
    fn cancel(&self, _token: BlockToken) -> Result<()> {
        *self.work.lock() = None;
        Ok(())
    }
    fn service(&self, _now: u64) -> Result<usize> {
        Ok(0)
    }
}
#[test]
fn writable_stop_requires_final_flush_completion() {
    let native = Native::new();
    let mut arena = Arena::new(Box::leak(vec![0; BUFFER_BYTES].into_boxed_slice()));
    let event = Box::leak(Box::new(CompletionEvent::new()));
    let mut disk = Block::bind(event, native, 64, true, 1_000_000, &mut arena).unwrap();
    assert!(!disk.stop(1).unwrap());
    assert!(!disk.stop(2).unwrap());
    assert!(disk.reset().is_err());
    assert_eq!(&*native.submitted.lock(), &[BlockOperation::Flush]);
    native.done.store(true, Ordering::Release);
    assert!(disk.stop(3).unwrap());
    assert!(disk.stop(4).unwrap());
    assert_eq!(&*native.submitted.lock(), &[BlockOperation::Flush]);
    assert_eq!(disk.device.completed, 0);
    disk.reset().unwrap();
    assert!(!disk.stop(5).unwrap());
    assert_eq!(native.submitted.lock().len(), 2);
}
