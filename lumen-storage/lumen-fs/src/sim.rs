//! The deterministic disk: the test bed docs/lumenfs.md says must exist
//! before the format does.
//!
//! [`SimDisk`] models the weakest storage the [`crate::disk::Disk`] contract
//! allows. Writes land in a pending list; reads see them immediately (the
//! page-cache view); [`SimDisk::flush`] makes them durable in order; and
//! [`SimDisk::crash`] is a power loss — every pending write independently
//! kept whole, dropped, or torn to a prefix, decided by a seeded generator.
//! The same seed replays the same failure, byte for byte, which is what
//! turns "a crash test failed" from an anecdote into a debuggable artifact:
//! every assertion in the crash suite carries its seed.
//!
//! There is deliberately no wall clock and no OS randomness anywhere in this
//! crate; [`SplitMix64`] is the one generator, and the caller seeds it.

use crate::disk::Disk;
use crate::error::{FsError, Result};

/// A tiny, well-known deterministic generator. Not cryptographic, and must
/// never be used for anything secret — it exists so simulations replay.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound`. `bound` must be non-zero.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }

    /// True with probability `percent / 100`.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next_below(100) < percent
    }

    /// Fill a buffer with deterministic bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

/// One write that has been accepted but not yet flushed.
#[derive(Debug, Clone)]
struct PendingWrite {
    offset: u64,
    data: Vec<u8>,
}

/// What a power loss did to one pending write — recorded so a failing test
/// can print the exact history that broke the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashFate {
    Kept,
    Dropped,
    /// Kept only the first `len` bytes of the write.
    Torn {
        len: usize,
    },
}

#[derive(Debug)]
pub struct SimDisk {
    /// What survives a crash.
    durable: Vec<u8>,
    /// Writes since the last flush, in issue order.
    pending: Vec<PendingWrite>,
    rng: SplitMix64,
    /// Counters a test can assert on or print with a failure.
    pub writes: u64,
    pub flushes: u64,
    pub crashes: u64,
}

impl SimDisk {
    pub fn new(size: u64, seed: u64) -> Self {
        SimDisk {
            durable: vec![0u8; size as usize],
            pending: Vec::new(),
            rng: SplitMix64::new(seed),
            writes: 0,
            flushes: 0,
            crashes: 0,
        }
    }

    fn check_bounds(&self, offset: u64, len: usize) -> Result<()> {
        let end = offset.checked_add(len as u64);
        match end {
            Some(end) if end <= self.durable.len() as u64 => Ok(()),
            _ => Err(FsError::OutOfBounds {
                offset,
                len: len as u64,
                disk_size: self.durable.len() as u64,
            }),
        }
    }

    /// Power loss. Every pending write is independently kept, dropped, or
    /// torn; the pending list is gone either way. Returns each write's fate
    /// in issue order, for the failure narrative.
    pub fn crash(&mut self) -> Vec<CrashFate> {
        self.crashes += 1;
        let pending = std::mem::take(&mut self.pending);
        let mut fates = Vec::with_capacity(pending.len());
        for write in pending {
            let fate = if self.rng.chance(50) {
                CrashFate::Kept
            } else if self.rng.chance(60) {
                CrashFate::Dropped
            } else {
                CrashFate::Torn {
                    len: self.rng.next_below(write.data.len() as u64 + 1) as usize,
                }
            };
            match fate {
                CrashFate::Kept => self.apply(&write, write.data.len()),
                CrashFate::Dropped => {}
                CrashFate::Torn { len } => self.apply(&write, len),
            }
            fates.push(fate);
        }
        fates
    }

    fn apply(&mut self, write: &PendingWrite, len: usize) {
        let start = write.offset as usize;
        self.durable[start..start + len].copy_from_slice(&write.data[..len]);
    }
}

impl Disk for SimDisk {
    fn size(&self) -> u64 {
        self.durable.len() as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_bounds(offset, buf.len())?;
        let start = offset as usize;
        buf.copy_from_slice(&self.durable[start..start + buf.len()]);
        // Overlay pending writes in issue order: reads observe the latest
        // write whether or not it is durable, exactly as a page cache would.
        let read_end = offset + buf.len() as u64;
        for write in &self.pending {
            let write_end = write.offset + write.data.len() as u64;
            if write.offset >= read_end || write_end <= offset {
                continue;
            }
            let from = write.offset.max(offset);
            let to = write_end.min(read_end);
            let src = (from - write.offset) as usize..(to - write.offset) as usize;
            let dst = (from - offset) as usize..(to - offset) as usize;
            buf[dst].copy_from_slice(&write.data[src]);
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check_bounds(offset, data.len())?;
        self.writes += 1;
        self.pending.push(PendingWrite {
            offset,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.flushes += 1;
        let pending = std::mem::take(&mut self.pending);
        for write in &pending {
            self.apply(write, write.data.len());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_sees_an_unflushed_write() {
        let mut disk = SimDisk::new(4096, 1);
        disk.write_at(100, b"hello").unwrap();
        let mut buf = [0u8; 5];
        disk.read_at(100, &mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn a_flushed_write_survives_a_crash() {
        let mut disk = SimDisk::new(4096, 2);
        disk.write_at(0, b"durable").unwrap();
        disk.flush().unwrap();
        disk.crash();
        let mut buf = [0u8; 7];
        disk.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"durable");
    }

    #[test]
    fn a_crash_with_the_same_seed_replays_the_same_fates() {
        let run = |seed: u64| {
            let mut disk = SimDisk::new(1 << 16, seed);
            for i in 0..32u64 {
                disk.write_at(i * 512, &[i as u8; 512]).unwrap();
            }
            disk.crash()
        };
        assert_eq!(run(42), run(42));
        assert_ne!(run(42), run(43));
    }

    #[test]
    fn an_out_of_bounds_write_is_refused_whole() {
        let mut disk = SimDisk::new(1024, 3);
        let err = disk.write_at(1000, &[0u8; 100]).unwrap_err();
        assert!(matches!(err, FsError::OutOfBounds { .. }));
    }

    #[test]
    fn overlapping_pending_writes_read_back_in_issue_order() {
        let mut disk = SimDisk::new(4096, 4);
        disk.write_at(0, b"aaaa").unwrap();
        disk.write_at(2, b"bb").unwrap();
        let mut buf = [0u8; 4];
        disk.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"aabb");
    }
}
