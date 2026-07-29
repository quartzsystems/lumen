//! LumenFS engine core: content-addressed, log-structured block storage.
//!
//! This crate is the beginning of the pooled-storage engine designed in
//! docs/lumenfs.md — phase 1, the single-node engine, which the design doc
//! requires to start with the test bed rather than the format. Everything
//! here is a pure state machine over one narrow seam ([`disk::Disk`]): no
//! threads, no async, no clocks, no ambient randomness. That is what lets
//! the crash-consistency suite in tests/ run thousands of simulated
//! power-loss histories under plain `cargo test`, each replayable from its
//! seed.
//!
//! ```text
//!   error.rs    what can go wrong, and the corrupt/missing distinction
//!   hash.rs     the content address — a BLAKE3-256 that is also the checksum
//!   disk.rs     the one seam to hardware: read, write, flush-barrier
//!   sim.rs      the deterministic disk: torn writes, reordering, power loss
//!   format.rs   the on-disk shapes: superblock, anchor, segment, record
//!   brick.rs    one disk's extent store: format, recover, put, get
//!   wal.rs      the write-ahead ring: durable before any tree knows
//!   map.rs      vdisk maps: COW radix trees whose nodes are pool blocks
//!   pool.rs     vdisks over the brick: write, read, flush, checkpoint
//! ```
//!
//! The durability contract, stated once and tested everywhere: a block is
//! guaranteed only after [`brick::Brick::flush`] returns. A crash may keep,
//! drop, or tear any write since the last flush — independently, in any
//! combination — and recovery must surface every acknowledged block intact
//! while never returning corrupt data for any block. Unacknowledged blocks
//! may survive or vanish; both are correct.

pub mod brick;
pub mod disk;
pub mod error;
pub mod format;
pub mod hash;
pub mod map;
pub mod pool;
pub mod sim;
pub mod wal;

pub use brick::{Brick, BrickParams, BrickStats};
pub use disk::Disk;
pub use error::{FsError, Result};
pub use format::{SECTOR_SIZE, SUPERBLOCK_SLOTS};
pub use hash::{hash_block, BlockHash};
pub use pool::Pool;
pub use sim::{SimDisk, SplitMix64};
