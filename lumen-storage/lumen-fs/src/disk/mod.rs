//! The one seam between the engine and hardware.
//!
//! Everything above this trait is deterministic and simulation-testable;
//! everything below it (io_uring, real devices) belongs to the daemon and
//! arrives in a later stage. The contract is deliberately the weakest thing
//! real disks provide, so the engine never leans on a guarantee a device
//! might not give:
//!
//! - `write_at` is *not* durable and *not* atomic. A crash may keep the
//!   write, drop it, or keep an arbitrary prefix of it — independently per
//!   write, in any combination.
//! - `read_at` observes every prior write, durable or not (the page-cache
//!   view a process sees between its own writes and a crash).
//! - `flush` is the only promise: when it returns, every prior write is
//!   durable, in its entirety. There is no partial flush.

use crate::error::Result;

/// The real thing: a file or block device, sized by seeking.
pub mod file;
/// The tortured thing: the deterministic crash-simulation disk.
pub mod sim;

/// A disk's durability barrier, detached from the disk's `&mut`: callable
/// while somebody else holds the disk, because a barrier only waits — it
/// never changes what the bytes are.
pub type FlushHandle = Box<dyn Fn() -> Result<()> + Send>;

/// A detached positional writer: gathers the slices to one offset, in
/// order, without holding the disk's `&mut`. The counterpart of
/// [`FlushHandle`] for the reserve/write/publish put path — the engine
/// reserves an extent under its lock, hands one of these out, and the
/// caller lands the bytes with the lock released. Safe for exactly the
/// reason the reservation exists: nothing else may touch a reserved
/// extent until it is published or abandoned.
pub type WriteHandle = Box<dyn Fn(u64, &[std::io::IoSlice<'_>]) -> Result<()> + Send + Sync>;

// `Send` because the engine that owns disks lives behind a mutex shared
// across threads, and a multi-brick flush syncs bricks concurrently —
// both already demand it of any real implementation.
pub trait Disk: Send {
    /// Total size in bytes. Fixed for the life of the handle.
    fn size(&self) -> u64;

    /// Fill `buf` from `offset`. Errors if the range leaves the disk.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Write `data` at `offset` — visible to reads immediately, durable only
    /// after the next `flush`. Errors if the range leaves the disk.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()>;

    /// The durability barrier. Everything written before this call is on
    /// stable storage when it returns.
    fn flush(&mut self) -> Result<()>;

    /// A [`FlushHandle`] for this disk, or `None` if it cannot offer one
    /// (the simulator: its durability is a model, not a syscall). The
    /// two-phase checkpoint drains writeback through these outside the
    /// engine lock; a `None` anywhere sends the caller back to the
    /// in-lock single-phase path.
    fn flush_handle(&self) -> Option<FlushHandle> {
        None
    }

    /// A [`WriteHandle`] for this disk, or `None` if it cannot offer one
    /// (the simulator: its writes must stay inside the deterministic
    /// model). The reserve/write/publish put path lands payloads through
    /// these outside the engine lock; a `None` sends the caller back to
    /// the in-lock single-call put.
    fn write_handle(&self) -> Option<WriteHandle> {
        None
    }
}
