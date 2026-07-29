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
    /// The write-ahead ring would lap its live history. The pool's cue to
    /// checkpoint and retry — never silently dropped work.
    WalFull,
    /// The vdisk manifest block cannot take another entry. A stated v1
    /// ceiling, like DRBD's twelve volumes — chaining arrives if a real
    /// deployment ever nears it.
    ManifestFull,
    /// An operation named a vdisk the pool does not have.
    UnknownVdisk(u64),
    /// A create named a vdisk the pool already has.
    VdiskExists(u64),
    /// A read or write beyond the vdisk's capacity.
    OutOfRange { index: u64, capacity: u64 },
    /// A snapshot create named an id the vdisk already carries.
    SnapshotExists { vdisk: u64, snapshot: u64 },
    /// An operation named a snapshot the vdisk does not carry.
    UnknownSnapshot { vdisk: u64, snapshot: u64 },
    /// A vdisk delete refused because snapshots still pin its history —
    /// deleting them first is an explicit act, not a cascade.
    HasSnapshots(u64),
    /// A write on a node that does not hold the vdisk's writer lease.
    NotWriter(u64),
    /// A claim on a lease another node holds under the current era. Taking
    /// it needs a handover or a fence verdict, never impatience.
    LeaseHeld { vdisk: u64, holder: u8 },
    /// A handover step for a vdisk whose lease is not in that state — no
    /// window open, or open toward a different node.
    NoSuchHandover(u64),
    /// Replication cannot acknowledge: the peer is unreachable and no
    /// verdict says it is dead. Integrity over availability, always.
    Suspended,
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
            FsError::WalFull => {
                write!(
                    f,
                    "the write-ahead ring is full; a checkpoint must retire it"
                )
            }
            FsError::ManifestFull => write!(f, "the vdisk manifest cannot take another entry"),
            FsError::UnknownVdisk(id) => write!(f, "no vdisk {id} exists in this pool"),
            FsError::VdiskExists(id) => write!(f, "vdisk {id} already exists in this pool"),
            FsError::OutOfRange { index, capacity } => write!(
                f,
                "block index {index} is beyond the vdisk's capacity of {capacity} blocks"
            ),
            FsError::SnapshotExists { vdisk, snapshot } => {
                write!(f, "vdisk {vdisk} already carries snapshot {snapshot}")
            }
            FsError::UnknownSnapshot { vdisk, snapshot } => {
                write!(f, "vdisk {vdisk} carries no snapshot {snapshot}")
            }
            FsError::HasSnapshots(id) => write!(
                f,
                "vdisk {id} still has snapshots; delete them before the vdisk"
            ),
            FsError::NotWriter(id) => {
                write!(f, "this node does not hold the writer lease for vdisk {id}")
            }
            FsError::LeaseHeld { vdisk, holder } => write!(
                f,
                "node {holder} holds the writer lease for vdisk {vdisk} in this era"
            ),
            FsError::NoSuchHandover(id) => {
                write!(f, "vdisk {id} has no handover open in that direction")
            }
            FsError::Suspended => write!(
                f,
                "i/o is suspended: the peer is unreachable and not known dead"
            ),
        }
    }
}

impl std::error::Error for FsError {}
