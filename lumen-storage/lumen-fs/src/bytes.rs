//! The byte-granular view of a vdisk — the shape a block device speaks.
//!
//! Everything above the pool thinks in bytes at arbitrary offsets; the pool
//! thinks in content-addressed blocks. This module is the translation, kept
//! in the library rather than the NBD tool because translation is exactly
//! where a block-device bridge corrupts data — read-modify-write edges,
//! zero-fill of unmapped and short blocks, tail blocks shorter than the
//! block size — and everything in the library runs under the simulation.
//!
//! Semantics:
//!
//! - Unmapped ranges read as zeros, as do the tails of short payloads —
//!   "never written" and "written short" both look like a disk that came
//!   zeroed from the factory.
//! - A write covering part of a block is a read-modify-write of that block;
//!   old-or-new atomicity holds per block, exactly as at the block layer.
//! - A trim unmaps only the blocks it covers entirely — the advisory
//!   semantics discard has everywhere. Partial edges are left alone.
//! - `WalFull` surfaces mid-write with the finished blocks already written.
//!   That is safe to leave: the caller checkpoints and retries the whole
//!   call, and rewriting the finished blocks is a dedupe hit, not a second
//!   copy.
//!
//! The view is a trait rather than pool methods because two things speak
//! blocks underneath it: a bare [`Pool`] (the single-node tool) and a
//! [`ReplNode`] (the daemon's guest path, where every write must also
//! cross the wire). One copy of the edge logic, tested once under the
//! simulation, serving both — a second hand-rolled RMW loop is exactly
//! where a quiet corruption would grow.

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::pool::Pool;
use crate::repl::ReplNode;

fn byte_bounds(size: u64, block_size: u64, offset: u64, len: u64) -> Result<()> {
    match offset.checked_add(len) {
        Some(end) if end <= size => Ok(()),
        _ => Err(FsError::OutOfRange {
            index: offset / block_size,
            capacity: size.div_ceil(block_size),
        }),
    }
}

/// Byte-granular I/O over anything that reads and writes vdisk blocks.
pub trait ByteView {
    fn block_size(&self) -> u32;
    fn vdisk_size(&self, vdisk: u64) -> Result<u64>;
    fn read_block(&self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>>;
    fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()>;
    fn trim_block(&mut self, vdisk: u64, index: u64) -> Result<()>;

    /// Read a byte range. Always returns exactly `len` bytes; what was
    /// never written is zeros.
    fn read_bytes(&self, vdisk: u64, offset: u64, len: u64) -> Result<Vec<u8>> {
        let size = self.vdisk_size(vdisk)?;
        let block_size = self.block_size() as u64;
        byte_bounds(size, block_size, offset, len)?;
        let mut out = vec![0u8; len as usize];
        let mut pos = offset;
        while pos < offset + len {
            let block = pos / block_size;
            let in_off = (pos - block * block_size) as usize;
            let take = ((block_size as usize) - in_off).min((offset + len - pos) as usize);
            if let Some(payload) = self.read_block(vdisk, block)? {
                let have = payload.len().saturating_sub(in_off).min(take);
                let at = (pos - offset) as usize;
                out[at..at + have].copy_from_slice(&payload[in_off..in_off + have]);
            }
            pos += take as u64;
        }
        Ok(out)
    }

    /// Write a byte range. Blocks covered only in part are read, overlaid,
    /// and rewritten whole.
    fn write_bytes(&mut self, vdisk: u64, offset: u64, data: &[u8]) -> Result<()> {
        let size = self.vdisk_size(vdisk)?;
        let block_size = self.block_size() as u64;
        byte_bounds(size, block_size, offset, data.len() as u64)?;
        let mut pos = offset;
        while pos < offset + data.len() as u64 {
            let block = pos / block_size;
            let block_start = block * block_size;
            // The final block of an unaligned vdisk is logically shorter;
            // covering it to the vdisk's edge is covering it whole.
            let logical = block_size.min(size - block_start) as usize;
            let in_off = (pos - block_start) as usize;
            let take = (logical - in_off).min((offset + data.len() as u64 - pos) as usize);
            let from = (pos - offset) as usize;
            if in_off == 0 && take == logical {
                self.write_block(vdisk, block, &data[from..from + take])?;
            } else {
                let mut whole = vec![0u8; logical];
                if let Some(existing) = self.read_block(vdisk, block)? {
                    let have = existing.len().min(logical);
                    whole[..have].copy_from_slice(&existing[..have]);
                }
                whole[in_off..in_off + take].copy_from_slice(&data[from..from + take]);
                self.write_block(vdisk, block, &whole)?;
            }
            pos += take as u64;
        }
        Ok(())
    }

    /// Discard a byte range, unmapping every block the range covers whole.
    /// Partial edges are untouched — a trim is advice, and this is the
    /// advice's honest granularity.
    fn trim_bytes(&mut self, vdisk: u64, offset: u64, len: u64) -> Result<()> {
        let size = self.vdisk_size(vdisk)?;
        let block_size = self.block_size() as u64;
        byte_bounds(size, block_size, offset, len)?;
        let first = offset.div_ceil(block_size);
        let end = offset + len;
        // Reaching the vdisk's edge covers its (possibly short) final
        // block whole.
        let bound = if end == size {
            size.div_ceil(block_size)
        } else {
            end / block_size
        };
        for block in first..bound {
            self.trim_block(vdisk, block)?;
        }
        Ok(())
    }
}

impl<D: Disk> ByteView for Pool<D> {
    fn block_size(&self) -> u32 {
        Pool::block_size(self)
    }
    fn vdisk_size(&self, vdisk: u64) -> Result<u64> {
        Pool::vdisk_size(self, vdisk)
    }
    fn read_block(&self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>> {
        Pool::read_block(self, vdisk, index)
    }
    fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()> {
        Pool::write_block(self, vdisk, index, payload)
    }
    fn trim_block(&mut self, vdisk: u64, index: u64) -> Result<()> {
        Pool::trim_block(self, vdisk, index)
    }
}

/// The daemon's guest path: reads stay local, mutations replicate. Same
/// byte semantics, because they are the same code.
impl<D: Disk> ByteView for ReplNode<D> {
    fn block_size(&self) -> u32 {
        self.pool().block_size()
    }
    fn vdisk_size(&self, vdisk: u64) -> Result<u64> {
        self.pool().vdisk_size(vdisk)
    }
    fn read_block(&self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>> {
        ReplNode::read_block(self, vdisk, index)
    }
    fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()> {
        ReplNode::write_block(self, vdisk, index, payload)
    }
    fn trim_block(&mut self, vdisk: u64, index: u64) -> Result<()> {
        ReplNode::trim_block(self, vdisk, index)
    }
}

#[cfg(test)]
mod tests {
    use super::ByteView;
    use crate::brick::{Brick, BrickParams};
    use crate::error::FsError;
    use crate::pool::Pool;
    use crate::sim::{SimDisk, SplitMix64};

    const KIB: u64 = 1024;
    const BLOCK: u64 = 4 * KIB;
    /// Deliberately not block-aligned: the tail block is short.
    const VDISK_SIZE: u64 = 50 * BLOCK + 1000;

    fn pool(seed: u64) -> Pool<SimDisk> {
        let brick = Brick::format(
            SimDisk::new(8 * KIB * KIB, seed),
            BrickParams {
                pool_uuid: [0xAA; 16],
                brick_uuid: [0xBB; 16],
                block_size: BLOCK as u32,
                segment_size: 128 * KIB,
                wal_size: 64 * KIB,
            },
        )
        .unwrap();
        let mut pool = Pool::create(brick).unwrap();
        pool.create_vdisk(1, VDISK_SIZE).unwrap();
        pool
    }

    #[test]
    fn a_fresh_vdisk_reads_as_zeros_everywhere() {
        let pool = pool(1);
        assert_eq!(pool.read_bytes(1, 0, 100).unwrap(), vec![0u8; 100]);
        let tail = pool.read_bytes(1, VDISK_SIZE - 10, 10).unwrap();
        assert_eq!(tail, vec![0u8; 10]);
    }

    #[test]
    fn reads_and_writes_beyond_the_edge_are_refused() {
        let mut pool = pool(2);
        assert!(matches!(
            pool.read_bytes(1, VDISK_SIZE - 5, 6).unwrap_err(),
            FsError::OutOfRange { .. }
        ));
        assert!(matches!(
            pool.write_bytes(1, VDISK_SIZE, b"x").unwrap_err(),
            FsError::OutOfRange { .. }
        ));
    }

    #[test]
    fn an_unaligned_write_straddling_blocks_reads_back_exactly() {
        let mut pool = pool(3);
        let data: Vec<u8> = (0..(BLOCK as usize + 700))
            .map(|i| (i % 251) as u8)
            .collect();
        pool.write_bytes(1, BLOCK - 300, &data).unwrap();
        assert_eq!(
            pool.read_bytes(1, BLOCK - 300, data.len() as u64).unwrap(),
            data
        );
        // The bytes just outside the write are still zeros.
        assert_eq!(pool.read_bytes(1, BLOCK - 301, 1).unwrap(), vec![0]);
        let after = BLOCK - 300 + data.len() as u64;
        assert_eq!(pool.read_bytes(1, after, 1).unwrap(), vec![0]);
    }

    #[test]
    fn the_short_tail_block_round_trips_to_the_last_byte() {
        let mut pool = pool(4);
        let data = vec![0xEE; 1000];
        pool.write_bytes(1, VDISK_SIZE - 1000, &data).unwrap();
        assert_eq!(pool.read_bytes(1, VDISK_SIZE - 1000, 1000).unwrap(), data);
    }

    #[test]
    fn a_trim_unmaps_whole_blocks_and_spares_the_edges() {
        let mut pool = pool(5);
        let stripe = vec![0x77u8; (4 * BLOCK) as usize];
        pool.write_bytes(1, 0, &stripe).unwrap();
        // Trim from mid-block-0 to mid-block-3: blocks 1 and 2 go, the
        // partially-covered edges stay.
        pool.trim_bytes(1, 100, 3 * BLOCK).unwrap();
        assert_eq!(pool.read_bytes(1, 0, 100).unwrap(), vec![0x77; 100]);
        assert_eq!(
            pool.read_bytes(1, BLOCK, 2 * BLOCK).unwrap(),
            vec![0u8; (2 * BLOCK) as usize]
        );
        assert_eq!(
            pool.read_bytes(1, 3 * BLOCK + 100, 100).unwrap(),
            vec![0x77; 100]
        );
    }

    /// The suite's centerpiece: seeded byte-level workloads checked
    /// against a shadow buffer, across flushes, checkpoints, and reopens.
    #[test]
    fn random_byte_workloads_agree_with_a_shadow_buffer() {
        for seed in 0..12u64 {
            let mut rng = SplitMix64::new(seed.wrapping_mul(0x0B17E5));
            let mut pool = pool(100 + seed);
            let mut shadow = vec![0u8; VDISK_SIZE as usize];

            for round in 0..60 {
                let roll = rng.next_below(100);
                if roll < 55 {
                    let offset = rng.next_below(VDISK_SIZE);
                    let len = 1 + rng.next_below((3 * BLOCK).min(VDISK_SIZE - offset));
                    let mut data = vec![0u8; len as usize];
                    rng.fill(&mut data);
                    pool.write_bytes(1, offset, &data).unwrap();
                    shadow[offset as usize..(offset + len) as usize].copy_from_slice(&data);
                } else if roll < 70 {
                    let offset = rng.next_below(VDISK_SIZE);
                    let len = 1 + rng.next_below((4 * BLOCK).min(VDISK_SIZE - offset));
                    pool.trim_bytes(1, offset, len).unwrap();
                    // Mirror the advisory semantics exactly: only blocks
                    // covered whole go to zeros.
                    let first = offset.div_ceil(BLOCK);
                    let end = offset + len;
                    let bound = if end == VDISK_SIZE {
                        VDISK_SIZE.div_ceil(BLOCK)
                    } else {
                        end / BLOCK
                    };
                    for block in first..bound {
                        let from = (block * BLOCK) as usize;
                        let to = ((block + 1) * BLOCK).min(VDISK_SIZE) as usize;
                        shadow[from..to].fill(0);
                    }
                } else if roll < 85 {
                    let offset = rng.next_below(VDISK_SIZE);
                    let len = 1 + rng.next_below((3 * BLOCK).min(VDISK_SIZE - offset));
                    assert_eq!(
                        pool.read_bytes(1, offset, len).unwrap(),
                        shadow[offset as usize..(offset + len) as usize].to_vec(),
                        "seed {seed} round {round}"
                    );
                } else if roll < 95 {
                    pool.flush().unwrap();
                } else {
                    pool.checkpoint().unwrap();
                }
            }

            pool.checkpoint().unwrap();
            let pool = Pool::open(Brick::open(pool.into_brick().into_disk()).unwrap()).unwrap();
            assert_eq!(
                pool.read_bytes(1, 0, VDISK_SIZE).unwrap(),
                shadow,
                "seed {seed}: the reopened vdisk disagrees with the shadow"
            );
        }
    }
}
