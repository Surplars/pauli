//! Scalar transport encoding, cancellation and concurrent channel checks.
//! Ownership/Lock Ordering: each test owns its fixture; no shared global state.
//! Invariants: all original acceptance assertions remain host-only.

use crate::{
    abi,
    queue::{self, Poll, Queue},
    *,
};
use std::vec::Vec;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn request() -> Request {
    Request {
        vm: VmId(1),
        epoch: 0,
        operation: Operation::Inspect,
    }
}
#[test]
fn provisioning_words_are_canonical_and_do_not_extend_legacy_rights() {
    let full = "alpha-0123456789";
    let lo = u64::from_le_bytes(*b"alpha-01");
    let hi = u64::from_le_bytes(*b"23456789");
    let name = ChildName::new(full).unwrap();
    assert_eq!(ChildName::from_words(lo, hi), Some(name));
    assert_eq!(
        abi::decode([1, 3, 2, 7, lo, hi]),
        Ok(Request {
            vm: VmId(2),
            epoch: 7,
            operation: Operation::Create(name.into()),
        })
    );
    for text in ["", "too-long-123456789", "7bad", "a/b", "a\0b", "a\0"] {
        assert_eq!(ChildName::new(text), None);
    }
    for words in [
        [1, 3, 2, 7, 0x610062, 0],
        [1, 3, 2, 0, lo, hi],
        [1, 3, 2, 7, 0xff, 0],
        [1, 4, 2, 7, 0, 1],
        [1, 4, 2, 0, 0, 0],
    ] {
        assert_eq!(abi::decode(words), Err(abi::INVALID_PARAM));
    }
    assert_eq!(
        abi::decode([1, 4, 2, 7, 0, 0]).unwrap().operation,
        Operation::Destroy
    );
    assert!(!Rights::ALL.contains(Rights::CREATE) && !Rights::ALL.contains(Rights::DESTROY));
    assert_eq!(
        Rights::parse("inspect,create"),
        Some(Rights::INSPECT.union(Rights::CREATE))
    );
    assert_eq!(
        abi::encode_for(Operation::Create(name.into()), Err(Error::Denied)),
        abi::Return::error(abi::DENIED)
    );
}
#[test]
fn shaped_wire_keeps_legacy_words_canonical_and_never_rounds_memory_replies() {
    let lo = u64::from_le_bytes(*b"worker\0\0");
    for (count, mib) in [(1u8, 0u16), (0, 128), (4, 1024), (16, 128)] {
        let word = 7 | u64::from(count) << 32 | u64::from(mib) << 40;
        let req = abi::decode([1, abi::CREATE_SHAPED, 2, word, lo, 0]).unwrap();
        let Operation::Create(spec) = req.operation else {
            panic!("not create")
        };
        assert_eq!(
            (
                req.epoch,
                spec.shape.vcpus.map(|v| v.get()).unwrap_or(0),
                spec.shape.memory_mib.map(|v| v.get()).unwrap_or(0)
            ),
            (7, count, mib)
        );
        assert_eq!(abi::decode([1, 3, 2, word, lo, 0]), Err(abi::INVALID_PARAM));
    }
    for args in [
        [1, abi::CREATE_SHAPED, 2, 7, lo, 0],
        [1, abi::CREATE_SHAPED, 2, 1 << 32, lo, 0],
        [1, abi::CREATE_SHAPED, 2, (1 << 56) | 7, lo, 0],
        [1, abi::MEMORY_BYTES, 2, 0, 0, 0],
        [1, abi::MEMORY_BYTES, 2, 7, lo, 0],
    ] {
        assert_eq!(abi::decode(args), Err(abi::INVALID_PARAM));
    }
    let req = abi::decode([1, abi::MEMORY_BYTES, 2, 7, 0, 0]).unwrap();
    assert_eq!(req.operation, Operation::MemoryBytes);
    let info = Info {
        vm: req.vm,
        epoch: 7,
        state: State::Stopped,
        active: false,
        vcpus: 2,
        memory_bytes: 4097,
    };
    assert_eq!(
        abi::encode_for(
            req.operation,
            Ok(Reply {
                info,
                changed: false
            })
        ),
        abi::Return {
            error: 0,
            value: 4097
        }
    );
}
#[test]
fn encoding() {
    assert!((0x0800_0000..=0x08ff_ffff).contains(&abi::EID));
    assert_eq!(abi::probe(5), (1u64 << 32) | 5);
    assert_eq!(abi::decode([1, 0, 1, 0, 0, 0]), Ok(request()));
    for (opcode, operation) in [(1, Operation::Start), (2, Operation::Stop)] {
        let decoded = abi::decode([1, opcode, u32::MAX as u64, 91, 0, 0]).unwrap();
        assert_eq!(
            (decoded.vm, decoded.epoch, decoded.operation),
            (VmId(u32::MAX), 91, operation)
        );
        assert_eq!(
            abi::decode([1, opcode, 1, 0, 0, 0]),
            Err(abi::INVALID_PARAM)
        );
    }
    // Validate scalars at the boundary, rather than silently truncating them.
    for bit in 0..64 {
        for index in [0, 3, 4, 5] {
            let mut words = [1, 0, 1, 0, 0, 0];
            words[index] ^= 1u64 << bit;
            assert_eq!(
                abi::decode(words),
                Err(if index == 0 {
                    abi::NOT_SUPPORTED
                } else {
                    abi::INVALID_PARAM
                })
            );
        }
        if bit >= 32 {
            let mut words = [1, 0, 1, 0, 0, 0];
            words[2] |= 1u64 << bit;
            assert_eq!(abi::decode(words), Err(abi::INVALID_PARAM));
        }
    }
    assert_eq!(abi::decode([1, 0, 0, 0, 0, 0]), Err(abi::INVALID_PARAM));
    assert_eq!(abi::decode([1, 3, 1, 0, 0, 0]), Err(abi::INVALID_PARAM));
}

#[test]
fn replies_and_rights() {
    for (index, state) in [
        State::Created,
        State::Ready,
        State::Running,
        State::Waiting,
        State::Stopping,
        State::Stopped,
        State::Faulted,
    ]
    .into_iter()
    .enumerate()
    {
        let reply = abi::encode(Ok(Reply {
            info: Info {
                vm: VmId(4),
                epoch: u32::MAX,
                state,
                active: true,
                vcpus: 4,
                memory_bytes: 256 << 20,
            },
            changed: true,
        }));
        assert_eq!(reply.error, 0);
        assert_eq!(reply.value as u32, u32::MAX);
        assert_eq!((reply.value >> 32) & 255, index as u64 + 1);
        assert_eq!((reply.value >> 40) & 1, 1);
        assert_eq!((reply.value >> 41) & 255, 4);
        assert_eq!((reply.value >> 49) & 1, 1);
        assert_eq!(reply.value >> 50, 0);
    }
    for (error, code) in [
        (Error::Denied, abi::DENIED),
        (Error::NotFound, abi::INVALID_PARAM),
        (Error::StaleEpoch, abi::INVALID_STATE),
        (Error::NotActive, abi::INVALID_STATE),
        (Error::NoSpace, abi::FAILED),
        (Error::Backend("private details"), abi::FAILED),
    ] {
        assert_eq!(abi::encode(Err(error)), abi::Return::error(code));
    }
    assert_eq!(
        Rights::parse("inspect,start,stop"),
        Some(Rights::INSPECT.union(Rights::START).union(Rights::STOP))
    );
    for invalid in ["", "all,stop", "inspect,", "inspect, stop", "admin"] {
        assert_eq!(Rights::parse(invalid), None);
    }
}
#[test]
fn transitions() {
    let mut queue = Queue::new();
    assert_eq!(queue.submit(0, 0, request()), Err(queue::Error::Invalid));
    let tickets: Vec<_> = (0..queue::MAX_PENDING)
        .map(|channel| {
            queue
                .submit(channel, channel as u32 + 1, request())
                .unwrap()
        })
        .collect();
    assert_eq!(
        queue.submit(128, 99, request()),
        Err(queue::Error::Exhausted)
    );
    for (channel, ticket) in tickets.into_iter().enumerate() {
        assert_eq!(
            queue.submit(channel, 99, request()),
            Err(queue::Error::Busy)
        );
        assert_eq!(queue.poll(ticket), Poll::Pending);
        let work = queue.take().unwrap();
        assert_eq!(work.ticket, ticket);
        assert_eq!(work.source, channel as u32 + 1);
        assert_eq!(queue.poll(ticket), Poll::Pending); // Dequeue is not completion.
        let result = abi::Return {
            error: 0,
            value: work.source as u64,
        };
        assert!(queue.complete(work, result));
        assert!(!queue.complete(work, result));
        assert_eq!(queue.poll(ticket), Poll::Complete(result));
        assert_eq!(queue.poll(ticket), Poll::Cancelled); // Exactly one consumption.
    }
    assert!(!queue.has_work());
    assert!(queue.take().is_none());
    // Sparse, high logical channels reuse physical queue slots without aliasing wakes.
    for channel in 16..16 + queue::MAX_PENDING {
        // Stop before dispatch means there is no request left to execute.
        let ticket = queue.submit(channel, 50, request()).unwrap();
        queue.cancel(ticket);
        assert_eq!(queue.poll(ticket), Poll::Cancelled);
        assert!(queue.take().is_none());
        // Stop while the manager holds a detached Work cannot resurrect its
        // completion, even after this channel is immediately reused.
        let old = queue.submit(channel, 51, request()).unwrap();
        let old_work = queue.take().unwrap();
        queue.cancel(old);
        let fresh = queue.submit(channel, 52, request()).unwrap();
        assert_ne!(old, fresh);
        assert!(!queue.complete(
            old_work,
            abi::Return {
                error: 0,
                value: 51
            }
        ));
        queue.cancel(old); // Cannot cancel a later incarnation's ticket.
        assert_eq!(queue.poll(fresh), Poll::Pending);
        let fresh_work = queue.take().unwrap();
        assert_eq!(fresh_work.ticket, fresh);
        let result = abi::Return {
            error: 0,
            value: 52,
        };
        assert!(queue.complete(fresh_work, result));
        assert_eq!(queue.poll(fresh), Poll::Complete(result));
    }
}
#[test]
fn concurrent() {
    const PRODUCERS: usize = 4;
    const ROUNDS: usize = 256;
    let queue = Arc::new(Mutex::new(Queue::new()));
    let done = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now() + Duration::from_secs(10);
    thread::scope(|scope| {
        for channel in 0..PRODUCERS {
            let queue = Arc::clone(&queue);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                for round in 0..ROUNDS {
                    let source = (channel * ROUNDS + round + 1) as u32;
                    let ticket = queue
                        .lock()
                        .unwrap()
                        .submit(channel, source, request())
                        .unwrap();
                    if round % 17 == 0 {
                        queue.lock().unwrap().cancel(ticket);
                        continue;
                    }
                    loop {
                        let result = queue.lock().unwrap().poll(ticket);
                        match result {
                            Poll::Complete(result) => {
                                assert_eq!(result.value, source as u64);
                                break;
                            }
                            Poll::Cancelled => panic!("live request disappeared"),
                            Poll::Pending => {}
                        }
                        assert!(Instant::now() < deadline, "producer deadline");
                        thread::yield_now();
                    }
                }
                done.fetch_add(1, Ordering::Release);
            });
        }
        while done.load(Ordering::Acquire) != PRODUCERS {
            let work = queue.lock().unwrap().take();
            if let Some(work) = work {
                thread::yield_now(); // Exercise cancellation/channel reuse after dequeue.
                queue.lock().unwrap().complete(
                    work,
                    abi::Return {
                        error: 0,
                        value: work.source as u64,
                    },
                );
            }
            assert!(Instant::now() < deadline, "manager deadline");
            thread::yield_now();
        }
    });
    assert!(!queue.lock().unwrap().has_work());
}
#[test]
fn volume_create_requires_canonical_slot_and_exact_slot_authority() {
    let name = u64::from_le_bytes(*b"child\0\0\0");
    for n in 0..4 {
        let slot = DiskSlot::new(n).unwrap();
        let selector = (u64::from(n) + 1) << 56;
        let call = |op, word| abi::decode([1, op, 2, word, name, 0]);
        let create = call(abi::CREATE_DISK, selector | 9).unwrap().operation;
        let required = Rights::CREATE.union(Rights::disk(slot));
        assert_eq!(create.right(), required);
        assert_eq!(Rights::parse(&format!("create,disk{n}")), Some(required));
        assert_eq!(
            call(abi::CREATE_DISK, selector | (2 << 32) | 9)
                .unwrap()
                .operation
                .right(),
            required.union(Rights::SHAPE)
        );
        for old in [3, abi::CREATE_SHAPED] {
            assert_eq!(call(old, selector | 9), Err(abi::INVALID_PARAM));
        }
    }
    for word in [9, 1 << 56, (5 << 56) | 9, (255 << 56) | 9] {
        assert_eq!(
            abi::decode([1, abi::CREATE_DISK, 2, word, name, 0]),
            Err(abi::INVALID_PARAM)
        );
    }
    for invalid in ["disk", "disk00", "disk4", "disk/"] {
        assert!(Rights::parse(invalid).is_none());
    }
}
