//! Reclaimable, host-only copied channels. A published child holds its side until destruction.
//! Management is the only provisioner. Device I/O borrows the shared channel under its own lock.
use crate::{
    HvError, Result,
    arch::HvArch,
    memory::pool::{DomainPool, Receipt},
};
use core::{
    mem::MaybeUninit,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};
use pauli_config::NetLease;
use pauli_control::VmId;
use pauli_devices::net_link::Channel;
use pauli_hal::sync::SpinLock;

struct Shared {
    users: AtomicUsize,
    channel: SpinLock<&'static mut Channel>,
    receipt: Receipt,
}

// Both parts occupy the same charged extent. Channel initialization never places the frame store
// on the native stack; the small ownership header can move out on last release.
#[repr(C)]
struct Storage {
    channel: MaybeUninit<Channel>,
    header: MaybeUninit<Shared>,
}

pub(crate) struct LeaseHandle(NonNull<Shared>);
// SAFETY: each handle pins the allocation. All channel access uses the shared spinlock.
unsafe impl Send for LeaseHandle {}
unsafe impl Sync for LeaseHandle {}

impl LeaseHandle {
    pub(crate) const BYTES: usize = DomainPool::host_bytes::<Storage>();

    pub(super) fn new(pool: &'static DomainPool, epoch: u32) -> Result<Self> {
        let (object, receipt) = pool.host_object::<Storage>()?;
        // SAFETY: Storage consists entirely of MaybeUninit fields, with no validity precondition.
        let storage = unsafe { object.assume_init_mut() };
        // SAFETY: Channel documents an all-zero, inert representation. Initialize it at its final
        // address, then set the unique incarnation before publishing any handle.
        let channel = unsafe {
            storage.channel.as_mut_ptr().write_bytes(0, 1);
            storage.channel.assume_init_mut()
        };
        channel.initialize(epoch);
        let header = storage.header.write(Shared {
            users: AtomicUsize::new(1),
            channel: SpinLock::new(channel),
            receipt,
        });
        Ok(Self(NonNull::from(header)))
    }

    fn shared(&self) -> &Shared {
        // SAFETY: this handle holds one live reference until Drop.
        unsafe { self.0.as_ref() }
    }

    pub(crate) fn with_channel<T>(&self, access: impl FnOnce(&mut Channel) -> T) -> T {
        access(&mut self.shared().channel.lock())
    }
}

impl Clone for LeaseHandle {
    fn clone(&self) -> Self {
        self.shared()
            .users
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("too many live channel handles");
        Self(self.0)
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        if self.shared().users.fetch_sub(1, Ordering::AcqRel) == 1 {
            // SAFETY: the last handle owns the header and channel exclusively; the acquire observes
            // every prior user's release. No device or pending wake holds a bare pointer.
            let Shared {
                channel, receipt, ..
            } = unsafe { self.0.read() };
            channel.lock().scrub();
            drop(channel);
            // SAFETY: the channel's buffers have been erased and every borrower has retired.
            unsafe { receipt.erase_and_release() };
        }
    }
}

impl<A: HvArch> super::VmManager<A> {
    /// Published endpoints reserve even stopped/faulted sides. The sibling's VM pin keeps the
    /// handle valid until we have acquired our own. No channel lock is taken while scanning.
    pub(super) fn sibling_channel(
        &self,
        request: NetLease,
        sponsor: Option<VmId>,
    ) -> Result<Option<LeaseHandle>> {
        let mut sibling = None;
        for vm in self.vms() {
            let Some(lease) = vm.config.net_lease.filter(|l| l.channel == request.channel) else {
                continue;
            };
            if vm.sponsor != sponsor {
                return Err(HvError::Permission);
            }
            if lease.side == request.side {
                return Err(HvError::Busy);
            }
            sibling = Some(
                vm.devices
                    .io
                    .lock()
                    .net
                    .as_ref()
                    .and_then(|net| net.lease_handle())
                    .ok_or(HvError::InvalidState)?,
            );
        }
        Ok(sibling)
    }
}

#[cfg(test)]
#[test]
fn last_channel_owner_scrubs_exact_pool_extent_before_reuse() {
    use crate::memory::pool::FramePool;
    use pauli_devices::{
        net_link::Side,
        virtio_net::{FRAME_MAX, NetBackend},
    };
    use pauli_hal::addr::PAGE_SIZE;
    use std::{boxed::Box, vec};

    const BYTES: usize = LeaseHandle::BYTES;
    let backing = Box::leak(vec![0xadu8; BYTES + PAGE_SIZE].into_boxed_slice());
    let base = (backing.as_mut_ptr() as usize).next_multiple_of(PAGE_SIZE);
    let frames = FramePool::new(base, BYTES).unwrap();
    // SAFETY: this test transfers the entire aligned bank exclusively to the allocator.
    let pool = Box::leak(Box::new(unsafe { DomainPool::from_boot(frames) }));
    let first = LeaseHandle::new(pool, 7).unwrap();
    let second = first.clone();
    first.with_channel(|channel| {
        let left = channel.bind(Side::A, 150, 7).unwrap();
        let right = channel.bind(Side::B, 991, 9).unwrap();
        channel.activate(left).unwrap();
        channel.activate(right).unwrap();
        channel
            .backend(left)
            .unwrap()
            .transmit(&[0x95; FRAME_MAX])
            .unwrap();
        channel
            .backend(right)
            .unwrap()
            .transmit(&[0x8b; FRAME_MAX])
            .unwrap();
    });
    drop(first);
    assert_eq!(pool.free_pages(), 0);
    drop(second);
    let (reused, receipt) = pool.host_object::<[u8; BYTES]>().unwrap();
    assert_eq!(reused.as_ptr().addr(), base);
    // SAFETY: the final owner explicitly initialized every byte with volatile stores. The free
    // list writes only its two-word header, whose pointer/size bytes are outside the assertion.
    let bytes = unsafe { reused.assume_init_ref() };
    assert!(bytes[2 * size_of::<usize>()..].iter().all(|&b| b == 0));
    // SAFETY: the test's read-only inspection has completed.
    unsafe { receipt.release() };
}
