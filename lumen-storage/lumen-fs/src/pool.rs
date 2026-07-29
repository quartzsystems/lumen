//! The pool: vdisks over one brick, held together by the WAL, the map
//! trees, and the anchor.
//!
//! This is the engine's write path from docs/lumenfs.md, single-node form:
//!
//! ```text
//!   write_block:  payload → brick.put        (content-addressed data)
//!                 mutation → wal.append      (the entry that survives)
//!                 dirty map in memory        (cheap until checkpoint)
//!   flush:        one barrier — data and WAL entry become durable
//!                 together; this is the acknowledgement
//!   checkpoint:   dirty maps fold into COW trees (ordinary blocks),
//!                 manifest block written, flush; anchor written, flush —
//!                 the WAL's history is now redundant and retires
//!   open:         brick recovers, anchor names the manifest and the WAL
//!                 position, replay rebuilds the dirty maps
//! ```
//!
//! ## Decisions
//!
//! **Replay trusts nothing it can't verify.** A replayed entry is applied
//! only if everything it references holds: the vdisk exists, the index is
//! in range, the data block is present in the extent store. The first entry
//! that fails ends replay — it can only be an unacknowledged tail, because
//! an acknowledged entry's references were made durable by the same flush
//! that acknowledged it.
//!
//! **Checkpoints are two flushes, in an order that cannot lie.** Tree
//! nodes and the manifest go in and are flushed before the anchor that
//! names them is written and flushed. A crash between the two leaves the
//! old anchor pointing at the old state with the WAL still live — nothing
//! is lost, the checkpoint simply never happened. Orphaned nodes from the
//! failed attempt are GC's business, later in phase 1.
//!
//! **The pool does not decide when to checkpoint.** `WalFull` surfaces to
//! the caller, and `checkpoint()` is explicit. Policy — WAL pressure,
//! timers, snapshot requests — belongs to the daemon; the engine only
//! promises that a checkpoint always makes room.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::brick::{Brick, BrickStats, GcStats};
use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::format::Anchor;
use crate::hash::{hash_block, BlockHash};
use crate::map;
use crate::repl::{SnapshotOffer, VdiskOffer};
use crate::wal::Wal;

/// What a scrub found. Empty vectors are the healthy answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubReport {
    pub blocks_verified: u64,
    /// Stored blocks whose bytes fail their content address.
    pub corrupt: Vec<BlockHash>,
    /// `(vdisk, index)` pairs mapped to blocks the store cannot produce.
    pub missing: Vec<(u64, u64)>,
}

const MANIFEST_MAGIC: &[u8; 8] = b"LFSMAN\0\0";
const MANIFEST_HEADER_LEN: usize = 20; // magic 8 + version 4 + two counts
const MANIFEST_ENTRY_LEN: usize = 48; // id 8 + size 8 + root 32
const MANIFEST_SNAPSHOT_LEN: usize = 56; // vdisk 8 + snapshot 8 + size 8 + root 32
const MANIFEST_VERSION: u32 = 1;

/// One vdisk's durable identity in the manifest.
#[derive(Debug, Clone)]
struct VdiskState {
    size_bytes: u64,
    /// The checkpointed tree, if any writes have ever been checkpointed.
    root: Option<BlockHash>,
    /// Mutations since the last checkpoint — replayable from the WAL, so
    /// purely in-memory. `Some` maps an index, `None` is a trim tombstone.
    /// BTreeMap so a fold is deterministic.
    dirty: BTreeMap<u64, Option<BlockHash>>,
}

/// One pinned moment of one vdisk: a root the trees will never rewrite and
/// GC will never sweep. The size rides along so the snapshot keeps its own
/// shape whatever later happens to the vdisk.
#[derive(Debug, Clone, Copy)]
struct SnapshotState {
    size_bytes: u64,
    root: Option<BlockHash>,
}

pub struct Pool<D: Disk> {
    brick: Brick<D>,
    wal: Wal,
    vdisks: HashMap<u64, VdiskState>,
    /// Keyed `(vdisk, snapshot)`; a BTreeMap so the manifest encodes
    /// deterministically.
    snapshots: BTreeMap<(u64, u64), SnapshotState>,
    anchor_generation: u64,
    /// The data generation (replication's era) — see format.rs's Anchor.
    era: u64,
}

/// What a WAL entry says. The encoding is fixed little-endian, one byte of
/// kind then the fields — small enough that hand-rolling beats a format
/// dependency, matching format.rs's position.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WalEntry {
    CreateVdisk {
        id: u64,
        size_bytes: u64,
    },
    MapWrite {
        vdisk: u64,
        index: u64,
        hash: BlockHash,
    },
    TrimBlock {
        vdisk: u64,
        index: u64,
    },
    DeleteVdisk {
        id: u64,
    },
}

impl WalEntry {
    fn encode(&self) -> Vec<u8> {
        match self {
            WalEntry::CreateVdisk { id, size_bytes } => {
                let mut buf = vec![1u8];
                buf.extend_from_slice(&id.to_le_bytes());
                buf.extend_from_slice(&size_bytes.to_le_bytes());
                buf
            }
            WalEntry::MapWrite { vdisk, index, hash } => {
                let mut buf = vec![2u8];
                buf.extend_from_slice(&vdisk.to_le_bytes());
                buf.extend_from_slice(&index.to_le_bytes());
                buf.extend_from_slice(hash.as_bytes());
                buf
            }
            WalEntry::TrimBlock { vdisk, index } => {
                let mut buf = vec![3u8];
                buf.extend_from_slice(&vdisk.to_le_bytes());
                buf.extend_from_slice(&index.to_le_bytes());
                buf
            }
            WalEntry::DeleteVdisk { id } => {
                let mut buf = vec![4u8];
                buf.extend_from_slice(&id.to_le_bytes());
                buf
            }
        }
    }

    fn decode(buf: &[u8]) -> Option<WalEntry> {
        match buf.first()? {
            1 if buf.len() == 17 => Some(WalEntry::CreateVdisk {
                id: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                size_bytes: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
            }),
            2 if buf.len() == 49 => Some(WalEntry::MapWrite {
                vdisk: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                index: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
                hash: BlockHash::from_bytes(buf[17..49].try_into().unwrap()),
            }),
            3 if buf.len() == 17 => Some(WalEntry::TrimBlock {
                vdisk: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                index: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
            }),
            4 if buf.len() == 9 => Some(WalEntry::DeleteVdisk {
                id: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
            }),
            _ => None,
        }
    }
}

impl<D: Disk> Pool<D> {
    /// A freshly formatted brick is a pool with no vdisks: the anchor the
    /// format wrote already says exactly that.
    pub fn create(brick: Brick<D>) -> Result<Pool<D>> {
        Self::open(brick)
    }

    /// Open a pool: the brick has already recovered its extent store; the
    /// anchor names the manifest and where WAL replay begins.
    pub fn open(brick: Brick<D>) -> Result<Pool<D>> {
        let anchor = brick
            .read_best_anchor()?
            .ok_or(FsError::Corrupt("a formatted brick with no valid anchor"))?;

        let mut vdisks = HashMap::new();
        let mut snapshots = BTreeMap::new();
        if anchor.manifest_hash != [0; 32] {
            let manifest_hash = BlockHash::from_bytes(anchor.manifest_hash);
            let manifest = brick
                .get(&manifest_hash)?
                .ok_or(FsError::Corrupt("the anchored manifest block is missing"))?;
            let decoded = decode_manifest(&manifest)?;
            for (id, state) in decoded.vdisks {
                vdisks.insert(id, state);
            }
            for (key, state) in decoded.snapshots {
                snapshots.insert(key, state);
            }
        }

        let (wal_start, wal_size) = brick.wal_bounds();
        let frames = Wal::recover(
            &brick,
            anchor.wal_replay_offset,
            anchor.wal_replay_seq,
            anchor.wal_epoch,
        )?;
        let mut wal = Wal::empty(
            wal_start,
            wal_size,
            anchor.wal_replay_offset,
            anchor.wal_replay_seq,
            anchor.wal_epoch,
        );
        wal.adopt(
            anchor.wal_replay_offset,
            anchor.wal_replay_seq,
            anchor.wal_replay_offset,
        );

        let mut pool = Pool {
            brick,
            wal,
            vdisks,
            snapshots,
            anchor_generation: anchor.generation,
            era: anchor.era,
        };

        // Replay: apply each entry only while everything it references
        // verifies; the first failure is the tail, and the ring continues
        // from exactly there. New frames then write under a strictly
        // higher epoch than anything replay accepted — the fence that
        // keeps this history's debris out of the next recovery's chain.
        let mut max_epoch = anchor.wal_epoch;
        for frame in frames {
            let entry = match WalEntry::decode(&frame.payload) {
                Some(entry) => entry,
                None => break,
            };
            if !pool.apply_replayed(&entry) {
                break;
            }
            max_epoch = max_epoch.max(frame.epoch);
            pool.wal
                .adopt(frame.cursor_after, frame.seq + 1, anchor.wal_replay_offset);
        }
        pool.wal.set_epoch(max_epoch + 1);
        Ok(pool)
    }

    /// Apply one replayed entry if its references hold. `false` ends replay.
    fn apply_replayed(&mut self, entry: &WalEntry) -> bool {
        match entry {
            WalEntry::CreateVdisk { id, size_bytes } => {
                if self.vdisks.contains_key(id) || *size_bytes == 0 {
                    return false;
                }
                self.vdisks.insert(
                    *id,
                    VdiskState {
                        size_bytes: *size_bytes,
                        root: None,
                        dirty: BTreeMap::new(),
                    },
                );
                true
            }
            WalEntry::MapWrite { vdisk, index, hash } => {
                if !self.brick.contains(hash) {
                    return false;
                }
                let capacity = match self.vdisks.get(vdisk) {
                    Some(state) => self.capacity_of(state),
                    None => return false,
                };
                if *index >= capacity {
                    return false;
                }
                self.vdisks
                    .get_mut(vdisk)
                    .unwrap()
                    .dirty
                    .insert(*index, Some(*hash));
                true
            }
            WalEntry::TrimBlock { vdisk, index } => {
                let capacity = match self.vdisks.get(vdisk) {
                    Some(state) => self.capacity_of(state),
                    None => return false,
                };
                if *index >= capacity {
                    return false;
                }
                self.vdisks
                    .get_mut(vdisk)
                    .unwrap()
                    .dirty
                    .insert(*index, None);
                true
            }
            WalEntry::DeleteVdisk { id } => self.vdisks.remove(id).is_some(),
        }
    }

    fn capacity_for(&self, size_bytes: u64) -> u64 {
        size_bytes.div_ceil(self.brick.block_size() as u64)
    }

    fn depth_for_size(&self, size_bytes: u64) -> u32 {
        map::depth_for(
            self.capacity_for(size_bytes),
            map::entries_per_node(self.brick.block_size()),
        )
    }

    fn capacity_of(&self, state: &VdiskState) -> u64 {
        self.capacity_for(state.size_bytes)
    }

    fn depth_of(&self, state: &VdiskState) -> u32 {
        self.depth_for_size(state.size_bytes)
    }

    /// Whether a manifest of these counts still fits its one block — the
    /// stated v1 ceiling on vdisks and snapshots together.
    fn manifest_fits(&self, vdisk_count: usize, snapshot_count: usize) -> bool {
        MANIFEST_HEADER_LEN
            + vdisk_count * MANIFEST_ENTRY_LEN
            + snapshot_count * MANIFEST_SNAPSHOT_LEN
            <= self.brick.block_size() as usize
    }

    /// Create a vdisk. Durable at the next flush, like any write.
    pub fn create_vdisk(&mut self, id: u64, size_bytes: u64) -> Result<()> {
        if self.vdisks.contains_key(&id) {
            return Err(FsError::VdiskExists(id));
        }
        if size_bytes == 0 {
            return Err(FsError::BadGeometry("a vdisk must hold at least one block"));
        }
        if !self.manifest_fits(self.vdisks.len() + 1, self.snapshots.len()) {
            return Err(FsError::ManifestFull);
        }
        let entry = WalEntry::CreateVdisk { id, size_bytes };
        self.wal.append(&mut self.brick, &entry.encode())?;
        self.vdisks.insert(
            id,
            VdiskState {
                size_bytes,
                root: None,
                dirty: BTreeMap::new(),
            },
        );
        Ok(())
    }

    /// Write one block of a vdisk. Old-or-new atomicity comes free: the
    /// payload lands under a fresh content address, and the map flips to it
    /// or doesn't — there is no state in between for a crash to expose.
    pub fn write_block(&mut self, vdisk: u64, index: u64, payload: &[u8]) -> Result<()> {
        let state = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        let capacity = self.capacity_of(state);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let hash = self.brick.put(payload)?;
        let entry = WalEntry::MapWrite { vdisk, index, hash };
        self.wal.append(&mut self.brick, &entry.encode())?;
        self.vdisks
            .get_mut(&vdisk)
            .unwrap()
            .dirty
            .insert(index, Some(hash));
        Ok(())
    }

    /// Unmap one block — a guest's discard. The read contract flips to
    /// "unmapped" immediately; the space itself returns at the collection
    /// after the next checkpoint.
    pub fn trim_block(&mut self, vdisk: u64, index: u64) -> Result<()> {
        let state = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        let capacity = self.capacity_of(state);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let entry = WalEntry::TrimBlock { vdisk, index };
        self.wal.append(&mut self.brick, &entry.encode())?;
        self.vdisks
            .get_mut(&vdisk)
            .unwrap()
            .dirty
            .insert(index, None);
        Ok(())
    }

    /// Forget a vdisk. Its tree becomes unreachable at the checkpoint after
    /// this lands, and its space returns at the collection after that —
    /// deletion is a promise about reachability, reclaim is GC's schedule.
    /// Refused while snapshots still pin the vdisk's history: deleting
    /// those is an explicit act, never a cascade.
    pub fn delete_vdisk(&mut self, id: u64) -> Result<()> {
        if !self.vdisks.contains_key(&id) {
            return Err(FsError::UnknownVdisk(id));
        }
        if self.snapshots.keys().any(|(vdisk, _)| *vdisk == id) {
            return Err(FsError::HasSnapshots(id));
        }
        let entry = WalEntry::DeleteVdisk { id };
        self.wal.append(&mut self.brick, &entry.encode())?;
        self.vdisks.remove(&id);
        Ok(())
    }

    /// Pin the vdisk's current content as a snapshot. Checkpoint-grade and
    /// synchronous: everything pending settles first, and the pin is
    /// durable when the call returns — there is no window where a snapshot
    /// exists in memory but not on disk. The pinned root is exactly what a
    /// clone starts from and a rollback returns to.
    pub fn snapshot_vdisk(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        if !self.vdisks.contains_key(&vdisk) {
            return Err(FsError::UnknownVdisk(vdisk));
        }
        if self.snapshots.contains_key(&(vdisk, snapshot)) {
            return Err(FsError::SnapshotExists { vdisk, snapshot });
        }
        if !self.manifest_fits(self.vdisks.len(), self.snapshots.len() + 1) {
            return Err(FsError::ManifestFull);
        }
        // First checkpoint settles the root being pinned; the second makes
        // the pin durable. A crash between them means the snapshot simply
        // never happened — the settling was just an ordinary checkpoint.
        self.checkpoint()?;
        let state = &self.vdisks[&vdisk];
        self.snapshots.insert(
            (vdisk, snapshot),
            SnapshotState {
                size_bytes: state.size_bytes,
                root: state.root,
            },
        );
        self.checkpoint()
    }

    /// Unpin a snapshot. Durable on return; whatever history only the pin
    /// kept alive returns to free space at the next collection.
    pub fn delete_snapshot(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        if self.snapshots.remove(&(vdisk, snapshot)).is_none() {
            return Err(FsError::UnknownSnapshot { vdisk, snapshot });
        }
        self.checkpoint()
    }

    /// Return a vdisk to a snapshot's content, durably, discarding whatever
    /// was written since — a rollback is a statement that the present is
    /// wrong, and half-keeping it would be keeping it.
    pub fn rollback_vdisk(&mut self, vdisk: u64, snapshot: u64) -> Result<()> {
        let snap = *self
            .snapshots
            .get(&(vdisk, snapshot))
            .ok_or(FsError::UnknownSnapshot { vdisk, snapshot })?;
        let state = self
            .vdisks
            .get_mut(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        state.dirty.clear();
        state.root = snap.root;
        state.size_bytes = snap.size_bytes;
        self.checkpoint()
    }

    /// A writable clone: a new vdisk whose starting content is a snapshot.
    /// It shares every block and map node with its source until writes
    /// diverge — the copy in copy-on-write, done by dedupe rather than by
    /// copying anything.
    pub fn clone_vdisk(&mut self, new_id: u64, vdisk: u64, snapshot: u64) -> Result<()> {
        if self.vdisks.contains_key(&new_id) {
            return Err(FsError::VdiskExists(new_id));
        }
        let snap = *self
            .snapshots
            .get(&(vdisk, snapshot))
            .ok_or(FsError::UnknownSnapshot { vdisk, snapshot })?;
        if !self.manifest_fits(self.vdisks.len() + 1, self.snapshots.len()) {
            return Err(FsError::ManifestFull);
        }
        self.vdisks.insert(
            new_id,
            VdiskState {
                size_bytes: snap.size_bytes,
                root: snap.root,
                dirty: BTreeMap::new(),
            },
        );
        self.checkpoint()
    }

    /// Read one block of a snapshot — the vdisk as it was at the pin.
    pub fn read_snapshot_block(
        &self,
        vdisk: u64,
        snapshot: u64,
        index: u64,
    ) -> Result<Option<Vec<u8>>> {
        let snap = self
            .snapshots
            .get(&(vdisk, snapshot))
            .ok_or(FsError::UnknownSnapshot { vdisk, snapshot })?;
        let capacity = self.capacity_for(snap.size_bytes);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let hash = match &snap.root {
            Some(root) => map::lookup(
                &self.brick,
                root,
                self.depth_for_size(snap.size_bytes),
                index,
            )?,
            None => None,
        };
        match hash {
            Some(hash) => match self.brick.get(&hash)? {
                Some(payload) => Ok(Some(payload)),
                None => Err(FsError::Corrupt("a mapped block is missing from the store")),
            },
            None => Ok(None),
        }
    }

    /// Every snapshot as `(vdisk, snapshot, size_bytes)`, sorted.
    pub fn snapshots(&self) -> Vec<(u64, u64, u64)> {
        self.snapshots
            .iter()
            .map(|((vdisk, snapshot), state)| (*vdisk, *snapshot, state.size_bytes))
            .collect()
    }

    /// Read one block. `Ok(None)` is "never written" — zeros, in the
    /// language of the block device this will eventually back.
    pub fn read_block(&self, vdisk: u64, index: u64) -> Result<Option<Vec<u8>>> {
        let state = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        let capacity = self.capacity_of(state);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let hash = match state.dirty.get(&index) {
            Some(Some(hash)) => Some(*hash),
            // Trimmed since the last checkpoint: unmapped, whatever the
            // tree still says underneath.
            Some(None) => None,
            None => match &state.root {
                Some(root) => map::lookup(&self.brick, root, self.depth_of(state), index)?,
                None => None,
            },
        };
        match hash {
            Some(hash) => match self.brick.get(&hash)? {
                Some(payload) => Ok(Some(payload)),
                // The map promised a block the store cannot produce: that
                // is corruption to repair, never a quiet zero-fill.
                None => Err(FsError::Corrupt("a mapped block is missing from the store")),
            },
            None => Ok(None),
        }
    }

    /// The acknowledgement barrier: every write and create before this call
    /// is durable when it returns.
    pub fn flush(&mut self) -> Result<()> {
        self.brick.flush()
    }

    /// Fold every dirty map into its tree, anchor the result, and retire
    /// the WAL's history. Two flushes; see the module header for why the
    /// order cannot lie.
    pub fn checkpoint(&mut self) -> Result<()> {
        let mut ids: Vec<u64> = self.vdisks.keys().copied().collect();
        ids.sort_unstable();

        for id in &ids {
            let state = &self.vdisks[id];
            if state.dirty.is_empty() {
                continue;
            }
            let depth = self.depth_of(state);
            let state = self.vdisks.get_mut(id).unwrap();
            let dirty = std::mem::take(&mut state.dirty);
            let previous = state.root;
            let root = map::fold(&mut self.brick, previous.as_ref(), depth, &dirty)?;
            self.vdisks.get_mut(id).unwrap().root = root;
        }

        let manifest_hash = if self.vdisks.is_empty() && self.snapshots.is_empty() {
            [0u8; 32]
        } else {
            let manifest = encode_manifest(&ids, &self.vdisks, &self.snapshots);
            *self.brick.put(&manifest)?.as_bytes()
        };
        self.brick.flush()?;

        self.wal.retire_to_cursor();
        let (wal_replay_offset, wal_replay_seq) = self.wal.position();
        self.anchor_generation += 1;
        self.brick.write_anchor(&Anchor {
            generation: self.anchor_generation,
            wal_replay_offset,
            wal_replay_seq,
            wal_epoch: self.wal.epoch(),
            era: self.era,
            manifest_hash,
        })?;
        self.brick.flush()
    }

    /// Collect garbage: checkpoint, mark everything reachable from the
    /// anchored state, and hand the brick the live set to sweep against.
    ///
    /// The checkpoint is not an optimization — it is the correctness
    /// precondition. It retires the WAL and empties every dirty map, so
    /// afterwards liveness is computable from the manifest and trees alone;
    /// without it, a swept block could still be referenced by a live WAL
    /// entry, and replay after a crash would mistake an acknowledged write
    /// for an invalid tail.
    pub fn collect_garbage(&mut self) -> Result<GcStats> {
        // A collection writes before it frees — the checkpoint below folds
        // dirty maps into new tree nodes, and compaction rewrites live
        // records ahead of releasing their segments. So it spends the
        // brick's reserve, which exists for exactly this and is closed
        // again however this ends.
        self.brick.open_reserve();
        let outcome = self.collect_within_reserve();
        self.brick.close_reserve();
        outcome
    }

    fn collect_within_reserve(&mut self) -> Result<GcStats> {
        self.checkpoint()?;

        let mut ids: Vec<u64> = self.vdisks.keys().copied().collect();
        ids.sort_unstable();
        let mut live: HashSet<BlockHash> = HashSet::new();
        if !(self.vdisks.is_empty() && self.snapshots.is_empty()) {
            // The same bytes the checkpoint just anchored, so the same hash
            // — recomputed rather than remembered, one source of truth.
            live.insert(hash_block(&encode_manifest(
                &ids,
                &self.vdisks,
                &self.snapshots,
            )));
        }
        let mut roots: Vec<(Option<BlockHash>, u32)> = ids
            .iter()
            .map(|id| {
                let state = &self.vdisks[id];
                (state.root, self.depth_of(state))
            })
            .collect();
        // Pinned history is exactly as live as the present.
        roots.extend(
            self.snapshots
                .values()
                .map(|snap| (snap.root, self.depth_for_size(snap.size_bytes))),
        );
        for (root, depth) in roots {
            if let Some(root) = root {
                map::walk(&self.brick, &root, depth, &mut |item| match item {
                    map::MapItem::Node(hash) => {
                        live.insert(hash);
                    }
                    map::MapItem::Block { hash, .. } => {
                        live.insert(hash);
                    }
                })?;
            }
        }
        self.brick.retain_and_reclaim(&live)
    }

    /// Verify everything: every stored block against its content address,
    /// and every map reference against the store. Repair is phase 2's
    /// business; a truthful report is this one's. A missing *map node* is
    /// an error rather than a report line — with the tree's shape gone,
    /// there is no honest way to say which blocks are missing.
    pub fn scrub(&self) -> Result<ScrubReport> {
        let (blocks_verified, corrupt) = self.brick.scrub()?;
        let mut missing = Vec::new();
        let mut ids: Vec<u64> = self.vdisks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let state = &self.vdisks[&id];
            for (index, value) in &state.dirty {
                if let Some(hash) = value {
                    if !self.brick.contains(hash) {
                        missing.push((id, *index));
                    }
                }
            }
            if let Some(root) = &state.root {
                let mut tree_refs = Vec::new();
                map::walk(&self.brick, root, self.depth_of(state), &mut |item| {
                    if let map::MapItem::Block { index, hash } = item {
                        tree_refs.push((index, hash));
                    }
                })?;
                for (index, hash) in tree_refs {
                    // A dirty entry supersedes the tree at this index; only
                    // the reference a read would actually follow counts.
                    if !state.dirty.contains_key(&index) && !self.brick.contains(&hash) {
                        missing.push((id, index));
                    }
                }
            }
        }
        // Pinned history answers reads too, so it scrubs like the present;
        // a hole is reported under the vdisk the snapshot belongs to.
        for ((vdisk, _), snap) in &self.snapshots {
            if let Some(root) = &snap.root {
                let mut tree_refs = Vec::new();
                map::walk(
                    &self.brick,
                    root,
                    self.depth_for_size(snap.size_bytes),
                    &mut |item| {
                        if let map::MapItem::Block { index, hash } = item {
                            tree_refs.push((index, hash));
                        }
                    },
                )?;
                for (index, hash) in tree_refs {
                    if !self.brick.contains(&hash) {
                        missing.push((*vdisk, index));
                    }
                }
            }
        }
        missing.sort_unstable();
        missing.dedup();
        Ok(ScrubReport {
            blocks_verified,
            corrupt,
            missing,
        })
    }

    /// The pool's block size — the unit bytes.rs translates to.
    pub fn block_size(&self) -> u32 {
        self.brick.block_size()
    }

    /// How the brick's space stands. A caller that only ever learns about
    /// pressure from [`FsError::Full`] learns too late: by then every write
    /// triggers a collection, and the pool spends its time collecting
    /// rather than storing. This is how policy sees the cliff coming.
    pub fn space(&self) -> BrickStats {
        self.brick.stats()
    }

    // -----------------------------------------------------------------
    // The replication layer's entry points (repl.rs). Everything here is
    // expressible through the public API's semantics; these exist so a
    // peer's stream can move blocks by address instead of re-shipping
    // payload logic through the guest-facing calls.

    /// The data generation this pool's state belongs to.
    pub fn era(&self) -> u64 {
        self.era
    }

    /// This node is continuing without its peer, on a fence verdict the
    /// caller holds: the state that follows belongs to a new generation,
    /// and the fact is anchored before any of that state exists.
    pub fn bump_era(&mut self) -> Result<()> {
        self.era += 1;
        self.checkpoint()
    }

    /// Store a payload without mapping it anywhere — a replicated block
    /// arriving ahead of the operation that references it.
    pub fn put_block(&mut self, payload: &[u8]) -> Result<BlockHash> {
        self.brick.put(payload)
    }

    /// Whether the store holds a block, by address.
    pub fn has_block(&self, hash: &BlockHash) -> bool {
        self.brick.contains(hash)
    }

    /// A stored block's payload, by address — what a resync source serves.
    pub fn block_payload(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        self.brick.get(hash)
    }

    /// A map write whose payload is already in the store — the replicated
    /// form of [`Pool::write_block`]. Refusing an absent block is what
    /// keeps a reordered or truncated stream from mapping a promise the
    /// store cannot keep.
    pub fn write_block_prehashed(&mut self, vdisk: u64, index: u64, hash: BlockHash) -> Result<()> {
        let state = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        let capacity = self.capacity_of(state);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        if !self.brick.contains(&hash) {
            return Err(FsError::Corrupt(
                "a replicated write names a block the store does not hold",
            ));
        }
        let entry = WalEntry::MapWrite { vdisk, index, hash };
        self.wal.append(&mut self.brick, &entry.encode())?;
        self.vdisks
            .get_mut(&vdisk)
            .unwrap()
            .dirty
            .insert(index, Some(hash));
        Ok(())
    }

    /// The settled state a resync source offers: era, vdisks, snapshots,
    /// roots. Meaningful only immediately after a checkpoint — the caller
    /// owns that ordering.
    pub fn sync_manifest(&self) -> (u64, Vec<VdiskOffer>, Vec<SnapshotOffer>) {
        let mut vdisks: Vec<VdiskOffer> = self
            .vdisks
            .iter()
            .map(|(id, state)| (*id, state.size_bytes, state.root))
            .collect();
        vdisks.sort_unstable_by_key(|(id, _, _)| *id);
        let snapshots = self
            .snapshots
            .iter()
            .map(|((vdisk, snapshot), state)| (*vdisk, *snapshot, state.size_bytes, state.root))
            .collect();
        (self.era, vdisks, snapshots)
    }

    /// Become the offered state: replace every vdisk and snapshot with the
    /// source's, adopt its era, and anchor it all. The blocks under every
    /// root must already be in the store — the resync's tree walk is what
    /// put them there — and whatever this node held before becomes garbage
    /// for the next collection. This is how a stale node stops being
    /// stale, and the discard of its divergent unacknowledged history is
    /// the point, not a side effect.
    pub fn adopt_sync(
        &mut self,
        era: u64,
        vdisks: &[VdiskOffer],
        snapshots: &[SnapshotOffer],
    ) -> Result<()> {
        for (_, _, root) in vdisks {
            if let Some(root) = root {
                if !self.brick.contains(root) {
                    return Err(FsError::Corrupt("adopting a root the store does not hold"));
                }
            }
        }
        for (_, _, _, root) in snapshots {
            if let Some(root) = root {
                if !self.brick.contains(root) {
                    return Err(FsError::Corrupt("adopting a root the store does not hold"));
                }
            }
        }
        self.vdisks = vdisks
            .iter()
            .map(|(id, size_bytes, root)| {
                (
                    *id,
                    VdiskState {
                        size_bytes: *size_bytes,
                        root: *root,
                        dirty: BTreeMap::new(),
                    },
                )
            })
            .collect();
        self.snapshots = snapshots
            .iter()
            .map(|(vdisk, snapshot, size_bytes, root)| {
                (
                    (*vdisk, *snapshot),
                    SnapshotState {
                        size_bytes: *size_bytes,
                        root: *root,
                    },
                )
            })
            .collect();
        self.era = era;
        self.checkpoint()
    }

    /// One vdisk's size in bytes.
    pub fn vdisk_size(&self, id: u64) -> Result<u64> {
        self.vdisks
            .get(&id)
            .map(|state| state.size_bytes)
            .ok_or(FsError::UnknownVdisk(id))
    }

    /// Every vdisk's `(id, size_bytes)`, sorted — the listing a console
    /// will eventually render.
    pub fn vdisks(&self) -> Vec<(u64, u64)> {
        let mut all: Vec<(u64, u64)> = self
            .vdisks
            .iter()
            .map(|(id, state)| (*id, state.size_bytes))
            .collect();
        all.sort_unstable();
        all
    }

    pub fn into_brick(self) -> Brick<D> {
        self.brick
    }
}

fn root_bytes(root: &Option<BlockHash>) -> [u8; 32] {
    root.map(|hash| *hash.as_bytes()).unwrap_or([0u8; 32])
}

fn root_from_bytes(bytes: [u8; 32]) -> Option<BlockHash> {
    if bytes == [0u8; 32] {
        None
    } else {
        Some(BlockHash::from_bytes(bytes))
    }
}

fn encode_manifest(
    ids: &[u64],
    vdisks: &HashMap<u64, VdiskState>,
    snapshots: &BTreeMap<(u64, u64), SnapshotState>,
) -> Vec<u8> {
    let mut buf = vec![
        0u8;
        MANIFEST_HEADER_LEN
            + ids.len() * MANIFEST_ENTRY_LEN
            + snapshots.len() * MANIFEST_SNAPSHOT_LEN
    ];
    buf[0..8].copy_from_slice(MANIFEST_MAGIC);
    buf[8..12].copy_from_slice(&MANIFEST_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(ids.len() as u32).to_le_bytes());
    buf[16..20].copy_from_slice(&(snapshots.len() as u32).to_le_bytes());
    let mut at = MANIFEST_HEADER_LEN;
    for id in ids {
        let state = &vdisks[id];
        buf[at..at + 8].copy_from_slice(&id.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&state.size_bytes.to_le_bytes());
        buf[at + 16..at + 48].copy_from_slice(&root_bytes(&state.root));
        at += MANIFEST_ENTRY_LEN;
    }
    for ((vdisk, snapshot), state) in snapshots {
        buf[at..at + 8].copy_from_slice(&vdisk.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&snapshot.to_le_bytes());
        buf[at + 16..at + 24].copy_from_slice(&state.size_bytes.to_le_bytes());
        buf[at + 24..at + 56].copy_from_slice(&root_bytes(&state.root));
        at += MANIFEST_SNAPSHOT_LEN;
    }
    buf
}

struct DecodedManifest {
    vdisks: Vec<(u64, VdiskState)>,
    snapshots: Vec<((u64, u64), SnapshotState)>,
}

fn decode_manifest(buf: &[u8]) -> Result<DecodedManifest> {
    if buf.len() < MANIFEST_HEADER_LEN || &buf[0..8] != MANIFEST_MAGIC {
        return Err(FsError::Corrupt("the manifest block has the wrong shape"));
    }
    if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != MANIFEST_VERSION {
        return Err(FsError::Corrupt(
            "the manifest block is a version this build does not speak",
        ));
    }
    let vdisk_count = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    let snapshot_count = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
    if buf.len()
        < MANIFEST_HEADER_LEN
            + vdisk_count * MANIFEST_ENTRY_LEN
            + snapshot_count * MANIFEST_SNAPSHOT_LEN
    {
        return Err(FsError::Corrupt(
            "the manifest block is shorter than its counts",
        ));
    }
    let mut vdisks = Vec::with_capacity(vdisk_count);
    let mut at = MANIFEST_HEADER_LEN;
    for _ in 0..vdisk_count {
        let id = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        let size_bytes = u64::from_le_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let root = root_from_bytes(buf[at + 16..at + 48].try_into().unwrap());
        vdisks.push((
            id,
            VdiskState {
                size_bytes,
                root,
                dirty: BTreeMap::new(),
            },
        ));
        at += MANIFEST_ENTRY_LEN;
    }
    let mut snapshots = Vec::with_capacity(snapshot_count);
    for _ in 0..snapshot_count {
        let vdisk = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        let snapshot = u64::from_le_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let size_bytes = u64::from_le_bytes(buf[at + 16..at + 24].try_into().unwrap());
        let root = root_from_bytes(buf[at + 24..at + 56].try_into().unwrap());
        snapshots.push(((vdisk, snapshot), SnapshotState { size_bytes, root }));
        at += MANIFEST_SNAPSHOT_LEN;
    }
    Ok(DecodedManifest { vdisks, snapshots })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::BrickParams;
    use crate::sim::SimDisk;

    const KIB: u64 = 1024;
    const BLOCK: usize = 4 * KIB as usize;

    fn params() -> BrickParams {
        BrickParams {
            pool_uuid: [0xAA; 16],
            brick_uuid: [0xBB; 16],
            block_size: BLOCK as u32,
            segment_size: 128 * KIB,
            wal_size: 32 * KIB,
        }
    }

    fn pool(seed: u64) -> Pool<SimDisk> {
        Pool::create(Brick::format(SimDisk::new(8 * KIB * KIB, seed), params()).unwrap()).unwrap()
    }

    fn reopen(pool: Pool<SimDisk>) -> Pool<SimDisk> {
        Pool::open(Brick::open(pool.into_brick().into_disk()).unwrap()).unwrap()
    }

    #[test]
    fn a_write_survives_reopen_through_the_wal_alone() {
        let mut pool = pool(1);
        pool.create_vdisk(7, 40 * BLOCK as u64).unwrap();
        pool.write_block(7, 3, b"through the wal").unwrap();
        pool.flush().unwrap();
        // No checkpoint: reopen leans entirely on anchor + replay.
        let pool = reopen(pool);
        assert_eq!(pool.read_block(7, 3).unwrap().unwrap(), b"through the wal");
        assert_eq!(pool.read_block(7, 4).unwrap(), None);
        assert_eq!(pool.vdisks(), vec![(7, 40 * BLOCK as u64)]);
    }

    #[test]
    fn a_write_survives_reopen_through_the_tree_alone() {
        let mut pool = pool(2);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 9, b"through the tree").unwrap();
        pool.checkpoint().unwrap();
        // The checkpoint retired the WAL: reopen leans on the manifest.
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 9).unwrap().unwrap(), b"through the tree");
    }

    #[test]
    fn an_overwrite_reads_back_newest_before_and_after_checkpoint() {
        let mut pool = pool(3);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 0, b"first").unwrap();
        pool.write_block(1, 0, b"second").unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"second");
        pool.checkpoint().unwrap();
        pool.write_block(1, 0, b"third").unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"third");
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"third");
    }

    #[test]
    fn a_depth_two_vdisk_round_trips_across_checkpoints() {
        let mut pool = pool(4);
        // 4 KiB blocks: 128 entries per node; 200 blocks needs depth 2.
        pool.create_vdisk(1, 200 * BLOCK as u64).unwrap();
        pool.write_block(1, 0, b"low").unwrap();
        pool.write_block(1, 199, b"high").unwrap();
        pool.checkpoint().unwrap();
        pool.write_block(1, 150, b"mid").unwrap();
        pool.flush().unwrap();
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"low");
        assert_eq!(pool.read_block(1, 150).unwrap().unwrap(), b"mid");
        assert_eq!(pool.read_block(1, 199).unwrap().unwrap(), b"high");
        assert_eq!(pool.read_block(1, 100).unwrap(), None);
    }

    #[test]
    fn two_vdisks_do_not_bleed_into_each_other() {
        let mut pool = pool(5);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.create_vdisk(2, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 5, b"one's data").unwrap();
        pool.write_block(2, 5, b"two's data").unwrap();
        pool.checkpoint().unwrap();
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 5).unwrap().unwrap(), b"one's data");
        assert_eq!(pool.read_block(2, 5).unwrap().unwrap(), b"two's data");
    }

    #[test]
    fn a_full_wal_is_an_error_a_checkpoint_cures() {
        let mut pool = pool(6);
        pool.create_vdisk(1, 4000 * BLOCK as u64).unwrap();
        let mut hit_full = false;
        for i in 0..4000u64 {
            match pool.write_block(1, i, &i.to_le_bytes()) {
                Ok(()) => {}
                Err(FsError::WalFull) => {
                    hit_full = true;
                    pool.checkpoint().unwrap();
                    pool.write_block(1, i, &i.to_le_bytes()).unwrap();
                    break;
                }
                Err(other) => panic!("{other}"),
            }
        }
        assert!(hit_full, "the ring never filled — grow the workload");
    }

    #[test]
    fn the_named_refusals_hold() {
        let mut pool = pool(7);
        pool.create_vdisk(1, 10 * BLOCK as u64).unwrap();
        assert_eq!(
            pool.create_vdisk(1, 10 * BLOCK as u64).unwrap_err(),
            FsError::VdiskExists(1)
        );
        assert_eq!(
            pool.write_block(9, 0, b"x").unwrap_err(),
            FsError::UnknownVdisk(9)
        );
        assert_eq!(
            pool.write_block(1, 10, b"x").unwrap_err(),
            FsError::OutOfRange {
                index: 10,
                capacity: 10
            }
        );
        assert_eq!(
            pool.read_block(1, 99).unwrap_err(),
            FsError::OutOfRange {
                index: 99,
                capacity: 10
            }
        );
    }

    #[test]
    fn an_empty_pool_reopens_empty_after_a_checkpoint() {
        let mut pool = pool(8);
        pool.checkpoint().unwrap();
        let pool = reopen(pool);
        assert_eq!(pool.vdisks(), vec![]);
    }

    #[test]
    fn a_trim_unmaps_through_wal_and_tree_alike() {
        let mut pool = pool(9);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 3, b"here today").unwrap();
        pool.checkpoint().unwrap();
        pool.trim_block(1, 3).unwrap();
        // The trim overlays the checkpointed tree immediately...
        assert_eq!(pool.read_block(1, 3).unwrap(), None);
        pool.flush().unwrap();
        // ...survives reopen through the WAL...
        let mut pool = reopen(pool);
        assert_eq!(pool.read_block(1, 3).unwrap(), None);
        pool.checkpoint().unwrap();
        // ...and survives reopen through the folded tree.
        let mut pool = reopen(pool);
        assert_eq!(pool.read_block(1, 3).unwrap(), None);
        pool.write_block(1, 3, b"back again").unwrap();
        assert_eq!(pool.read_block(1, 3).unwrap().unwrap(), b"back again");
    }

    #[test]
    fn a_deleted_vdisk_is_gone_through_wal_and_manifest_alike() {
        let mut pool = pool(10);
        pool.create_vdisk(1, 10 * BLOCK as u64).unwrap();
        pool.create_vdisk(2, 10 * BLOCK as u64).unwrap();
        pool.checkpoint().unwrap();
        pool.delete_vdisk(1).unwrap();
        pool.flush().unwrap();
        let mut pool = reopen(pool);
        assert_eq!(pool.vdisks(), vec![(2, 10 * BLOCK as u64)]);
        assert_eq!(pool.read_block(1, 0).unwrap_err(), FsError::UnknownVdisk(1));
        pool.checkpoint().unwrap();
        let mut pool = reopen(pool);
        assert_eq!(pool.vdisks(), vec![(2, 10 * BLOCK as u64)]);
        // The id is free again — a new vdisk, not a resurrection.
        pool.create_vdisk(1, 20 * BLOCK as u64).unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap(), None);
    }

    #[test]
    fn a_collection_reclaims_overwritten_history_and_current_data_survives() {
        let mut pool = pool(11);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        // Rounds of overwrites with checkpoints between: old data blocks,
        // old tree nodes, and old manifests all become garbage.
        for round in 0..6u8 {
            for index in 0..20u64 {
                let mut payload = vec![round; 3000];
                payload[0..8].copy_from_slice(&index.to_le_bytes());
                pool.write_block(1, index, &payload).unwrap();
            }
            pool.checkpoint().unwrap();
        }
        let stats = pool.collect_garbage().unwrap();
        assert!(stats.blocks_dropped > 0, "nothing collected: {stats:?}");
        assert!(stats.segments_freed > 0, "nothing reclaimed: {stats:?}");
        for index in 0..20u64 {
            let payload = pool.read_block(1, index).unwrap().unwrap();
            assert_eq!(payload[8], 5, "index {index} lost its last round");
        }
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 19).unwrap().unwrap()[8], 5);
    }

    #[test]
    fn deleting_a_vdisk_and_collecting_returns_its_space() {
        let mut pool = pool(12);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.create_vdisk(2, 40 * BLOCK as u64).unwrap();
        for index in 0..30u64 {
            pool.write_block(1, index, &[7u8; 2000]).unwrap();
            let mut other = vec![8u8; 2000];
            other[0..8].copy_from_slice(&index.to_le_bytes());
            pool.write_block(2, index, &other).unwrap();
        }
        pool.checkpoint().unwrap();
        pool.delete_vdisk(1).unwrap();
        let stats = pool.collect_garbage().unwrap();
        assert!(
            stats.blocks_dropped > 0,
            "the dead tree lingered: {stats:?}"
        );
        // The survivor is untouched, here and after reopen.
        let pool = reopen(pool);
        assert_eq!(
            pool.read_block(2, 12).unwrap().unwrap()[8..12],
            [8, 8, 8, 8]
        );
        assert_eq!(pool.vdisks().len(), 1);
    }

    #[test]
    fn a_brick_that_has_run_out_can_still_be_collected() {
        // The liveness hazard a real burn-in found on its third round: a
        // collection has to write before it can free, so a brick with
        // nothing left would be uncollectable exactly when collecting is
        // the only thing that helps. The reserve is what breaks that
        // circle, and this is the shape of the failure it prevents.
        let mut pool = pool(20);
        pool.create_vdisk(1, 200 * BLOCK as u64).unwrap();
        let mut round = 0u8;
        loop {
            round = round.wrapping_add(1);
            let mut hit_full = false;
            for index in 0..200u64 {
                let mut payload = vec![round; 3000];
                payload[0..8].copy_from_slice(&index.to_le_bytes());
                match pool.write_block(1, index, &payload) {
                    Ok(()) => {}
                    Err(FsError::WalFull) => pool.checkpoint().unwrap(),
                    Err(FsError::Full) => {
                        hit_full = true;
                        break;
                    }
                    Err(other) => panic!("{other}"),
                }
            }
            if hit_full {
                break;
            }
            assert!(round < 200, "the brick never filled");
        }

        // Out of room — and a collection must still run and give room back.
        let stats = pool.collect_garbage().unwrap();
        assert!(
            stats.segments_freed > 0,
            "the collection freed nothing: {stats:?}"
        );
        pool.write_block(1, 0, b"room again").unwrap();
        pool.checkpoint().unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"room again");
    }

    #[test]
    fn a_pool_worked_hard_keeps_storing_instead_of_only_collecting() {
        // The shape of a burn-in that reported twenty passing rounds while
        // doing nothing for sixteen of them: a caller collects below some
        // level, a collection stops at that same level, and from then on
        // every write buys a collection with nothing left to do. Progress
        // may become gradual near full — a copy-on-write store must move a
        // byte to place a byte — but it must not stop.
        let mut pool = pool(21);
        // A vdisk about half the pool, overwritten far past its size: the
        // regime where live data alone denies any generous free-space goal.
        let capacity = 220u64;
        pool.create_vdisk(1, capacity * BLOCK as u64).unwrap();
        let quarter = pool.space().segments_total / 4;

        let mut written = 0u64;
        let mut collections = 0u64;
        for round in 0..12u8 {
            for index in 0..capacity {
                let mut payload = vec![round; 3000];
                payload[0..8].copy_from_slice(&index.to_le_bytes());
                // Make room and try again — a full ring wants a checkpoint,
                // a full brick wants a collection, and neither is a reason
                // to drop the write.
                for attempt in 0..3 {
                    match pool.write_block(1, index, &payload) {
                        Ok(()) => {
                            written += 1;
                            break;
                        }
                        Err(FsError::WalFull) => pool.checkpoint().unwrap(),
                        Err(FsError::Full) => {
                            pool.collect_garbage().unwrap();
                            collections += 1;
                        }
                        Err(other) => panic!("{other}"),
                    }
                    assert!(
                        attempt < 2,
                        "block {index} would not go in after making room twice"
                    );
                }
            }
            // Collect the way a caller should: on the way down, not at the
            // bottom — and then insist it actually bought headroom.
            if pool.space().segments_free <= quarter {
                pool.collect_garbage().unwrap();
                collections += 1;
                assert!(
                    pool.space().segments_free > quarter,
                    "round {round}: a collection left the caller still asking \
                     ({} free, asks at {quarter}) — every write from here pays \
                     for a collection that can do nothing",
                    pool.space().segments_free,
                );
            }
        }

        assert_eq!(written, 12 * capacity, "writing stalled");
        assert!(collections > 0, "the pool never came under pressure");
        for index in 0..capacity {
            assert_eq!(pool.read_block(1, index).unwrap().unwrap()[8], 11);
        }
    }

    #[test]
    fn a_healthy_pool_scrubs_clean() {
        let mut pool = pool(13);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        for index in 0..10u64 {
            pool.write_block(1, index, &index.to_le_bytes()).unwrap();
        }
        pool.checkpoint().unwrap();
        let report = pool.scrub().unwrap();
        assert!(report.blocks_verified > 10, "{report:?}");
        assert_eq!(report.corrupt, vec![]);
        assert_eq!(report.missing, vec![]);
    }

    #[test]
    fn a_snapshot_pins_the_past_while_the_present_moves_on() {
        let mut pool = pool(15);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 5, b"the past").unwrap();
        pool.snapshot_vdisk(1, 100).unwrap();
        pool.write_block(1, 5, b"the present").unwrap();
        pool.write_block(1, 6, b"only now").unwrap();
        pool.checkpoint().unwrap();
        assert_eq!(pool.read_block(1, 5).unwrap().unwrap(), b"the present");
        assert_eq!(
            pool.read_snapshot_block(1, 100, 5).unwrap().unwrap(),
            b"the past"
        );
        assert_eq!(pool.read_snapshot_block(1, 100, 6).unwrap(), None);
        // Both survive a reopen.
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 6).unwrap().unwrap(), b"only now");
        assert_eq!(
            pool.read_snapshot_block(1, 100, 5).unwrap().unwrap(),
            b"the past"
        );
        assert_eq!(pool.snapshots(), vec![(1, 100, 40 * BLOCK as u64)]);
    }

    #[test]
    fn a_rollback_discards_the_present_including_unflushed_writes() {
        let mut pool = pool(16);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 0, b"keep me").unwrap();
        pool.snapshot_vdisk(1, 1).unwrap();
        pool.write_block(1, 0, b"checkpointed over").unwrap();
        pool.checkpoint().unwrap();
        pool.write_block(1, 0, b"not even flushed").unwrap();
        pool.write_block(1, 3, b"collateral").unwrap();
        pool.rollback_vdisk(1, 1).unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"keep me");
        assert_eq!(pool.read_block(1, 3).unwrap(), None);
        // Durable without any further flush: rollback is checkpoint-grade.
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 0).unwrap().unwrap(), b"keep me");
    }

    #[test]
    fn a_clone_diverges_from_its_source_without_disturbing_it() {
        let mut pool = pool(17);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        pool.write_block(1, 2, b"shared history").unwrap();
        pool.snapshot_vdisk(1, 1).unwrap();
        pool.clone_vdisk(9, 1, 1).unwrap();
        assert_eq!(pool.read_block(9, 2).unwrap().unwrap(), b"shared history");
        pool.write_block(9, 2, b"the clone's own").unwrap();
        pool.write_block(1, 2, b"the source's own").unwrap();
        pool.checkpoint().unwrap();
        let pool = reopen(pool);
        assert_eq!(pool.read_block(9, 2).unwrap().unwrap(), b"the clone's own");
        assert_eq!(pool.read_block(1, 2).unwrap().unwrap(), b"the source's own");
        assert_eq!(
            pool.read_snapshot_block(1, 1, 2).unwrap().unwrap(),
            b"shared history"
        );
        assert_eq!(pool.vdisks().len(), 2);
    }

    #[test]
    fn a_vdisk_with_snapshots_refuses_to_die_until_they_do() {
        let mut pool = pool(18);
        pool.create_vdisk(1, 10 * BLOCK as u64).unwrap();
        pool.snapshot_vdisk(1, 7).unwrap();
        assert_eq!(pool.delete_vdisk(1).unwrap_err(), FsError::HasSnapshots(1));
        pool.delete_snapshot(1, 7).unwrap();
        pool.delete_vdisk(1).unwrap();
        assert_eq!(
            pool.delete_snapshot(1, 7).unwrap_err(),
            FsError::UnknownSnapshot {
                vdisk: 1,
                snapshot: 7
            }
        );
    }

    #[test]
    fn gc_spares_what_a_snapshot_pins_and_reclaims_it_when_unpinned() {
        let mut pool = pool(19);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        for index in 0..20u64 {
            let mut payload = vec![0xAB; 3000];
            payload[0..8].copy_from_slice(&index.to_le_bytes());
            pool.write_block(1, index, &payload).unwrap();
        }
        pool.snapshot_vdisk(1, 1).unwrap();
        // Overwrite everything: without the pin, the old blocks would all
        // be garbage now.
        for index in 0..20u64 {
            let mut payload = vec![0xCD; 3000];
            payload[0..8].copy_from_slice(&index.to_le_bytes());
            pool.write_block(1, index, &payload).unwrap();
        }
        pool.collect_garbage().unwrap();
        let old = pool.read_snapshot_block(1, 1, 12).unwrap().unwrap();
        assert_eq!(old[8], 0xAB, "the pinned past was swept");
        // Unpin and collect again: now the history goes.
        pool.delete_snapshot(1, 1).unwrap();
        let stats = pool.collect_garbage().unwrap();
        assert!(
            stats.blocks_dropped >= 20,
            "unpinned history lingered: {stats:?}"
        );
        assert_eq!(pool.read_block(1, 12).unwrap().unwrap()[8], 0xCD);
    }

    #[test]
    fn scrub_names_the_vdisk_block_whose_data_rotted_away() {
        use crate::disk::Disk;
        let mut pool = pool(14);
        pool.create_vdisk(1, 40 * BLOCK as u64).unwrap();
        let marker = b"scrub will come looking for exactly these bytes";
        pool.write_block(1, 6, marker).unwrap();
        pool.checkpoint().unwrap();
        // Rot the payload on the raw device, then reopen: the recovery scan
        // refuses the record, so the store no longer holds a block the tree
        // still references.
        let mut disk = pool.into_brick().into_disk();
        let mut whole = vec![0u8; disk.size() as usize];
        disk.read_at(0, &mut whole).unwrap();
        let at = whole
            .windows(marker.len())
            .position(|w| w == marker)
            .expect("the payload must be somewhere on the disk") as u64;
        disk.write_at(at, &[!marker[0]]).unwrap();
        disk.flush().unwrap();
        let pool = Pool::open(Brick::open(disk).unwrap()).unwrap();
        let report = pool.scrub().unwrap();
        assert_eq!(report.missing, vec![(1, 6)]);
        assert_eq!(
            pool.read_block(1, 6).unwrap_err(),
            FsError::Corrupt("a mapped block is missing from the store")
        );
    }
}
