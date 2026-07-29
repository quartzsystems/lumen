//! What can go wrong in the engine, as one typed enum.
//!
//! The distinction this module exists to keep honest is corrupt versus
//! missing. A block the index does not know is [`None`] from a lookup — an
//! ordinary answer. A block the index *does* know whose bytes fail their
//! hash is [`FsError::Corrupt`] — never a silent `None`, because the caller
//! (eventually the replication layer) must know to repair from a peer rather
//! than conclude the block never existed.

use std::fmt;

pub type Result<T> = std::result::Result<T, FsError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// A read or write fell outside the disk. Always an engine bug or a
    /// geometry mismatch, never an expected runtime answer.
    OutOfBounds {
        offset: u64,
        len: u64,
        disk_size: u64,
    },
    /// The disk carries no valid LumenFS superblock in either slot.
    NotFormatted,
    /// A valid superblock, but a format version this build does not speak.
    UnsupportedVersion(u32),
    /// The requested geometry cannot be laid out on this disk.
    BadGeometry(&'static str),
    /// Stored bytes contradict their own integrity data. The payload names
    /// where, for the log line; the caller's reaction is repair, not retry.
    Corrupt(&'static str),
    /// A put larger than the pool's block size.
    PayloadTooLarge { len: usize, block_size: u32 },
    /// A put of zero bytes — a block that cannot round-trip.
    EmptyPayload,
    /// No free segment can take the next block. GC arrives later in phase 1;
    /// until then a full brick refuses further puts rather than wedging.
    Full,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::OutOfBounds {
                offset,
                len,
                disk_size,
            } => write!(
                f,
                "i/o of {len} bytes at offset {offset} falls outside a {disk_size}-byte disk"
            ),
            FsError::NotFormatted => write!(f, "no valid LumenFS superblock in either slot"),
            FsError::UnsupportedVersion(v) => {
                write!(f, "superblock format version {v} is not supported")
            }
            FsError::BadGeometry(why) => write!(f, "unusable geometry: {why}"),
            FsError::Corrupt(what) => write!(f, "corruption detected: {what}"),
            FsError::PayloadTooLarge { len, block_size } => write!(
                f,
                "payload of {len} bytes exceeds the pool block size of {block_size}"
            ),
            FsError::EmptyPayload => write!(f, "a block must carry at least one byte"),
            FsError::Full => write!(f, "no free segment remains on this brick"),
        }
    }
}

impl std::error::Error for FsError {}
