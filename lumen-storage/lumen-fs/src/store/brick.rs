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
use crate::hash::{hash_block, BlockHash};
use crate::store::format::{
    record_span, Anchor, RecordHeader, RosterEntry, SegmentHeader, Superblock, ANCHOR_AREA_START,
    ANCHOR_SLOTS, RECORD_HEADER_LEN, SECTOR_SIZE, WAL_AREA_START,
};

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
    /// and guards the space. Zero — and only zero — when this brick is not
    /// the WAL holder.
    pub wal_size: u64,
    /// The device class assigned by the drive wizard: 0 is the fastest
    /// class present, downward from there. Written into the superblock so
    /// the platter is the record.
    pub tier: u8,
    /// Whether this brick hosts its node's WAL ring and anchor slots.
    /// Exactly one brick per node does, always at tier 0; the others carry
    /// only segments, with `wal_size` 0 and their anchor slots zeroed.
    pub wal_holder: bool,
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
    pub segments_total: u64,
    pub segments_live: u64,
    pub segments_free: u64,
    /// Payload bytes the index points at (not the padded on-disk spans).
    pub payload_bytes: u64,
}

/// A brick's space said in bytes — see [`Brick::byte_space`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ByteSpace {
    /// What this brick can hold: every segment outside the collection
    /// reserve, at full-block payload density.
    pub usable_bytes: u64,
    /// What it can still take, on the same accounting.
    pub free_bytes: u64,
    /// What it holds now: the payload bytes the index points at.
    pub payload_bytes: u64,
}

impl ByteSpace {
    /// Fold another brick's figures into these — how a set and a tier sum.
    pub fn add(&mut self, other: &ByteSpace) {
        self.usable_bytes += other.usable_bytes;
        self.free_bytes += other.free_bytes;
        self.payload_bytes += other.payload_bytes;
    }
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

/// Segments held back from ordinary writes so a collection can always run.
///
/// A collection must write before it can free — the checkpoint it starts
/// with folds dirty maps into new tree nodes, and compaction rewrites live
/// records ahead of releasing their old segments. A brick with nothing free
/// could therefore never be collected: the state where reclaiming is most
/// needed would be the one state where it is impossible. Roughly a
/// sixteenth of the brick, never less than one segment, is kept for that —
/// and a brick too small to spare one keeps none, because refusing every
/// write would be worse than the hazard.
fn reserve_segments(segment_count: u64) -> u64 {
    (segment_count / 16)
        .max(1)
        .min(segment_count.saturating_sub(1))
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
    /// How many free segments ordinary writes must leave alone.
    reserve: u64,
    /// Whether the reserve is currently available — true only inside a
    /// collection.
    reserve_open: bool,
}

impl<D: Disk> Brick<D> {
    /// Lay the format down and return the opened, empty brick. Everything is
    /// written before the single flush, so a crash mid-format leaves either
    /// a formatted brick or no brick — never half of one. A holder's initial
    /// anchor rosters this brick alone — the single-brick pool's shape; a
    /// set formatted together goes through [`Brick::format_rostered`].
    pub fn format(disk: D, params: BrickParams) -> Result<Brick<D>> {
        let roster = vec![RosterEntry {
            brick_uuid: params.brick_uuid,
            tier: params.tier,
        }];
        Self::format_inner(disk, params, roster)
    }

    /// Format the WAL holder of a multi-brick set: its initial anchor
    /// rosters every brick of the set, itself included. The holder is
    /// formatted *last*, so a crash anywhere in a set's format leaves no
    /// anchor naming bricks that do not exist — just orphan non-holders a
    /// retry reformats.
    pub fn format_rostered(
        disk: D,
        params: BrickParams,
        roster: Vec<RosterEntry>,
    ) -> Result<Brick<D>> {
        if !params.wal_holder {
            return Err(FsError::BrickSetMismatch(
                "only the WAL holder writes the anchor its roster lives in",
            ));
        }
        if !roster
            .iter()
            .any(|entry| entry.brick_uuid == params.brick_uuid && entry.tier == params.tier)
        {
            return Err(FsError::BrickSetMismatch(
                "a holder's roster must include the holder itself",
            ));
        }
        if roster.len() > crate::store::format::ROSTER_CAP {
            return Err(FsError::BrickSetMismatch(
                "more bricks than one node's anchor can roster",
            ));
        }
        Self::format_inner(disk, params, roster)
    }

    fn format_inner(
        mut disk: D,
        params: BrickParams,
        roster: Vec<RosterEntry>,
    ) -> Result<Brick<D>> {
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
        if params.wal_holder {
            if !params.wal_size.is_multiple_of(SECTOR_SIZE) || params.wal_size < 2 * SECTOR_SIZE {
                return Err(FsError::BadGeometry(
                    "the WAL area must be a sector multiple of at least two sectors",
                ));
            }
        } else if params.wal_size != 0 {
            // A ring nobody anchors is a ring nobody replays: space that
            // could only ever hold debris.
            return Err(FsError::BadGeometry(
                "only the WAL holder carries a WAL area",
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
            tier: params.tier,
            wal_holder: params.wal_holder,
        };
        let encoded = sb.encode();
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        sector[..encoded.len()].copy_from_slice(&encoded);
        disk.write_at(0, &sector)?;
        disk.write_at(SECTOR_SIZE, &sector)?;
        // Retire any anchors a previous format left behind (anchors carry
        // no brick uuid — zeroing the slots is what unlinks history), then
        // lay down the initial one: an empty pool, replay from the top of
        // the WAL, rostering this brick alone — a brick set grows by being
        // formatted together, and every member of one appears in the anchor
        // its holder writes. A non-holder gets only the zeroing: its anchor
        // slots stay empty for good, because the holder's anchor speaks for
        // the whole set. The same single flush covers superblocks and
        // anchor, so a crash mid-format still leaves a brick or no brick.
        let zero = vec![0u8; SECTOR_SIZE as usize];
        for slot in 0..ANCHOR_SLOTS {
            disk.write_at(ANCHOR_AREA_START + slot * SECTOR_SIZE, &zero)?;
        }
        if params.wal_holder {
            let anchor = Anchor {
                generation: 1,
                wal_replay_offset: WAL_AREA_START,
                wal_replay_seq: 1,
                wal_epoch: 1,
                era: 1,
                manifest_hash: [0; 32],
                roster,
            };
            let mut anchor_sector = vec![0u8; SECTOR_SIZE as usize];
            anchor_sector[..anchor.encode().len()].copy_from_slice(&anchor.encode());
            disk.write_at(Anchor::slot_offset(anchor.generation), &anchor_sector)?;
        }
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
            reserve: reserve_segments(segment_count),
            reserve_open: false,
        })
    }

    /// Open a formatted brick: pick the best superblock, scan every segment,
    /// rebuild the index. The scan is the recovery — there is no journal to
    /// replay and no repair step; invalid tails are simply never entered
    /// into the index.
    pub fn open(disk: D) -> Result<Brick<D>> {
        let mut best: Option<Superblock> = None;
        // A slot refusing by version does not end the open — its twin may
        // be a valid superblock of this version, and the twin wins. Only
        // when *no* slot is usable does the version refusal surface, which
        // is exactly the v1 brick: both slots agree it is another format,
        // and the answer is its name, not NotFormatted.
        let mut refused: Option<FsError> = None;
        for slot in 0..2u64 {
            let mut buf = vec![0u8; SECTOR_SIZE as usize];
            disk.read_at(slot * SECTOR_SIZE, &mut buf)?;
            match Superblock::decode(&buf) {
                Ok(Some(sb)) => {
                    let newer = best
                        .as_ref()
                        .map(|b| sb.generation > b.generation)
                        .unwrap_or(true);
                    if newer {
                        best = Some(sb);
                    }
                }
                Ok(None) => {}
                Err(err) => refused = Some(err),
            }
        }
        let sb = match (best, refused) {
            (Some(sb), _) => sb,
            (None, Some(err)) => return Err(err),
            (None, None) => return Err(FsError::NotFormatted),
        };
        let area_end = sb
            .segment_area_start
            .checked_add(sb.segment_count.saturating_mul(sb.segment_size));
        match area_end {
            Some(end) if end <= disk.size() => {}
            _ => return Err(FsError::BadGeometry("superblock geometry exceeds the disk")),
        }

        let reserve = reserve_segments(sb.segment_count);
        let mut brick = Brick {
            disk,
            sb,
            index: HashMap::new(),
            free: Vec::new(),
            max_seq: 0,
            open: None,
            reserve,
            reserve_open: false,
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

    /// A put whose hash the caller already computed — the brick-set path,
    /// where the set hashes once to consult its route and must not pay for
    /// the same bytes twice. The within-brick dedupe check stays: a crash
    /// can resurrect a copy here that the set routed elsewhere, and
    /// appending a third would compound the leak GC exists to reclaim.
    pub(crate) fn put_hashed(&mut self, hash: BlockHash, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Err(FsError::EmptyPayload);
        }
        if payload.len() > self.sb.block_size as usize {
            return Err(FsError::PayloadTooLarge {
                len: payload.len(),
                block_size: self.sb.block_size,
            });
        }
        if self.index.contains_key(&hash) {
            return Ok(());
        }
        let location = self.append_record(hash, payload)?;
        self.index.insert(hash, location);
        Ok(())
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
        // Ordinary writes stop at the reserve; a collection may spend it.
        let floor = if self.reserve_open { 0 } else { self.reserve };
        let index = if self.free.len() as u64 <= floor {
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

    /// Make the reserve available. Only a collection may call this, and
    /// only for as long as it runs — see [`reserve_segments`].
    pub fn open_reserve(&mut self) {
        self.reserve_open = true;
    }

    /// Put the reserve back out of reach.
    pub fn close_reserve(&mut self) {
        self.reserve_open = false;
    }

    /// Which segment an absolute record offset belongs to.
    fn segment_of(&self, offset: u64) -> u64 {
        (offset - self.sb.segment_area_start) / self.sb.segment_size
    }

    /// Retire a segment: zero its header and hand it back to the free list.
    /// The zeroing is pending, and either fate in a crash is safe — a
    /// surviving header describes records nothing references, which the
    /// next collection drops again.
    fn release_segment(&mut self, segment: u64) -> Result<()> {
        let zero = vec![0u8; SECTOR_SIZE as usize];
        self.disk.write_at(self.sb.segment_offset(segment), &zero)?;
        self.free.push(segment);
        Ok(())
    }

    /// The sweep half of garbage collection: keep exactly the blocks in
    /// `live`, reclaim what that empties out.
    ///
    /// The caller (the pool) must have made `live` computable from durable
    /// state alone — checkpointed, WAL retired — before calling; that
    /// contract is what lets this run with no persistent bookkeeping at
    /// all. Unreachable index entries are dropped first. Their records stay
    /// on disk until their segment is reused; if a crash resurrects them
    /// into a future index, they are unreferenced blocks the next
    /// collection drops again — noise, not loss.
    ///
    /// Then segments are reclaimed, **emptiest first, releasing each one as
    /// it is finished**. Both halves of that are load-bearing, and a real
    /// workload taught them:
    ///
    /// - *Emptiest first, with no fixed threshold.* Refusing to touch
    ///   anything above some fraction of live data sounds like thrift, but
    ///   it means a pool sitting near that fraction has no eligible segment
    ///   anywhere and collection quietly becomes a no-op — busy, and
    ///   freeing nothing. Cheapest-first always finds the best available
    ///   work, and only a segment that is almost entirely live is skipped,
    ///   because moving it would cost about what it returns.
    /// - *Release as you go.* Reclaiming everything at the end means every
    ///   evacuation must fit in the space that existed **before** any of it
    ///   was freed — on a full brick, only the reserve. It runs out, no
    ///   source is ever emptied, and the collection frees nothing precisely
    ///   when it is needed most. Freeing each segment as its evacuation
    ///   lands makes the space compound, so a collection can work its way
    ///   out of a brick with almost nothing free.
    ///
    /// The ordering that keeps a crash honest is unchanged and now applies
    /// per segment: a source's replacement is **flushed before** the source
    /// becomes reusable, because a reused segment overwrites history and
    /// history may not die before its replacement is durable.
    pub fn retain_and_reclaim(&mut self, live: &HashSet<BlockHash>) -> Result<GcStats> {
        let before = self.index.len() as u64;
        self.index.retain(|hash, _| live.contains(hash));
        let mut stats = GcStats {
            blocks_dropped: before - self.index.len() as u64,
            ..Default::default()
        };

        let mut by_segment: HashMap<u64, Vec<BlockHash>> = HashMap::new();
        for (hash, location) in &self.index {
            by_segment
                .entry(self.segment_of(location.offset))
                .or_default()
                .push(*hash);
        }

        // Cheapest work first.
        let usable = self.sb.segment_size - SECTOR_SIZE;
        let mut candidates: Vec<(u64, u64)> = (0..self.sb.segment_count)
            .map(|segment| {
                let span: u64 = by_segment
                    .get(&segment)
                    .map(|hashes| {
                        hashes
                            .iter()
                            .filter_map(|hash| self.index.get(hash))
                            .map(|location| record_span(location.length))
                            .sum()
                    })
                    .unwrap_or(0);
                (span, segment)
            })
            .collect();
        candidates.sort_unstable();

        // When to stop, expressed two ways — and it needs both.
        //
        // `target_free` is the comfortable level: far enough above
        // whatever makes a caller ask for a collection that writing can go
        // a long way before asking again. A collection that stopped at
        // exactly the level that triggers it would leave the caller still
        // asking, and every write from then on would pay for a collection
        // with nothing left to do.
        //
        // But a level alone is a goal that can be impossible. Live data
        // approaching half the brick puts "half of it free" permanently
        // out of reach, and a collector chasing an unreachable target
        // evacuates everything it is allowed to, every single time. So the
        // *work* is bounded too, and that is the bound that actually
        // holds: each collection moves at most a few segments' worth, does
        // whatever good that buys, and returns. Progress at high
        // utilisation becomes gradual — which is honest, since a
        // copy-on-write store near full genuinely must move a byte to
        // place a byte — rather than collapsing.
        let target_free = (self.sb.segment_count / 2).max(self.reserve + 2);
        let move_budget = self.sb.segment_size.saturating_mul(4);
        let mut moved_bytes = 0u64;

        for (_, segment) in candidates {
            // Freshly, not from a snapshot: this loop frees segments as it
            // goes, and the write head moves into them.
            if self.open.map(|open| open.index) == Some(segment) || self.free.contains(&segment) {
                continue;
            }
            // Blocks may have moved out of this segment earlier in the
            // loop, so membership is taken from where they live now.
            let members: Vec<BlockHash> = by_segment
                .get(&segment)
                .map(|hashes| {
                    let mut here: Vec<BlockHash> = hashes
                        .iter()
                        .copied()
                        .filter(|hash| {
                            self.index
                                .get(hash)
                                .map(|location| self.segment_of(location.offset) == segment)
                                .unwrap_or(false)
                        })
                        .collect();
                    here.sort_unstable();
                    here
                })
                .unwrap_or_default();

            // A segment holding nothing live is always taken back: it is
            // free space already, and saying so costs one sector.
            if members.is_empty() {
                self.release_segment(segment)?;
                stats.segments_freed += 1;
                continue;
            }

            // Past here, reclaiming means *moving* data, which is paid for
            // in writes. Enough room recovered, or enough moving done,
            // and there is no reason to keep paying — the rest of the
            // garbage will still be garbage at the next collection.
            if self.free.len() as u64 >= target_free || moved_bytes >= move_budget {
                break;
            }

            let live_span: u64 = members
                .iter()
                .map(|hash| record_span(self.index[hash].length))
                .sum();
            // Almost entirely live: moving it would cost about what it
            // gives back, and the segments after it in this ordering are
            // no cheaper.
            if live_span * 10 >= usable * 9 {
                break;
            }

            let mut whole = true;
            for hash in &members {
                let location = self.index[hash];
                let payload = self.read_location(hash, &location)?;
                match self.append_record(*hash, &payload) {
                    Ok(new_location) => {
                        self.index.insert(*hash, new_location);
                        stats.blocks_moved += 1;
                    }
                    Err(FsError::Full) => {
                        whole = false;
                        break;
                    }
                    Err(other) => return Err(other),
                }
            }
            if whole {
                // The replacements must be durable before the original can
                // be handed out to be overwritten.
                self.disk.flush()?;
                self.release_segment(segment)?;
                stats.segments_compacted += 1;
                stats.segments_freed += 1;
                moved_bytes += live_span;
            }
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

    /// The device class this brick was assigned at format.
    pub fn tier(&self) -> u8 {
        self.sb.tier
    }

    /// Whether this brick hosts its node's WAL ring and anchor slots.
    pub fn wal_holder(&self) -> bool {
        self.sb.wal_holder
    }

    /// This brick as its own roster line — what a checkpoint writes into
    /// the anchor for it.
    pub fn roster_entry(&self) -> RosterEntry {
        RosterEntry {
            brick_uuid: self.sb.brick_uuid,
            tier: self.sb.tier,
        }
    }

    /// The pool this brick belongs to — what a replication handshake
    /// checks before a single frame crosses.
    pub fn pool_uuid(&self) -> [u8; 16] {
        self.sb.pool_uuid
    }

    pub fn stats(&self) -> BrickStats {
        BrickStats {
            blocks: self.index.len() as u64,
            segments_total: self.sb.segment_count,
            segments_free: self.free.len() as u64,
            segments_live: self.sb.segment_count
                - self.free.len() as u64
                - self.open.map(|_| 0).unwrap_or(0),
            payload_bytes: self.index.values().map(|l| l.length as u64).sum(),
        }
    }

    /// Hand the disk back — how a test crashes a brick: drop the handle,
    /// crash the disk, reopen.
    /// Every hash the index holds — what a brick set's route is rebuilt
    /// from at open, without re-reading a single payload.
    pub fn indexed_hashes(&self) -> impl Iterator<Item = BlockHash> + '_ {
        self.index.keys().copied()
    }

    /// Free segments right now — the allocation signal a brick set levels
    /// by. Cheaper than [`Brick::stats`] on the hot path.
    pub fn free_segments(&self) -> u64 {
        self.free.len() as u64
    }

    /// This brick's space in bytes, honestly: what segments flatter away —
    /// headers, record padding, the collection reserve — is charged here,
    /// so "usable" is the number an operator can actually store against.
    /// Dedupe is deliberately not modeled: it only makes these numbers
    /// conservative, which is the direction a capacity figure is allowed
    /// to be wrong in.
    pub fn byte_space(&self) -> ByteSpace {
        let block_size = self.sb.block_size;
        // A maximal record's on-disk span, and how many fit a segment
        // beside its header sector.
        let blocks_per_segment = (self.sb.segment_size - SECTOR_SIZE) / record_span(block_size);
        let segment_payload = blocks_per_segment * block_size as u64;
        let usable_segments = self.sb.segment_count.saturating_sub(self.reserve);
        let free_segments = (self.free.len() as u64).saturating_sub(self.reserve);
        let stats = self.stats();
        ByteSpace {
            usable_bytes: usable_segments * segment_payload,
            free_bytes: free_segments * segment_payload,
            payload_bytes: stats.payload_bytes,
        }
    }

    pub fn into_disk(self) -> D {
        self.disk
    }
}

// ---------------------------------------------------------------------------
// The store seams the map layer builds on.

/// Read-side content addressing: what a map lookup or walk needs. `Brick`
/// implements it directly; a brick set implements it per tier, which is how
/// the same fold code serves one disk or many without knowing which.
pub trait BlockRead {
    fn get(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>>;
    fn contains(&self, hash: &BlockHash) -> bool;
    fn block_size(&self) -> u32;
}

/// The write side a fold needs on top of reading.
pub trait BlockWrite: BlockRead {
    fn put(&mut self, payload: &[u8]) -> Result<BlockHash>;
}

impl<D: Disk> BlockRead for Brick<D> {
    fn get(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        Brick::get(self, hash)
    }
    fn contains(&self, hash: &BlockHash) -> bool {
        Brick::contains(self, hash)
    }
    fn block_size(&self) -> u32 {
        Brick::block_size(self)
    }
}

impl<D: Disk> BlockWrite for Brick<D> {
    fn put(&mut self, payload: &[u8]) -> Result<BlockHash> {
        Brick::put(self, payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::sim::SimDisk;

    const KIB: u64 = 1024;

    fn params() -> BrickParams {
        BrickParams {
            pool_uuid: [0xAA; 16],
            brick_uuid: [0xBB; 16],
            block_size: 16 * KIB as u32,
            segment_size: 256 * KIB,
            wal_size: 64 * KIB,
            tier: 0,
            wal_holder: true,
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

    /// Phase 4's compatibility decision at the layer that enforces it: a
    /// brick formatted under version 1 opens as its version's name, never
    /// as an unformatted disk the caller might feel free to reuse.
    #[test]
    fn a_version_one_brick_is_refused_by_name_not_reported_blank() {
        let mut buf = [0u8; 128];
        buf[0..8].copy_from_slice(b"LUMENFS\0");
        buf[8..12].copy_from_slice(&1u32.to_le_bytes());
        buf[12..16].copy_from_slice(&(16 * KIB as u32).to_le_bytes());
        buf[16..24].copy_from_slice(&(256 * KIB).to_le_bytes());
        buf[24..32].copy_from_slice(&(WAL_AREA_START + 64 * KIB).to_le_bytes());
        buf[32..40].copy_from_slice(&7u64.to_le_bytes());
        buf[40..48].copy_from_slice(&1u64.to_le_bytes());
        buf[48..56].copy_from_slice(&WAL_AREA_START.to_le_bytes());
        buf[56..64].copy_from_slice(&(64 * KIB).to_le_bytes());
        buf[64..80].copy_from_slice(&[1; 16]);
        buf[80..96].copy_from_slice(&[2; 16]);
        let check = crate::hash::full_check(&buf[0..96]);
        buf[96..128].copy_from_slice(&check);

        let mut disk = SimDisk::new(4 * KIB * KIB, 6);
        let mut sector = vec![0u8; SECTOR_SIZE as usize];
        sector[..buf.len()].copy_from_slice(&buf);
        disk.write_at(0, &sector).unwrap();
        disk.write_at(SECTOR_SIZE, &sector).unwrap();
        disk.flush().unwrap();
        assert_eq!(
            Brick::open(disk).unwrap_err(),
            FsError::UnsupportedVersion(1)
        );
    }

    #[test]
    fn a_non_holder_brick_carries_no_wal_and_no_anchor() {
        let quiet = BrickParams {
            wal_size: 0,
            tier: 1,
            wal_holder: false,
            ..params()
        };
        let mut brick = Brick::format(SimDisk::new(4 * KIB * KIB, 7), quiet).unwrap();
        assert_eq!(brick.wal_bounds(), (WAL_AREA_START, 0));
        assert_eq!(brick.read_best_anchor().unwrap(), None);
        assert_eq!(brick.tier(), 1);
        assert!(!brick.wal_holder());
        let hash = brick.put(b"payload").unwrap();
        brick.flush().unwrap();
        let brick = Brick::open(brick.into_disk()).unwrap();
        assert_eq!(brick.get(&hash).unwrap().unwrap(), b"payload");
        assert_eq!(brick.read_best_anchor().unwrap(), None);
        assert_eq!(brick.tier(), 1);
    }

    #[test]
    fn wal_geometry_must_match_the_holder_flag() {
        // A holder without a ring has nowhere to anchor replay.
        let holder_no_ring = BrickParams {
            wal_size: 0,
            ..params()
        };
        assert!(matches!(
            Brick::format(SimDisk::new(4 * KIB * KIB, 8), holder_no_ring).unwrap_err(),
            FsError::BadGeometry(_)
        ));
        // A ring on a non-holder is space nobody would ever replay.
        let ring_no_holder = BrickParams {
            wal_holder: false,
            ..params()
        };
        assert!(matches!(
            Brick::format(SimDisk::new(4 * KIB * KIB, 9), ring_no_holder).unwrap_err(),
            FsError::BadGeometry(_)
        ));
    }

    #[test]
    fn a_formatted_brick_states_its_tier_and_rosters_itself() {
        let tiered = BrickParams {
            tier: 2,
            ..params()
        };
        let brick = Brick::format(SimDisk::new(4 * KIB * KIB, 10), tiered).unwrap();
        assert_eq!(brick.tier(), 2);
        assert!(brick.wal_holder());
        let anchor = brick.read_best_anchor().unwrap().unwrap();
        assert_eq!(anchor.roster, vec![brick.roster_entry()]);
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
