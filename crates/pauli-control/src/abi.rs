//! Experimental scalar ABI v1. No native layouts, pointers or caller identity
//! cross this boundary. RISC-V carries the six words in a0..a5 (FID=1).
//! Ownership/Lock Ordering: by-value scalar conversion; no locks or backend access.
//! Invariants: caller identity and pointers are never accepted from request words.
use crate::{
    Call, ChildName, ChildSpec, ConsoleChunk, DiskSlot, Error, MailChunk, MailMessage, Operation,
    Reply, Request, Shape, State, StatsWord, VmId,
};
use core::num::{NonZeroU8, NonZeroU16};

pub const EID: u32 = 0x0800_4f49;
pub const VERSION: u64 = 1;
/// Highest addressable public VM ID, not a count of visible or free resources.
pub const fn probe(capacity: u32) -> u64 {
    (VERSION << 32) | capacity as u64
}
pub const FAILED: i64 = -1;
pub const NOT_SUPPORTED: i64 = -2;
pub const INVALID_PARAM: i64 = -3;
pub const DENIED: i64 = -4;
pub const INVALID_STATE: i64 = -10;
pub const CREATE_SHAPED: u64 = 5;
pub const MEMORY_BYTES: u64 = 6;
pub const CREATE_DISK: u64 = 7;
pub const CONSOLE_READ: u64 = 8;
pub const MAIL_SEND: u64 = 9;
pub const MAIL_RECV: u64 = 10;
pub const STATS_READ: u64 = 11;
/// Supported online stats protocol, independent of the caller's permission.
pub const FEAT_STATS: u64 = 1;
pub const FEAT_EVENTS: u64 = 2;
pub const EVENTS_TAKE: u64 = 0;
pub const EVENTS_MASK: u64 = 1;
pub const EVENT_MAIL: u64 = 1;
pub const EVENT_LOG: u64 = 2;
pub const EVENT_STATE: u64 = 4;
/// A subscribed MAIL binding expired. No peer identity or activity is disclosed.
pub const EVENT_RESCAN: u64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Return {
    pub error: i64,
    pub value: u64,
}
impl Return {
    pub const fn error(error: i64) -> Self {
        Self { error, value: 0 }
    }
}

/// Decode host-scoped calls before the VM protocol's nonzero resource-ID check.
pub fn decode_call(args: [u64; 6]) -> Result<Call, i64> {
    if args[1] != STATS_READ {
        return decode(args).map(Call::Vm);
    }
    if args[0] != VERSION {
        return Err(NOT_SUPPORTED);
    }
    let selector = args[3];
    if args[2] != 0 || selector >> 24 != 0 || args[4] != 0 || args[5] != 0 {
        return Err(INVALID_PARAM);
    }
    StatsWord::new(
        selector as u8,
        (selector >> 8) as u8,
        (selector >> 16) as u8,
    )
    .map(Call::StatsRead)
    .ok_or(INVALID_PARAM)
}

/// Decode a VM-targeted request.
pub fn decode(args: [u64; 6]) -> Result<Request, i64> {
    if args[0] != VERSION {
        return Err(NOT_SUPPORTED);
    }
    let epoch = args[3] as u32;
    if args[2] == 0
        || args[2] > u32::MAX as u64
        || (!matches!(args[1], CREATE_SHAPED | CREATE_DISK | MAIL_SEND)
            && args[3] > u32::MAX as u64)
        || (!matches!(
            args[1],
            3 | CREATE_SHAPED | CREATE_DISK | CONSOLE_READ | MAIL_SEND
        ) && (args[4] != 0 || args[5] != 0))
    {
        return Err(INVALID_PARAM);
    }
    let operation = match args[1] {
        0 if args[3] == 0 => Operation::Inspect,
        1 if args[3] != 0 => Operation::Start,
        2 if args[3] != 0 => Operation::Stop,
        3 | CREATE_SHAPED | CREATE_DISK if epoch != 0 => {
            // Bits 32..39: vCPU count; 40..55: MiB; zero inherits one field.
            if args[1] == CREATE_SHAPED && (args[3] >> 56 != 0 || args[3] >> 32 == 0) {
                return Err(INVALID_PARAM);
            }
            Operation::Create(ChildSpec {
                name: ChildName::from_words(args[4], args[5]).ok_or(INVALID_PARAM)?,
                // Only the new opcode consumes the high byte: one-based, zero invalid.
                disk: if args[1] == CREATE_DISK {
                    Some(
                        ((args[3] >> 56) as u8)
                            .checked_sub(1)
                            .and_then(DiskSlot::new)
                            .ok_or(INVALID_PARAM)?,
                    )
                } else {
                    None
                },
                shape: Shape {
                    vcpus: NonZeroU8::new((args[3] >> 32) as u8),
                    memory_mib: NonZeroU16::new((args[3] >> 40) as u16),
                },
            })
        }
        4 if args[3] != 0 => Operation::Destroy,
        MEMORY_BYTES if epoch != 0 => Operation::MemoryBytes,
        CONSOLE_READ if epoch != 0 && args[5] == 0 => Operation::ConsoleRead(args[4]),
        MAIL_SEND if args[3] >> 48 == 0 && args[5] == 0 => {
            // The length occupies bits 32..35; reading the full byte also
            // rejects every reserved bit between the length and tag.
            let len = (args[3] >> 32) as u8;
            if len > 6 || args[4] >> (8 * len) != 0 {
                return Err(INVALID_PARAM);
            }
            let mut data = [0; 6];
            data.copy_from_slice(&args[4].to_le_bytes()[..6]);
            Operation::MailSend(MailMessage {
                tag: (args[3] >> 40) as u8,
                len,
                data,
            })
        }
        MAIL_RECV => Operation::MailRecv,
        _ => return Err(INVALID_PARAM),
    };
    Ok(Request {
        vm: VmId(args[2] as u32),
        epoch,
        operation,
    })
}

/// CREATE returns the newly assigned (one-based resource ID, epoch). Existing
/// operations retain their v1 status word, including its reserved zero bits.
pub fn encode_for(operation: Operation, result: Result<Reply, Error>) -> Return {
    match (operation, result) {
        (Operation::Create(_), Ok(reply)) => Return {
            error: 0,
            value: (u64::from(reply.info.vm.0) << 32) | u64::from(reply.info.epoch),
        },
        (Operation::MemoryBytes, Ok(reply)) => Return {
            error: 0,
            value: reply.info.memory_bytes,
        },
        (Operation::ConsoleRead(_), Ok(reply)) => match reply.log {
            Some(chunk) => encode_log(chunk),
            None => Return::error(FAILED),
        },
        (Operation::MailSend(_), Ok(_)) => Return { error: 0, value: 0 },
        (Operation::MailRecv, Ok(reply)) => match reply.mail {
            Some(chunk) => encode_mail(chunk),
            None => Return::error(FAILED),
        },
        _ => encode(result),
    }
}

fn encode_mail(chunk: MailChunk) -> Return {
    let Some(message) = chunk.message else {
        return if chunk.more {
            Return::error(FAILED)
        } else {
            Return { error: 0, value: 0 }
        };
    };
    if !message.canonical() {
        return Return::error(FAILED);
    }
    let mut bytes = [0; 8];
    bytes[..6].copy_from_slice(&message.data);
    Return {
        error: 0,
        value: u64::from_le_bytes(bytes)
            | (u64::from(message.len) << 48)
            | (1 << 51)
            | (u64::from(chunk.more) << 52)
            | (u64::from(message.tag) << 53),
    }
}

fn encode_log(chunk: ConsoleChunk) -> Return {
    let value = match chunk {
        ConsoleChunk::Bytes { data, len } => {
            let Some(payload) = data.get(..usize::from(len)) else {
                return Return::error(INVALID_PARAM);
            };
            let mut bytes = [0u8; 8];
            bytes[..payload.len()].copy_from_slice(payload);
            u64::from_le_bytes(bytes) | (u64::from(len) << 48)
        }
        ConsoleChunk::Overrun { resume } if resume < 1 << 48 => resume | (1 << 51),
        ConsoleChunk::Overrun { .. } => return Return::error(INVALID_PARAM),
    };
    Return { error: 0, value }
}

/// Explicit codes, independent of Rust enum discriminants. The upper bits of
/// the metadata word are zero; physical addresses never enter this response.
pub fn encode(result: Result<Reply, Error>) -> Return {
    match result {
        Ok(reply) => {
            let state = match reply.info.state {
                State::Created => 1,
                State::Ready => 2,
                State::Running => 3,
                State::Waiting => 4,
                State::Stopping => 5,
                State::Stopped => 6,
                State::Faulted => 7,
            };
            Return {
                error: 0,
                value: u64::from(reply.info.epoch)
                    | (state << 32)
                    | (u64::from(reply.info.active) << 40)
                    | (u64::from(reply.info.vcpus) << 41)
                    | (u64::from(reply.changed) << 49),
            }
        }
        Err(error) => Return::error(match error {
            Error::Denied => DENIED,
            Error::NotFound | Error::Invalid => INVALID_PARAM,
            Error::StaleEpoch | Error::NotActive | Error::Busy => INVALID_STATE,
            Error::Unsupported => NOT_SUPPORTED,
            Error::NoSpace | Error::Backend(_) => FAILED,
        }),
    }
}
