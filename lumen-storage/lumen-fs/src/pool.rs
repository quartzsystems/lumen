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

use std::collections::{BTreeMap, HashMap};

use crate::brick::Brick;
use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::format::Anchor;
use crate::hash::BlockHash;
use crate::map;
use crate::wal::Wal;

const MANIFEST_MAGIC: &[u8; 8] = b"LFSMAN\0\0";
const MANIFEST_HEADER_LEN: usize = 16; // magic 8 + version 4 + count 4
const MANIFEST_ENTRY_LEN: usize = 48; // id 8 + size 8 + root 32
const MANIFEST_VERSION: u32 = 1;

/// One vdisk's durable identity in the manifest.
#[derive(Debug, Clone)]
struct VdiskState {
    size_bytes: u64,
    /// The checkpointed tree, if any writes have ever been checkpointed.
    root: Option<BlockHash>,
    /// Mutations since the last checkpoint — replayable from the WAL, so
    /// purely in-memory. BTreeMap so a fold is deterministic.
    dirty: BTreeMap<u64, BlockHash>,
}

pub struct Pool<D: Disk> {
    brick: Brick<D>,
    wal: Wal,
    vdisks: HashMap<u64, VdiskState>,
    anchor_generation: u64,
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
        if anchor.manifest_hash != [0; 32] {
            let manifest_hash = BlockHash::from_bytes(anchor.manifest_hash);
            let manifest = brick
                .get(&manifest_hash)?
                .ok_or(FsError::Corrupt("the anchored manifest block is missing"))?;
            for (id, state) in decode_manifest(&manifest)? {
                vdisks.insert(id, state);
            }
        }

        let (wal_start, wal_size) = brick.wal_bounds();
        let frames = Wal::recover(&brick, anchor.wal_replay_offset, anchor.wal_replay_seq)?;
        let mut wal = Wal::empty(
            wal_start,
            wal_size,
            anchor.wal_replay_offset,
            anchor.wal_replay_seq,
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
            anchor_generation: anchor.generation,
        };

        // Replay: apply each entry only while everything it references
        // verifies; the first failure is the tail, and the ring continues
        // from exactly there.
        for frame in frames {
            let entry = match WalEntry::decode(&frame.payload) {
                Some(entry) => entry,
                None => break,
            };
            if !pool.apply_replayed(&entry) {
                break;
            }
            pool.wal
                .adopt(frame.cursor_after, frame.seq + 1, anchor.wal_replay_offset);
        }
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
                    .insert(*index, *hash);
                true
            }
        }
    }

    fn capacity_of(&self, state: &VdiskState) -> u64 {
        state.size_bytes.div_ceil(self.brick.block_size() as u64)
    }

    fn depth_of(&self, state: &VdiskState) -> u32 {
        map::depth_for(
            self.capacity_of(state),
            map::entries_per_node(self.brick.block_size()),
        )
    }

    /// Create a vdisk. Durable at the next flush, like any write.
    pub fn create_vdisk(&mut self, id: u64, size_bytes: u64) -> Result<()> {
        if self.vdisks.contains_key(&id) {
            return Err(FsError::VdiskExists(id));
        }
        if size_bytes == 0 {
            return Err(FsError::BadGeometry("a vdisk must hold at least one block"));
        }
        if (self.vdisks.len() + 1) * MANIFEST_ENTRY_LEN + MANIFEST_HEADER_LEN
            > self.brick.block_size() as usize
        {
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
            .insert(index, hash);
        Ok(())
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
            Some(hash) => Some(*hash),
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
            let root = map::fold(&mut self.brick, state.root.as_ref(), depth, &dirty)?;
            self.vdisks.get_mut(id).unwrap().root = Some(root);
        }

        let manifest_hash = if self.vdisks.is_empty() {
            [0u8; 32]
        } else {
            let manifest = encode_manifest(&ids, &self.vdisks);
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
            manifest_hash,
        })?;
        self.brick.flush()
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

fn encode_manifest(ids: &[u64], vdisks: &HashMap<u64, VdiskState>) -> Vec<u8> {
    let mut buf = vec![0u8; MANIFEST_HEADER_LEN + ids.len() * MANIFEST_ENTRY_LEN];
    buf[0..8].copy_from_slice(MANIFEST_MAGIC);
    buf[8..12].copy_from_slice(&MANIFEST_VERSION.to_le_bytes());
    buf[12..16].copy_from_slice(&(ids.len() as u32).to_le_bytes());
    for (n, id) in ids.iter().enumerate() {
        let state = &vdisks[id];
        let at = MANIFEST_HEADER_LEN + n * MANIFEST_ENTRY_LEN;
        buf[at..at + 8].copy_from_slice(&id.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&state.size_bytes.to_le_bytes());
        if let Some(root) = &state.root {
            buf[at + 16..at + 48].copy_from_slice(root.as_bytes());
        }
    }
    buf
}

fn decode_manifest(buf: &[u8]) -> Result<Vec<(u64, VdiskState)>> {
    if buf.len() < MANIFEST_HEADER_LEN || &buf[0..8] != MANIFEST_MAGIC {
        return Err(FsError::Corrupt("the manifest block has the wrong shape"));
    }
    if u32::from_le_bytes(buf[8..12].try_into().unwrap()) != MANIFEST_VERSION {
        return Err(FsError::Corrupt(
            "the manifest block is a version this build does not speak",
        ));
    }
    let count = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    if buf.len() < MANIFEST_HEADER_LEN + count * MANIFEST_ENTRY_LEN {
        return Err(FsError::Corrupt(
            "the manifest block is shorter than its count",
        ));
    }
    let mut out = Vec::with_capacity(count);
    for n in 0..count {
        let at = MANIFEST_HEADER_LEN + n * MANIFEST_ENTRY_LEN;
        let id = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        let size_bytes = u64::from_le_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let root_bytes: [u8; 32] = buf[at + 16..at + 48].try_into().unwrap();
        let root = if root_bytes == [0u8; 32] {
            None
        } else {
            Some(BlockHash::from_bytes(root_bytes))
        };
        out.push((
            id,
            VdiskState {
                size_bytes,
                root,
                dirty: BTreeMap::new(),
            },
        ));
    }
    Ok(out)
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
}
