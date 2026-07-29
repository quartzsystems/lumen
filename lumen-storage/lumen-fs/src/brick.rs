//! One disk's extent store: format it, recover it, put blocks, get blocks.
//!
//! A brick is the bottom layer of the pool — content-addressed storage on a
//! single device. Everything above it (replication, vdisk maps, dedupe
//! refcounts) arrives in later stages; what this layer promises is exactly
//! the durability contract from lib.rs: after [`Brick::flush`] returns,
//! every block put before it survives any crash, intact, under its content
//! address — and a [`Brick::get`] never returns bytes that fail that
//! address.
//!
//! ## Decisions
//!
//! **Recovery never appends into an existing segment.** After open, the
//! write head is always a fresh segment incarnation (a higher seq). Writing
//! at the tail of a recovered segment would mean new records physically
//! interleaved with the debris of unacknowledged ones from before the crash
//! — parseable in ways that depend on exactly how the debris tore. A fresh
//! incarnation makes the recovered state append-only history, at the cost of
//! some fragmentation that segment GC (later in phase 1) reclaims. A
//! recovered segment holding zero valid records is reclaimed immediately, so
//! an incarnation that landed nothing costs nothing; one whose unflushed
//! records happened to survive the crash is live — those blocks are real,
//! merely never promised — and the space they strand is GC's business, not
//! recovery's.
//!
//! **Dedupe is the index.** A put whose hash the index already holds is a
//! no-op returning the existing address. Compare-by-hash, no byte-compare
//! fallback — the position taken in hash.rs.
//!
//! **The index is rebuilt by scan, not persisted.** Phase 1 trades open
//! time for having no index durability story to get wrong yet; the
//! persisted LSM index arrives with the dedupe-refcount work. Nothing on
//! disk assumes the index exists.

use std::collections::{HashMap, HashSet};

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::format::{
    record_span, Anchor, RecordHeader, SegmentHeader, Superblock, ANCHOR_AREA_START, ANCHOR_SLOTS,
    RECORD_HEADER_LEN, SECTOR_SIZE, WAL_AREA_START,
};
use crate::hash::{hash_block, BlockHash};

/// Everything format-time about a brick. The caller supplies the UUIDs —
/// this crate has no randomness of its own, by design.
#[derive(Debug, Clone)]
pub struct BrickParams {
    pub pool_uuid: [u8; 16],
    pub brick_uuid: [u8; 16],
    /// Max payload bytes per block. The pool-wide dedupe unit.
    pub block_size: u32,
    /// Bytes per segment; a sector multiple with room for the header and at
    /// least one maximal record.
    pub segment_size: u64,
    /// Bytes reserved for the write-ahead ring; a sector multiple. The WAL
    /// itself is the pool's business (wal.rs) — the brick only carves out
    /// and guards the space.
    pub wal_size: u64,
}

/// Where one block lives.
#[derive(Debug, Clone, Copy)]
struct Location {
    /// Absolute disk offset of the record header.
    offset: u64,
    length: u32,
    /// The segment incarnation the record was written under; a read
    /// re-validates against it.
    seq: u64,
}

/// The write head: one segment incarnation accepting appends.
#[derive(Debug, Clone, Copy)]
struct OpenSegment {
    index: u64,
    seq: u64,
    /// Absolute offset of the next record.
    cursor: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrickStats {
    pub blocks: u64,
    pub segments_live: u64,
    pub segments_free: u64,
    /// Payload bytes the index points at (not the padded on-disk spans).
    pub payload_bytes: u64,
}

/// What one garbage collection accomplished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcStats {
    /// Index entries dropped as unreachable.
    pub blocks_dropped: u64,
    /// Live blocks rewritten out of sparse segments.
    pub blocks_moved: u64,
    /// Segments returned to the free list.
    pub segments_freed: u64,
    /// Of those, segments that needed evacuating first.
    pub segments_compacted: u64,
}

#[derive(Debug)]
pub struct Brick<D: Disk> {
    disk: D,
    sb: Superblock,
    index: HashMap<BlockHash, Location>,
    /// Segment indices with no live data, lowest first.
    free: Vec<u64>,
    /// Highest incarnation ever observed on this brick.
    max_seq: u64,
    open: Option<OpenSegment>,
}

impl<D: Disk> Brick<D> {
    /// Lay the format down and return the opened, empty brick. Everything is
    /// written before the single flush, so a crash mid-format leaves either
    /// a formatted brick or no brick — never half of one.
    pub fn format(mut disk: D, params: BrickParams) -> Result<Brick<D>> {
        if params.block_size == 0 {
            return Err(FsError::BadGeometry("block size must be non-zero"));
        }
        if !params.segment_size.is_multiple_of(SECTOR_SIZE) {
            return Err(FsError::BadGeometry(
                "segment size must be a sector multiple",
            ));
        }
        if SECTOR_SIZE + record_span(params.block_size) > params.segment_size {
            return Err(FsError::BadGeometry(
                "a segment must hold its header and one maximal block",
            ));
        }
        if !params.wal_size.is_multiple_of(SECTOR_SIZE) || params.wal_size < 2 * SECTOR_SIZE {
            return Err(FsError::BadGeometry(
                "the WAL area must be a sector multiple of at least two sectors",
            ));
        }
        let segment_area_start = WAL_AREA_START + params.wal_size;
        let area = disk.size().saturating_sub(segment_area_start);
        let segment_count = area / params.segment_size;
        if segment_count == 0 {
            return Err(FsError::BadGeometry("disk too small for one segment"));
        }

        let sb = Superblock {
            block_size: params.block_size,
            segment_size: params.segment_size,
            segment_area_start,
            segment_count,
            generation: 1,
            wal_start: WAL_AREA_START,
            wal_size: params.wal_size,
            pool_uuid: params.pool_uuid,
            brick_uuid: params.brick_uuid,
        };
        let encoded = sb.encode();
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        sector[..encoded.len()].copy_from_slice(&encoded);
        disk.write_at(0, &sector)?;
        disk.write_at(SECTOR_SIZE, &sector)?;
        // Retire any anchors a previous format left behind (anchors carry
        // no brick uuid — zeroing the slots is what unlinks history), then
        // lay down the initial one: an empty pool, replay from the top of
        // the WAL. The same single flush covers superblocks and anchor, so
        // a crash mid-format still leaves a brick or no brick.
        let zero = vec![0u8; SECTOR_SIZE as usize];
        for slot in 0..ANCHOR_SLOTS {
            disk.write_at(ANCHOR_AREA_START + slot * SECTOR_SIZE, &zero)?;
        }
        let anchor = Anchor {
            generation: 1,
            wal_replay_offset: WAL_AREA_START,
            wal_replay_seq: 1,
            wal_epoch: 1,
            era: 1,
            manifest_hash: [0; 32],
        };
        let mut anchor_sector = vec![0u8; SECTOR_SIZE as usize];
        anchor_sector[..anchor.encode().len()].copy_from_slice(&anchor.encode());
        disk.write_at(Anchor::slot_offset(anchor.generation), &anchor_sector)?;
        disk.flush()?;

        // A fresh brick_uuid is what retires any segment headers left by a
        // previous format: they fail the uuid comparison and read as free.
        Ok(Brick {
            disk,
            free: (0..segment_count).collect(),
            sb,
            index: HashMap::new(),
            max_seq: 0,
            open: None,
        })
    }

    /// Open a formatted brick: pick the best superblock, scan every segment,
    /// rebuild the index. The scan is the recovery — there is no journal to
    /// replay and no repair step; invalid tails are simply never entered
    /// into the index.
    pub fn open(disk: D) -> Result<Brick<D>> {
        let mut best: Option<Superblock> = None;
        for slot in 0..2u64 {
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            disk.read_at(slot * SECTOR_SIZE, &mut buf)?;
            if let Some(sb) = Superblock::decode(&buf)? {
                let newer = best
                    .as_ref()
                    .map(|b| sb.generation > b.generation)
                    .unwrap_or(true);
                if newer {
                    best = Some(sb);
                }
            }
        }
        let sb = best.ok_or(FsError::NotFormatted)?;
        let area_end = sb
            .segment_area_start
            .checked_add(sb.segment_count.saturating_mul(sb.segment_size));
        match area_end {
            Some(end) if end <= disk.size() => {}
            _ => return Err(FsError::BadGeometry("superblock geometry exceeds the disk")),
        }

        let mut brick = Brick {
            disk,
            sb,
            index: HashMap::new(),
            free: Vec::new(),
            max_seq: 0,
            open: None,
        };
        for segment in 0..brick.sb.segment_count {
            brick.recover_segment(segment)?;
        }
        Ok(brick)
    }

    fn recover_segment(&mut self, segment: u64) -> Result<()> {
        let base = self.sb.segment_offset(segment);
        let mut header_buf = vec![0u8; SECTOR_SIZE as usize];
        self.disk.read_at(base, &mut header_buf)?;
        let header = match SegmentHeader::decode(&header_buf, segment, &self.sb.brick_uuid) {
            Some(header) => header,
            None => {
                // Torn, stale, foreign, or blank: unused either way.
                self.free.push(segment);
                return Ok(());
            }
        };
        self.max_seq = self.max_seq.max(header.seq);

        let end = base + self.sb.segment_size;
        let mut cursor = base + SECTOR_SIZE;
        let mut records = 0u64;
        while cursor + RECORD_HEADER_LEN as u64 <= end {
            let mut head = [0u8; RECORD_HEADER_LEN];
            self.disk.read_at(cursor, &mut head)?;
            let record = match RecordHeader::decode(&head, header.seq, self.sb.block_size) {
                Some(record) if cursor + record_span(record.length) <= end => record,
                _ => {
                    // Not a record of this incarnation — a torn tail, rot,
                    // or debris. Resync at the next sector rather than
                    // giving up on the segment: a valid record beyond this
                    // point is either acknowledged data that must not be
                    // lost to a neighbor's failure, or an unacknowledged
                    // survivor the contract allows either way.
                    cursor += SECTOR_SIZE;
                    continue;
                }
            };
            let mut payload = vec![0u8; record.length as usize];
            self.disk
                .read_at(cursor + RECORD_HEADER_LEN as u64, &mut payload)?;
            if hash_block(&payload) != record.hash {
                // A record whose header survived but whose payload did not.
                cursor += SECTOR_SIZE;
                continue;
            }
            self.index.insert(
                record.hash,
                Location {
                    offset: cursor,
                    length: record.length,
                    seq: header.seq,
                },
            );
            records += 1;
            cursor += record_span(record.length);
        }

        if records == 0 {
            // Headed but empty — a crash between opening the segment and
            // landing a block. Reclaim it; the next incarnation's higher
            // seq retires this header's debris.
            self.free.push(segment);
        }
        Ok(())
    }

    /// Store a payload under its content address. Not durable until the
    /// next [`Brick::flush`]. A payload the brick already holds is a no-op
    /// returning the same address.
    pub fn put(&mut self, payload: &[u8]) -> Result<BlockHash> {
        if payload.is_empty() {
            return Err(FsError::EmptyPayload);
        }
        if payload.len() > self.sb.block_size as usize {
            return Err(FsError::PayloadTooLarge {
                len: payload.len(),
                block_size: self.sb.block_size,
            });
        }
        let hash = hash_block(payload);
        if self.index.contains_key(&hash) {
            return Ok(hash);
        }
        let location = self.append_record(hash, payload)?;
        self.index.insert(hash, location);
        Ok(hash)
    }

    /// Append one record at the write head, regardless of the index — the
    /// shared tail of a put and of a compaction move.
    fn append_record(&mut self, hash: BlockHash, payload: &[u8]) -> Result<Location> {
        let span = record_span(payload.len() as u32);
        let open = self.writable_segment(span)?;
        let header = RecordHeader {
            seq: open.seq,
            length: payload.len() as u32,
            hash,
        };
        let mut record = vec![0u8; span as usize];
        record[..RECORD_HEADER_LEN].copy_from_slice(&header.encode());
        record[RECORD_HEADER_LEN..RECORD_HEADER_LEN + payload.len()].copy_from_slice(payload);
        self.disk.write_at(open.cursor, &record)?;
        self.open = Some(OpenSegment {
            cursor: open.cursor + span,
            ..open
        });
        Ok(Location {
            offset: open.cursor,
            length: payload.len() as u32,
            seq: open.seq,
        })
    }

    /// A write head with room for `span` bytes, opening a fresh segment
    /// incarnation when the current one is full or absent.
    fn writable_segment(&mut self, span: u64) -> Result<OpenSegment> {
        if let Some(open) = self.open {
            let end = self.sb.segment_offset(open.index) + self.sb.segment_size;
            if open.cursor + span <= end {
                return Ok(open);
            }
            // Sealed by being full; it stays live through the index alone.
            self.open = None;
        }
        let index = if self.free.is_empty() {
            return Err(FsError::Full);
        } else {
            let lowest = *self.free.iter().min().unwrap();
            self.free.retain(|&s| s != lowest);
            lowest
        };
        self.max_seq += 1;
        let header = SegmentHeader {
            seq: self.max_seq,
            segment_index: index,
            brick_uuid: self.sb.brick_uuid,
        };
        let base = self.sb.segment_offset(index);
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        sector[..header.encode().len()].copy_from_slice(&header.encode());
        // Pending alongside the records that follow: the flush barrier
        // orders them, and until a flush the whole incarnation is
        // unacknowledged anyway.
        self.disk.write_at(base, &sector)?;
        let open = OpenSegment {
            index,
            seq: self.max_seq,
            cursor: base + SECTOR_SIZE,
        };
        self.open = Some(open);
        Ok(open)
    }

    /// Fetch a block by content address. `Ok(None)` is "this brick does not
    /// hold that block"; [`FsError::Corrupt`] is "it should, and the bytes
    /// are wrong" — the caller repairs, it never guesses.
    pub fn get(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        let location = match self.index.get(hash) {
            Some(location) => *location,
            None => return Ok(None),
        };
        Ok(Some(self.read_location(hash, &location)?))
    }

    /// Read and fully verify the record at a known location — the shared
    /// heart of a get, a compaction move, and the scrub.
    fn read_location(&self, hash: &BlockHash, location: &Location) -> Result<Vec<u8>> {
        let mut head = [0u8; RECORD_HEADER_LEN];
        self.disk.read_at(location.offset, &mut head)?;
        let record = RecordHeader::decode(&head, location.seq, self.sb.block_size)
            .ok_or(FsError::Corrupt("record header no longer parses"))?;
        if record.length != location.length || record.hash != *hash {
            return Err(FsError::Corrupt("record header contradicts the index"));
        }
        let mut payload = vec![0u8; record.length as usize];
        self.disk
            .read_at(location.offset + RECORD_HEADER_LEN as u64, &mut payload)?;
        if hash_block(&payload) != *hash {
            return Err(FsError::Corrupt("payload fails its content address"));
        }
        Ok(payload)
    }

    /// The durability barrier: every put before this call is on stable
    /// storage when it returns. This is the only acknowledgement point.
    pub fn flush(&mut self) -> Result<()> {
        self.disk.flush()
    }

    /// The WAL area's bounds: `(start, size)`.
    pub fn wal_bounds(&self) -> (u64, u64) {
        (self.sb.wal_start, self.sb.wal_size)
    }

    /// Read within the auxiliary area — anchors and WAL. The brick owns the
    /// device; the pool's machinery is a guest in this range and nowhere
    /// else, and the bounds check is what holds that.
    pub fn aux_read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check_aux(offset, buf.len())?;
        self.disk.read_at(offset, buf)
    }

    /// Write within the auxiliary area — same guest, same fence.
    pub fn aux_write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check_aux(offset, data.len())?;
        self.disk.write_at(offset, data)
    }

    fn check_aux(&self, offset: u64, len: usize) -> Result<()> {
        let end = offset.checked_add(len as u64);
        match end {
            Some(end) if offset >= ANCHOR_AREA_START && end <= self.sb.segment_area_start => Ok(()),
            _ => Err(FsError::OutOfBounds {
                offset,
                len: len as u64,
                disk_size: self.sb.segment_area_start,
            }),
        }
    }

    /// The valid anchor with the highest generation, or `None` for a brick
    /// that somehow lost both — which open() treats as corruption, since
    /// format always writes one.
    pub fn read_best_anchor(&self) -> Result<Option<Anchor>> {
        let mut best: Option<Anchor> = None;
        for slot in 0..ANCHOR_SLOTS {
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            self.aux_read(ANCHOR_AREA_START + slot * SECTOR_SIZE, &mut buf)?;
            if let Some(anchor) = Anchor::decode(&buf) {
                let newer = best
                    .as_ref()
                    .map(|b| anchor.generation > b.generation)
                    .unwrap_or(true);
                if newer {
                    best = Some(anchor);
                }
            }
        }
        Ok(best)
    }

    /// Write an anchor into its generation's slot. Not durable until the
    /// next flush — the checkpoint sequence owns the ordering.
    pub fn write_anchor(&mut self, anchor: &Anchor) -> Result<()> {
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        sector[..anchor.encode().len()].copy_from_slice(&anchor.encode());
        self.aux_write(Anchor::slot_offset(anchor.generation), &sector)
    }

    /// The sweep half of garbage collection: keep exactly the blocks in
    /// `live`, reclaim what that empties out.
    ///
    /// The caller (the pool) must have made `live` computable from durable
    /// state alone — checkpointed, WAL retired — before calling; that
    /// contract is what lets this run with no persistent bookkeeping at
    /// all. Three moves, in an order a crash cannot break:
    ///
    /// 1. Unreachable index entries are dropped. Their records stay on disk
    ///    until their segment is reused; if a crash resurrects them into a
    ///    future index, they are unreferenced blocks the next collection
    ///    drops again — noise, not loss.
    /// 2. Sparse segments (live spans at most half the usable space) are
    ///    evacuated: live records rewritten at the write head. Content
    ///    addressing makes the move invisible to every map that references
    ///    the block. The moves are **flushed before** any source becomes
    ///    reusable — a reused source overwrites history, and history may
    ///    not die before its replacement is durable.
    /// 3. Emptied segments get their headers zeroed (pending — either fate
    ///    in a crash is safe) and join the free list.
    ///
    /// An evacuation stopped by a full brick keeps its source segment live
    /// with whatever remains; the copies already made are merely dead
    /// weight for the next collection.
    pub fn retain_and_reclaim(&mut self, live: &HashSet<BlockHash>) -> Result<GcStats> {
        let before = self.index.len() as u64;
        self.index.retain(|hash, _| live.contains(hash));
        let mut stats = GcStats {
            blocks_dropped: before - self.index.len() as u64,
            ..Default::default()
        };

        let mut by_segment: HashMap<u64, Vec<BlockHash>> = HashMap::new();
        for (hash, location) in &self.index {
            let segment = (location.offset - self.sb.segment_area_start) / self.sb.segment_size;
            by_segment.entry(segment).or_default().push(*hash);
        }

        let open_segment = self.open.map(|open| open.index);
        let already_free: HashSet<u64> = self.free.iter().copied().collect();
        let mut reclaimed = Vec::new();
        let mut moved_any = false;
        for segment in 0..self.sb.segment_count {
            if Some(segment) == open_segment || already_free.contains(&segment) {
                continue;
            }
            let hashes = match by_segment.get(&segment) {
                None => {
                    // Nothing live at all — reclaim without touching data.
                    reclaimed.push(segment);
                    continue;
                }
                Some(hashes) => {
                    let mut hashes = hashes.clone();
                    hashes.sort_unstable();
                    hashes
                }
            };
            let live_span: u64 = hashes
                .iter()
                .map(|hash| record_span(self.index[hash].length))
                .sum();
            if live_span * 2 > self.sb.segment_size - SECTOR_SIZE {
                continue;
            }
            let mut whole = true;
            for hash in &hashes {
                let location = self.index[hash];
                let payload = self.read_location(hash, &location)?;
                match self.append_record(*hash, &payload) {
                    Ok(new_location) => {
                        self.index.insert(*hash, new_location);
                        stats.blocks_moved += 1;
                        moved_any = true;
                    }
                    Err(FsError::Full) => {
                        whole = false;
                        break;
                    }
                    Err(other) => return Err(other),
                }
            }
            if whole {
                stats.segments_compacted += 1;
                reclaimed.push(segment);
            }
        }

        if moved_any {
            self.disk.flush()?;
        }
        let zero = vec![0u8; SECTOR_SIZE as usize];
        for segment in reclaimed {
            self.disk.write_at(self.sb.segment_offset(segment), &zero)?;
            self.free.push(segment);
            stats.segments_freed += 1;
        }
        Ok(stats)
    }

    /// Verify every indexed record against its content address. Returns how
    /// many verified and the addresses that did not — rot found on a
    /// schedule instead of by an unlucky read. Repair belongs to phase 2
    /// and a peer's replica; this layer's whole duty is to never let wrong
    /// bytes pass as right ones.
    pub fn scrub(&self) -> Result<(u64, Vec<BlockHash>)> {
        let mut hashes: Vec<BlockHash> = self.index.keys().copied().collect();
        hashes.sort_unstable();
        let mut verified = 0u64;
        let mut corrupt = Vec::new();
        for hash in hashes {
            let location = self.index[&hash];
            match self.read_location(&hash, &location) {
                Ok(_) => verified += 1,
                Err(FsError::Corrupt(_)) => corrupt.push(hash),
                Err(other) => return Err(other),
            }
        }
        Ok((verified, corrupt))
    }

    pub fn contains(&self, hash: &BlockHash) -> bool {
        self.index.contains_key(hash)
    }

    pub fn block_size(&self) -> u32 {
        self.sb.block_size
    }

    pub fn stats(&self) -> BrickStats {
        BrickStats {
            blocks: self.index.len() as u64,
            segments_free: self.free.len() as u64,
            segments_live: self.sb.segment_count
                - self.free.len() as u64
                - self.open.map(|_| 0).unwrap_or(0),
            payload_bytes: self.index.values().map(|l| l.length as u64).sum(),
        }
    }

    /// Hand the disk back — how a test crashes a brick: drop the handle,
    /// crash the disk, reopen.
    pub fn into_disk(self) -> D {
        self.disk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimDisk;

    const KIB: u64 = 1024;

    fn params() -> BrickParams {
        BrickParams {
            pool_uuid: [0xAA; 16],
            brick_uuid: [0xBB; 16],
            block_size: 16 * KIB as u32,
            segment_size: 256 * KIB,
            wal_size: 64 * KIB,
        }
    }

    fn small_brick(seed: u64) -> Brick<SimDisk> {
        Brick::format(SimDisk::new(4 * KIB * KIB, seed), params()).unwrap()
    }

    #[test]
    fn a_put_block_reads_back_after_reopen() {
        let mut brick = small_brick(1);
        let hash = brick.put(b"the block").unwrap();
        brick.flush().unwrap();
        let brick = Brick::open(brick.into_disk()).unwrap();
        assert_eq!(brick.get(&hash).unwrap().unwrap(), b"the block");
    }

    #[test]
    fn an_unformatted_disk_refuses_to_open() {
        assert_eq!(
            Brick::open(SimDisk::new(4 * KIB * KIB, 2)).unwrap_err(),
            FsError::NotFormatted
        );
    }

    #[test]
    fn a_duplicate_put_is_one_block_not_two() {
        let mut brick = small_brick(3);
        let first = brick.put(&[7u8; 4096]).unwrap();
        let bytes_after_first = brick.stats().payload_bytes;
        let second = brick.put(&[7u8; 4096]).unwrap();
        assert_eq!(first, second);
        assert_eq!(brick.stats().blocks, 1);
        assert_eq!(brick.stats().payload_bytes, bytes_after_first);
    }

    #[test]
    fn blocks_roll_into_a_second_segment_and_all_survive_reopen() {
        let mut brick = small_brick(4);
        // Each 16 KiB block spans 5 sectors; a 256 KiB segment holds 12.
        let mut hashes = Vec::new();
        for i in 0..30u8 {
            hashes.push(brick.put(&[i; 16 * KIB as usize]).unwrap());
        }
        brick.flush().unwrap();
        let brick = Brick::open(brick.into_disk()).unwrap();
        for (i, hash) in hashes.iter().enumerate() {
            assert_eq!(
                brick.get(hash).unwrap().unwrap(),
                vec![i as u8; 16 * KIB as usize]
            );
        }
    }

    #[test]
    fn a_full_brick_refuses_the_next_block_and_stays_usable() {
        let mut brick = Brick::format(SimDisk::new(400 * KIB, 5), params()).unwrap();
        // One usable segment after the superblock sectors: 12 blocks fit.
        let mut last = None;
        for i in 0..12u8 {
            last = Some(brick.put(&[i; 16 * KIB as usize]).unwrap());
        }
        assert_eq!(
            brick.put(&[99u8; 16 * KIB as usize]).unwrap_err(),
            FsError::Full
        );
        assert_eq!(
            brick.get(&last.unwrap()).unwrap().unwrap(),
            vec![11u8; 16 * KIB as usize]
        );
    }

    #[test]
    fn an_oversized_and_an_empty_payload_are_refused_by_name() {
        let mut brick = small_brick(6);
        assert_eq!(brick.put(&[]).unwrap_err(), FsError::EmptyPayload);
        assert!(matches!(
            brick.put(&vec![0u8; 16 * KIB as usize + 1]).unwrap_err(),
            FsError::PayloadTooLarge { .. }
        ));
    }

    #[test]
    fn a_corrupt_superblock_slot_falls_back_to_its_twin() {
        let mut brick = small_brick(7);
        let hash = brick.put(b"resilient").unwrap();
        brick.flush().unwrap();
        let mut disk = brick.into_disk();
        disk.write_at(0, &[0xFF; 4096]).unwrap();
        disk.flush().unwrap();
        let brick = Brick::open(disk).unwrap();
        assert_eq!(brick.get(&hash).unwrap().unwrap(), b"resilient");
    }

    #[test]
    fn a_flipped_payload_bit_is_corruption_never_a_silent_answer() {
        let mut brick = small_brick(8);
        let hash = brick.put(&[3u8; 8192]).unwrap();
        brick.flush().unwrap();
        let mut disk = brick.into_disk();
        // Flip one payload byte of the first record: segment 0 starts after
        // the superblock and anchor sectors and the WAL area, its first
        // record after its header.
        let payload_at = 4 * 4096 + 64 * KIB + 4096 + RECORD_HEADER_LEN as u64 + 100;
        let mut byte = [0u8; 1];
        disk.read_at(payload_at, &mut byte).unwrap();
        disk.write_at(payload_at, &[byte[0] ^ 0x40]).unwrap();
        disk.flush().unwrap();
        let brick = Brick::open(disk).unwrap();
        // The scan already refuses the record; the block is simply absent
        // rather than wrong.
        assert_eq!(brick.get(&hash).unwrap(), None);
    }
}
