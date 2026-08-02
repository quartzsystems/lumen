//! The pool: vdisks over one node's bricks, held together by the WAL, the
//! map trees, and the anchor.
//!
//! Since phase 4 the store under a pool is a [`BrickSet`] — one brick per
//! disk, per tier. The pool's job barely changes: data blocks go to the
//! **vdisk's tier**, map nodes and the manifest go to **tier 0** (the map
//! working set lives on the fastest class, docs/lumenfs.md "Tiers"), and
//! the WAL and anchors live on the one brick the set designates. A
//! single-brick pool is the degenerate set and behaves byte-identically to
//! phase 1–3.
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

/// The byte-granular view a block device speaks.
pub mod bytes;
/// The COW radix trees mapping block index to content address.
pub mod map;
/// The write-ahead ring on the holder brick's aux area.
pub mod wal;

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::{hash_block, BlockHash};
use crate::pool::wal::Wal;
use crate::repl::slice::SliceMap;
use crate::repl::{NodeId, SnapshotOffer, VdiskOffer};
use crate::store::brick::{Brick, BrickStats, GcStats};
use crate::store::brick_set::BrickSet;
use crate::store::format::Anchor;

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
const MANIFEST_HEADER_LEN: usize = 24; // magic 8 + version 4 + three counts
const MANIFEST_ENTRY_LEN: usize = 56; // id 8 + size 8 + tier 1 + pad 7 + root 32
const MANIFEST_SNAPSHOT_LEN: usize = 56; // vdisk 8 + snapshot 8 + size 8 + root 32
const MANIFEST_LEASE_LEN: usize = 24; // vdisk 8 + holder 1 + handing 1 + pad 6 + era 8
/// Version 4's tail: the slice map — its version and 256 home pairs. The
/// map is not recomputable (`reassigned` chains from its predecessor), so
/// the committed assignment is durable state like everything else here.
const MANIFEST_MAP_LEN: usize = 8 + 2 * crate::repl::slice::SLICES;
/// Version 2 added the writer leases; version 3 the per-vdisk tier;
/// version 4 appends the slice map. Version 3 is the one deliberate
/// decode-compat exception to the refuse-by-name posture: it is live on
/// real hardware (the two-member pair), an unplaced pool still *writes*
/// it — so the deployed pair's manifests stay byte-identical and an old
/// binary can still read them — and a v3 manifest is exactly a v4 with no
/// map. Anything older is refused by name, as ever.
const MANIFEST_VERSION: u32 = 3;
const MANIFEST_VERSION_PLACED: u32 = 4;

/// Nobody — the encoded stand-in for "no node", since every real
/// [`NodeId`] is a valid `u8`.
const NO_NODE: u8 = 0xFF;

/// Who may write to a vdisk, and whether that is being handed on.
///
/// Single-writer is a data-integrity property, not a scheduling
/// convenience: two nodes writing one vdisk is the corruption every layer
/// below this has been built to make impossible, and it would arrive
/// through this door. So the lease is durable — it survives a restart
/// rather than being rebuilt from whatever a peer happens to remember —
/// and it names the era it was granted in, because a survivor that has
/// moved on is not bound by the promises of the era it survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub holder: NodeId,
    /// Set while a migration window is open: the guest is running on
    /// `holder` and starting on this node, both have the disk open, and
    /// exactly one of them may write.
    pub handing_to: Option<NodeId>,
    /// The era this lease was granted in.
    pub era: u64,
}

/// One vdisk's durable identity in the manifest.
#[derive(Debug, Clone)]
struct VdiskState {
    size_bytes: u64,
    /// The device class this vdisk's data lives on, named at creation.
    /// Map nodes stay on tier 0 whatever this says — the working set
    /// belongs on the fastest class.
    tier: u8,
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

/// An open two-phase checkpoint: everything the drain and the commit
/// need, captured under the lock by [`Pool::checkpoint_begin`]. The
/// `flushers` are the phase between — run them all, off the lock, before
/// offering the ticket back to [`Pool::checkpoint_commit`]; a ticket
/// whose flushers did not all succeed must be dropped, not committed.
pub struct CheckpointTicket {
    manifest_hash: [u8; 32],
    cursor: u64,
    seq: u64,
    epoch: u64,
    era: u64,
    generation: u64,
    pub flushers: Vec<crate::disk::FlushHandle>,
}

pub struct Pool<D: Disk> {
    store: BrickSet<D>,
    wal: Wal,
    vdisks: HashMap<u64, VdiskState>,
    /// Keyed `(vdisk, snapshot)`; a BTreeMap so the manifest encodes
    /// deterministically.
    snapshots: BTreeMap<(u64, u64), SnapshotState>,
    /// Who may write each vdisk. BTreeMap so the manifest encodes
    /// deterministically.
    leases: BTreeMap<u64, Lease>,
    anchor_generation: u64,
    /// The data generation (replication's era) — see format.rs's Anchor.
    era: u64,
    /// What each peer session holds live beyond the manifest. Volatile on
    /// purpose: a crash ends the sessions that needed them, and the next
    /// sessions pin their own.
    session_pins: BTreeMap<NodeId, SessionPins>,
    /// Which member this pool is, and which slices it homes. `None` is
    /// "hold everything" — the single-node tools, and every pool from
    /// before placement existed. Handed in at open (WAL replay's validity
    /// checks consult it); the committed map persists in the manifest.
    placement: Option<Placement>,
    /// A reassignment in flight: the map the pool is moving *to*. While
    /// present, storage answers for the union of both maps — a write may
    /// land under either — and reads route by the committed map alone,
    /// whose homes are guaranteed to hold. Volatile on purpose: the layer
    /// that ordered the reassignment re-delivers it after a crash, and
    /// the moves are Merkle-idempotent to re-run.
    pending_map: Option<SliceMap>,
}

/// This node's seat in the slice map: who it is, and therefore which data
/// blocks its bricks are homes for. Map nodes and manifests are exempt —
/// metadata lives on every member.
#[derive(Debug, Clone)]
struct Placement {
    node: NodeId,
    map: SliceMap,
}

/// One peer session's claims against garbage collection: the roots of a
/// resync offer in flight on that session, and payloads that arrived
/// ahead of the ops that will reference them. Both are blocks the manifest
/// does not reach yet, and both must outlive any collection that runs
/// while the session does.
#[derive(Debug, Default)]
struct SessionPins {
    /// Offer roots as `(hash, depth, data_tier)` — depth as map::walk
    /// counts it, and the tier the tree's *data* blocks live on (its
    /// nodes are tier 0 like every map node).
    offer: Vec<(BlockHash, u32, u8)>,
    /// Blocks delivered ahead of their op, as `(store_tier, hash)` —
    /// pinned exactly until the referencing op lands, because a
    /// collection in that gap would sweep the block and the op's replay
    /// would then name a block the store does not hold.
    inflight: HashSet<(u8, BlockHash)>,
}

/// How much of an offer an adoption replaces. `Whole` is the two-node
/// resync: the target becomes the offer, and every vdisk the offer does
/// not name is discarded with the rest of the divergent history. `Named`
/// replaces only the vdisks the offer carries and leaves the rest
/// standing — the shape a three-member rejoin needs, where each vdisk is
/// adopted from *its lease holder's* offer and no single peer speaks for
/// all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptScope {
    Whole,
    Named,
}

/// What a WAL entry says. The encoding is fixed little-endian, one byte of
/// kind then the fields — small enough that hand-rolling beats a format
/// dependency, matching format.rs's position.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WalEntry {
    CreateVdisk {
        id: u64,
        size_bytes: u64,
        tier: u8,
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
    SetLease {
        vdisk: u64,
        lease: Lease,
    },
}

impl WalEntry {
    fn encode(&self) -> Vec<u8> {
        match self {
            WalEntry::CreateVdisk {
                id,
                size_bytes,
                tier,
            } => {
                let mut buf = vec![1u8];
                buf.extend_from_slice(&id.to_le_bytes());
                buf.extend_from_slice(&size_bytes.to_le_bytes());
                buf.push(*tier);
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
            WalEntry::SetLease { vdisk, lease } => {
                let mut buf = vec![5u8];
                buf.extend_from_slice(&vdisk.to_le_bytes());
                buf.push(lease.holder);
                buf.push(lease.handing_to.unwrap_or(NO_NODE));
                buf.extend_from_slice(&lease.era.to_le_bytes());
                buf
            }
        }
    }

    fn decode(buf: &[u8]) -> Option<WalEntry> {
        match buf.first()? {
            1 if buf.len() == 18 => Some(WalEntry::CreateVdisk {
                id: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                size_bytes: u64::from_le_bytes(buf[9..17].try_into().unwrap()),
                tier: buf[17],
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
            5 if buf.len() == 19 => Some(WalEntry::SetLease {
                vdisk: u64::from_le_bytes(buf[1..9].try_into().unwrap()),
                lease: Lease {
                    holder: buf[9],
                    handing_to: (buf[10] != NO_NODE).then_some(buf[10]),
                    era: u64::from_le_bytes(buf[11..19].try_into().unwrap()),
                },
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

    /// Open a single-brick pool — the degenerate set, and the shape every
    /// pool before phase 4 had.
    pub fn open(brick: Brick<D>) -> Result<Pool<D>> {
        Self::open_set(BrickSet::single(brick)?)
    }

    /// A freshly formatted set is a pool with no vdisks, same as one brick.
    pub fn create_set(store: BrickSet<D>) -> Result<Pool<D>> {
        Self::open_set(store)
    }

    /// Open a pool over its brick set with a placement seat: `node` is who
    /// this member is, `map` which slices it homes. The placement must be
    /// present *at open*, not set afterwards, because WAL replay's
    /// validity checks run here — a replayed map write naming a block this
    /// member never stores is legitimate exactly when the placement says
    /// so, and without it replay would silently drop the entry and this
    /// member's map trees would diverge from every peer's forever.
    pub fn open_set_placed(store: BrickSet<D>, node: NodeId, map: SliceMap) -> Result<Pool<D>> {
        Self::open_set_inner(store, Some(Placement { node, map }))
    }

    /// Open a pool over its brick set: every brick has already recovered
    /// its extent store; the holder's anchor names the manifest and where
    /// WAL replay begins. No placement: this member holds everything —
    /// the single-node tools and the pre-placement pools.
    pub fn open_set(store: BrickSet<D>) -> Result<Pool<D>> {
        Self::open_set_inner(store, None)
    }

    fn open_set_inner(store: BrickSet<D>, mut placement: Option<Placement>) -> Result<Pool<D>> {
        let anchor = store
            .wal_brick()
            .read_best_anchor()?
            .ok_or(FsError::Corrupt("a formatted brick with no valid anchor"))?;

        let mut vdisks = HashMap::new();
        let mut snapshots = BTreeMap::new();
        let mut leases = BTreeMap::new();
        if anchor.manifest_hash != [0; 32] {
            let manifest_hash = BlockHash::from_bytes(anchor.manifest_hash);
            let manifest = store
                .get(0, &manifest_hash)?
                .ok_or(FsError::Corrupt("the anchored manifest block is missing"))?;
            let decoded = decode_manifest(&manifest)?;
            for (id, state) in decoded.vdisks {
                vdisks.insert(id, state);
            }
            for (key, state) in decoded.snapshots {
                snapshots.insert(key, state);
            }
            for (vdisk, lease) in decoded.leases {
                leases.insert(vdisk, lease);
            }
            // The anchored map is the committed truth; whatever the caller
            // handed in was only a seed for a virgin pool. But a map with
            // no seat is unusable — the manifest is identical on every
            // member, so *which member this is* can only come from above.
            if let Some(map) = decoded.map {
                match placement.as_mut() {
                    Some(placement) => placement.map = map,
                    None => {
                        return Err(FsError::Corrupt(
                            "a placed pool must be opened with its seat",
                        ))
                    }
                }
            }
        }

        let (wal_start, wal_size) = store.wal_brick().wal_bounds();
        let frames = Wal::recover(
            store.wal_brick(),
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
            store,
            wal,
            vdisks,
            snapshots,
            leases,
            anchor_generation: anchor.generation,
            era: anchor.era,
            session_pins: BTreeMap::new(),
            placement,
            pending_map: None,
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

    /// Whether this member's bricks *store* a data block — a home under
    /// the committed map or, mid-reassignment, under the pending one: a
    /// write may land under either, and what arrived under either must
    /// survive a collection until the commit decides. Metadata (map
    /// nodes, manifests) never consults this: it lives on every member.
    fn holds_data(&self, hash: &BlockHash) -> bool {
        match &self.placement {
            Some(placement) => {
                placement.map.holds(placement.node, hash)
                    || self
                        .pending_map
                        .as_ref()
                        .is_some_and(|map| map.holds(placement.node, hash))
            }
            None => true,
        }
    }

    /// Whether this member's bricks are *guaranteed to hold* a data block
    /// — a home under the committed map alone. The read path routes by
    /// this: a pending home mid-move may not have pulled the block yet,
    /// and treating its absence as corruption would page someone for a
    /// move in progress.
    fn serves_data(&self, hash: &BlockHash) -> bool {
        match &self.placement {
            Some(placement) => placement.map.holds(placement.node, hash),
            None => true,
        }
    }

    /// This node's seat and map, if placement has arrived.
    pub fn placement(&self) -> Option<(NodeId, &SliceMap)> {
        self.placement
            .as_ref()
            .map(|placement| (placement.node, &placement.map))
    }

    /// The map a reassignment is moving to, while one is in flight.
    pub fn pending_map(&self) -> Option<&SliceMap> {
        self.pending_map.as_ref()
    }

    /// Where a write's payload belongs right now: the committed homes,
    /// plus — mid-reassignment — the pending ones. Deduplicated, order
    /// preserving the committed map's read preference.
    pub fn write_homes_of(&self, hash: &BlockHash) -> Option<Vec<NodeId>> {
        let placement = self.placement.as_ref()?;
        let mut homes: Vec<NodeId> = placement.map.homes_of(hash).to_vec();
        if let Some(pending) = &self.pending_map {
            for home in pending.homes_of(hash) {
                if !homes.contains(&home) {
                    homes.push(home);
                }
            }
        }
        Some(homes)
    }

    /// Hand the pool its seat after open. Prefer the placed constructors —
    /// a placement set after WAL replay cannot repair what replay already
    /// dropped — but a freshly *created* pool has no WAL to replay, and
    /// the harness builds its members this way.
    pub fn set_placement(&mut self, node: NodeId, map: SliceMap) {
        self.placement = Some(Placement { node, map });
    }

    /// Open a reassignment toward `members` at `version`: the pending map
    /// is computed (never received — the arithmetic chains from the
    /// committed map and every member computes the same answer), storage
    /// widens to the union, and the moves come back for the caller to
    /// run. Idempotent at the same version — re-delivery after a crash
    /// recomputes the identical map. A reassignment that would orphan a
    /// slice — both homes gone at once, RF=2 exhausted — is refused here,
    /// by name, before anything widens.
    pub fn prepare_reassign(
        &mut self,
        version: u64,
        members: &[NodeId],
    ) -> Result<Vec<crate::repl::slice::SliceMove>> {
        let placement = self.placement.as_ref().ok_or(FsError::Placement(
            "an unplaced pool has nothing to reassign",
        ))?;
        if let Some(pending) = &self.pending_map {
            if pending.version() > version {
                return Err(FsError::Placement(
                    "a reassignment is already pending at a newer version",
                ));
            }
        }
        let reassignment = placement.map.reassigned(version, members)?;
        if !reassignment.orphaned.is_empty() {
            return Err(FsError::Placement(
                "the change would orphan slices whose homes are all leaving; refused rather \
                 than papered over with homes that hold nothing",
            ));
        }
        self.pending_map = Some(reassignment.map);
        Ok(reassignment.moves)
    }

    /// The commit: the pending map becomes the committed one, anchored.
    /// The caller confirms every member's moves are complete first — after
    /// this, a collection is free to drop the displaced copies, and a
    /// fetch routes by the new homes.
    pub fn commit_reassign(&mut self) -> Result<()> {
        let pending = self
            .pending_map
            .take()
            .ok_or(FsError::Placement("no reassignment is pending to commit"))?;
        let placement = self
            .placement
            .as_mut()
            .ok_or(FsError::Placement("an unplaced pool has nothing to commit"))?;
        placement.map = pending;
        self.checkpoint()
    }

    /// Adopt a newer committed map whole, from a member that lived
    /// through reassignments this one slept through. The map is not
    /// recomputable — `reassigned` chains from its predecessor — so the
    /// pairs come over the wire and persist here before streaming resumes.
    pub fn adopt_map(&mut self, map: SliceMap) -> Result<()> {
        let placement = self
            .placement
            .as_mut()
            .ok_or(FsError::Placement("an unplaced pool cannot adopt a map"))?;
        if map.version() <= placement.map.version() {
            return Err(FsError::Placement(
                "a map adoption must move the version forward",
            ));
        }
        placement.map = map;
        self.pending_map = None;
        self.checkpoint()
    }

    /// Apply one replayed entry if its references hold. `false` ends replay.
    fn apply_replayed(&mut self, entry: &WalEntry) -> bool {
        match entry {
            WalEntry::CreateVdisk {
                id,
                size_bytes,
                tier,
            } => {
                if self.vdisks.contains_key(id) || *size_bytes == 0 || !self.store.has_tier(*tier) {
                    return false;
                }
                self.vdisks.insert(
                    *id,
                    VdiskState {
                        size_bytes: *size_bytes,
                        tier: *tier,
                        root: None,
                        dirty: BTreeMap::new(),
                    },
                );
                true
            }
            WalEntry::MapWrite { vdisk, index, hash } => {
                let (capacity, tier) = match self.vdisks.get(vdisk) {
                    Some(state) => (self.capacity_of(state), state.tier),
                    None => return false,
                };
                // A home must hold what it maps; a non-home never stored
                // the block and maps it anyway — dropping the entry here
                // is the sleeper that would fork this member's map trees
                // from every peer's.
                if *index >= capacity || (self.holds_data(hash) && !self.store.contains(tier, hash))
                {
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
            WalEntry::DeleteVdisk { id } => {
                self.leases.remove(id);
                self.vdisks.remove(id).is_some()
            }
            WalEntry::SetLease { vdisk, lease } => {
                if !self.vdisks.contains_key(vdisk) {
                    return false;
                }
                self.leases.insert(*vdisk, *lease);
                true
            }
        }
    }

    // -----------------------------------------------------------------
    // Writer leases.

    /// Who may write this vdisk, if anyone yet has.
    pub fn lease(&self, vdisk: u64) -> Option<Lease> {
        self.leases.get(&vdisk).copied()
    }

    /// Every lease, sorted — the view a console would render.
    pub fn leases(&self) -> Vec<(u64, Lease)> {
        self.leases.iter().map(|(id, l)| (*id, *l)).collect()
    }

    /// Whether `node` may write `vdisk` right now.
    ///
    /// A lease granted in an older era does not bind: it was made in a
    /// world that has since been resolved by a fence verdict, and the
    /// survivor that bumped the era is not answerable to it. During a
    /// handover the holder keeps writing — the window is for the
    /// destination to *open* the disk, not to write it — so there is
    /// never an instant with two writers.
    pub fn may_write(&self, vdisk: u64, node: NodeId) -> bool {
        match self.leases.get(&vdisk) {
            Some(lease) if lease.era >= self.era => lease.holder == node,
            // Stale or absent: whoever claims it next may have it.
            _ => false,
        }
    }

    /// Record a lease. Durable at the next flush, replayed like any write
    /// — that is the whole point of it living here rather than in a peer's
    /// memory.
    pub fn set_lease(&mut self, vdisk: u64, lease: Lease) -> Result<()> {
        if !self.vdisks.contains_key(&vdisk) {
            return Err(FsError::UnknownVdisk(vdisk));
        }
        let entry = WalEntry::SetLease { vdisk, lease };
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
        self.leases.insert(vdisk, lease);
        Ok(())
    }

    /// Take the lease for `node`. Legal when the vdisk has no binding
    /// lease — never held, or held under an era this node has moved past
    /// — and refused outright while a peer holds it in the current era,
    /// because that is the case a handover or a fence verdict exists to
    /// resolve.
    pub fn claim_lease(&mut self, vdisk: u64, node: NodeId) -> Result<()> {
        if let Some(lease) = self.leases.get(&vdisk) {
            if lease.era >= self.era && lease.holder != node {
                return Err(FsError::LeaseHeld {
                    vdisk,
                    holder: lease.holder,
                });
            }
        }
        let era = self.era;
        self.set_lease(
            vdisk,
            Lease {
                holder: node,
                handing_to: None,
                era,
            },
        )
    }

    /// Open the migration window: the guest is still running here, and
    /// starting over there. Both may hold the disk open; only the holder
    /// writes.
    pub fn begin_handover(&mut self, vdisk: u64, from: NodeId, to: NodeId) -> Result<()> {
        let lease = self.leases.get(&vdisk).copied();
        match lease {
            Some(lease) if lease.holder == from && lease.era >= self.era => {
                let era = self.era;
                self.set_lease(
                    vdisk,
                    Lease {
                        holder: from,
                        handing_to: Some(to),
                        era,
                    },
                )
            }
            _ => Err(FsError::NotWriter(vdisk)),
        }
    }

    /// The migration completed: the holder **hands the pen over**, in one
    /// durable step, and the window closes with it.
    ///
    /// This runs on the source, and that is the whole point. If the
    /// destination took the lease instead, the source would go on believing
    /// it could write until the change reached it — an interval, however
    /// short, in which two nodes both think they hold the pen. Because only
    /// the holder ever changes the record, no such interval exists: the
    /// source stops being the writer at the instant it says so, and the
    /// destination becomes one only once that has been said.
    pub fn relinquish_lease(&mut self, vdisk: u64, from: NodeId, to: NodeId) -> Result<()> {
        let lease = self.leases.get(&vdisk).copied();
        match lease {
            // A window must already be open toward exactly this
            // destination: handing the pen somewhere nobody asked for it is
            // not a migration, it is a giveaway.
            Some(lease)
                if lease.holder == from
                    && lease.handing_to == Some(to)
                    && lease.era >= self.era =>
            {
                let era = self.era;
                self.set_lease(
                    vdisk,
                    Lease {
                        holder: to,
                        handing_to: None,
                        era,
                    },
                )
            }
            _ => Err(FsError::NoSuchHandover(vdisk)),
        }
    }

    /// The migration failed: the window closes and the disk stays exactly
    /// where it was. Every path out of a migration closes the window —
    /// leaving one open is how two writers eventually happen.
    pub fn abort_handover(&mut self, vdisk: u64, from: NodeId) -> Result<()> {
        let lease = self.leases.get(&vdisk).copied();
        match lease {
            Some(lease) if lease.holder == from && lease.handing_to.is_some() => {
                let era = lease.era;
                self.set_lease(
                    vdisk,
                    Lease {
                        holder: from,
                        handing_to: None,
                        era,
                    },
                )
            }
            _ => Err(FsError::NoSuchHandover(vdisk)),
        }
    }

    fn capacity_for(&self, size_bytes: u64) -> u64 {
        size_bytes.div_ceil(self.store.block_size() as u64)
    }

    fn depth_for_size(&self, size_bytes: u64) -> u32 {
        map::depth_for(
            self.capacity_for(size_bytes),
            map::entries_per_node(self.store.block_size()),
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
        // Every vdisk may carry a lease, so the ceiling counts one each
        // whether or not they exist yet — discovering the manifest is full
        // at the moment a migration needs a lease would be a poor time.
        // A placed pool's manifest also carries the map, always.
        let map_len = if self.placement.is_some() {
            MANIFEST_MAP_LEN
        } else {
            0
        };
        MANIFEST_HEADER_LEN
            + map_len
            + vdisk_count * (MANIFEST_ENTRY_LEN + MANIFEST_LEASE_LEN)
            + snapshot_count * MANIFEST_SNAPSHOT_LEN
            <= self.store.block_size() as usize
    }

    /// Create a vdisk on a tier. Durable at the next flush, like any
    /// write. A tier this node's set does not carry is refused — there is
    /// no spill, because a capacity figure that lied about where bytes
    /// land would be worse than a refusal.
    pub fn create_vdisk(&mut self, id: u64, size_bytes: u64, tier: u8) -> Result<()> {
        if self.vdisks.contains_key(&id) {
            return Err(FsError::VdiskExists(id));
        }
        if size_bytes == 0 {
            return Err(FsError::BadGeometry("a vdisk must hold at least one block"));
        }
        if !self.store.has_tier(tier) {
            return Err(FsError::NoSuchTier(tier));
        }
        if !self.manifest_fits(self.vdisks.len() + 1, self.snapshots.len()) {
            return Err(FsError::ManifestFull);
        }
        let entry = WalEntry::CreateVdisk {
            id,
            size_bytes,
            tier,
        };
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
        self.vdisks.insert(
            id,
            VdiskState {
                size_bytes,
                tier,
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
        let tier = state.tier;
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let hash = self.store.put(tier, payload)?;
        let entry = WalEntry::MapWrite { vdisk, index, hash };
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
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
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
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
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
        self.vdisks.remove(&id);
        self.leases.remove(&id);
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
        // The clone shares every block with its source, and a block's tier
        // is part of its home — so the clone lives on the source's tier by
        // construction, not by choice.
        let tier = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?
            .tier;
        self.vdisks.insert(
            new_id,
            VdiskState {
                size_bytes: snap.size_bytes,
                tier,
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
        // The snapshot's data lives on its vdisk's tier; the vdisk always
        // exists, because deleting one with snapshots is refused.
        let tier = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::Corrupt("a snapshot outlives its vdisk"))?
            .tier;
        let capacity = self.capacity_for(snap.size_bytes);
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        let hash = match &snap.root {
            Some(root) => map::lookup(
                &self.store.tier_ref(0),
                root,
                self.depth_for_size(snap.size_bytes),
                index,
            )?,
            None => None,
        };
        match hash {
            Some(hash) => match self.store.get(tier, &hash)? {
                Some(payload) => Ok(Some(payload)),
                None if !self.serves_data(&hash) => Err(FsError::BlockElsewhere { tier, hash }),
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
                Some(root) => {
                    map::lookup(&self.store.tier_ref(0), root, self.depth_of(state), index)?
                }
                None => None,
            },
        };
        match hash {
            Some(hash) => match self.store.get(state.tier, &hash)? {
                Some(payload) => Ok(Some(payload)),
                // Absent on a non-home is the ordinary answer — the bytes
                // live at the named address on the slice's homes, and the
                // caller's next move is a fetch. Absent on a home is
                // corruption to repair, never a quiet zero-fill.
                None if !self.serves_data(&hash) => Err(FsError::BlockElsewhere {
                    tier: state.tier,
                    hash,
                }),
                None => Err(FsError::Corrupt("a mapped block is missing from the store")),
            },
            None => Ok(None),
        }
    }

    /// The acknowledgement barrier: every write and create before this call
    /// is durable when it returns, on every brick it touched.
    pub fn flush(&mut self) -> Result<()> {
        self.store.flush()
    }

    /// Fold every dirty map into its tree, anchor the result, and retire
    /// the WAL's history. Two flushes; see the module header for why the
    /// order cannot lie — the first now spans every dirty brick, and
    /// nothing is anchored until all of them have answered.
    /// The buffered half of a checkpoint: fold every dirty map into its
    /// tree and write the manifest — everything that must see a
    /// consistent pool, none of it waiting on a platter.
    fn fold_and_manifest(&mut self) -> Result<[u8; 32]> {
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
            // Tree nodes are tier-0 blocks whatever tier the data went to:
            // the map working set belongs on the fastest class.
            let root = map::fold(
                &mut self.store.tier_mut(0),
                previous.as_ref(),
                depth,
                &dirty,
            )?;
            self.vdisks.get_mut(id).unwrap().root = root;
        }

        // A placed pool always writes its manifest — the map inside is
        // durable state even when no vdisk exists yet.
        if self.vdisks.is_empty() && self.snapshots.is_empty() && self.placement.is_none() {
            Ok([0u8; 32])
        } else {
            let manifest = encode_manifest(
                &ids,
                &self.vdisks,
                &self.snapshots,
                &self.leases,
                self.placement.as_ref().map(|placement| &placement.map),
            );
            Ok(*self.store.put(0, &manifest)?.as_bytes())
        }
    }

    pub fn checkpoint(&mut self) -> Result<()> {
        let manifest_hash = self.fold_and_manifest()?;
        self.store.flush()?;

        self.wal.retire_to_cursor();
        let (wal_replay_offset, wal_replay_seq) = self.wal.position();
        self.anchor_generation += 1;
        let roster = self.store.roster();
        self.store.wal_brick_mut().write_anchor(&Anchor {
            generation: self.anchor_generation,
            wal_replay_offset,
            wal_replay_seq,
            wal_epoch: self.wal.epoch(),
            era: self.era,
            manifest_hash,
            roster,
        })?;
        self.store.flush()
    }

    /// Open a two-phase checkpoint: the buffered half runs now, under
    /// whatever lock owns this pool, and the ticket carries everything
    /// the drain and the commit need. The caller runs every flusher —
    /// off the lock, that being the point — then offers the ticket to
    /// [`Pool::checkpoint_commit`]. `None` means a brick could not give
    /// a detached barrier (the simulator); take the single-phase
    /// [`Pool::checkpoint`] instead — the folds already done here make
    /// it no more expensive.
    ///
    /// An abandoned ticket costs nothing but the work: the folds are
    /// exactly what a checkpoint does anyway, no anchor was written, and
    /// the un-retired WAL still replays everything the old anchor lacks.
    pub fn checkpoint_begin(&mut self) -> Result<Option<CheckpointTicket>> {
        let manifest_hash = self.fold_and_manifest()?;
        // Handles come AFTER the folds: the tree and manifest writes
        // above are precisely what phase B's drain must cover.
        let Some(flushers) = self.store.flush_handles() else {
            return Ok(None);
        };
        let (cursor, seq) = self.wal.position();
        Ok(Some(CheckpointTicket {
            manifest_hash,
            cursor,
            seq,
            epoch: self.wal.epoch(),
            era: self.era,
            generation: self.anchor_generation,
            flushers,
        }))
    }

    /// Close a two-phase checkpoint. The anchor written here names the
    /// ticket's snapshot — manifest and WAL cursor as of `begin` — which
    /// is durable only because the caller drained the ticket's flushers
    /// first; the trailing in-lock flush covers the anchor itself and
    /// whatever landed since the drain, and is cheap for exactly that
    /// reason. Returns `false` without writing when the world moved —
    /// an era, epoch, or another checkpoint crossed between the phases —
    /// because an anchor naming a stale snapshot of a changed history is
    /// how acknowledged writes get forgotten.
    pub fn checkpoint_commit(&mut self, ticket: CheckpointTicket) -> Result<bool> {
        if ticket.era != self.era
            || ticket.epoch != self.wal.epoch()
            || ticket.generation != self.anchor_generation
        {
            return Ok(false);
        }
        self.wal.retire_to(ticket.cursor);
        self.anchor_generation += 1;
        let roster = self.store.roster();
        self.store.wal_brick_mut().write_anchor(&Anchor {
            generation: self.anchor_generation,
            wal_replay_offset: ticket.cursor,
            wal_replay_seq: ticket.seq,
            wal_epoch: ticket.epoch,
            era: self.era,
            manifest_hash: ticket.manifest_hash,
            roster,
        })?;
        self.store.flush()?;
        Ok(true)
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
        // records ahead of releasing their segments. So it spends every
        // brick's reserve, which exists for exactly this and is closed
        // again however this ends.
        self.store.open_reserve();
        let outcome = self.collect_within_reserve();
        self.store.close_reserve();
        outcome
    }

    fn collect_within_reserve(&mut self) -> Result<GcStats> {
        self.checkpoint()?;

        let mut ids: Vec<u64> = self.vdisks.keys().copied().collect();
        ids.sort_unstable();
        // Liveness is per tier — the same hash on two tiers is two blocks,
        // and marking one must not keep the other.
        let mut live: HashSet<(u8, BlockHash)> = HashSet::new();
        if !(self.vdisks.is_empty() && self.snapshots.is_empty() && self.placement.is_none()) {
            // The same bytes the checkpoint just anchored, so the same hash
            // — recomputed rather than remembered, one source of truth.
            live.insert((
                0,
                hash_block(&encode_manifest(
                    &ids,
                    &self.vdisks,
                    &self.snapshots,
                    &self.leases,
                    self.placement.as_ref().map(|placement| &placement.map),
                )),
            ));
        }
        let mut roots: Vec<(Option<BlockHash>, u32, u8)> = ids
            .iter()
            .map(|id| {
                let state = &self.vdisks[id];
                (state.root, self.depth_of(state), state.tier)
            })
            .collect();
        // Pinned history is exactly as live as the present, on the tier of
        // the vdisk it pins.
        roots.extend(self.snapshots.iter().map(|((vdisk, _), snap)| {
            (
                snap.root,
                self.depth_for_size(snap.size_bytes),
                self.vdisks[vdisk].tier,
            )
        }));
        let mut walked: Vec<(u8, BlockHash)> = Vec::new();
        for (root, depth, data_tier) in roots {
            if let Some(root) = root {
                map::walk(
                    &self.store.tier_ref(0),
                    &root,
                    depth,
                    &mut |item| match item {
                        map::MapItem::Node(hash) => {
                            live.insert((0, hash));
                        }
                        map::MapItem::Block { hash, .. } => {
                            walked.push((data_tier, hash));
                        }
                    },
                )?;
            }
        }
        // A referenced data block is live *here* only while this member
        // homes it — under the committed map or, mid-reassignment, the
        // pending one. This filter is the displacement drop, and its gate
        // is the commit: metadata references every block from every
        // member forever, so without it a member reassigned away from a
        // slice would carry that slice's copies to the end of time. It is
        // also why the drop cannot run early — until the commit, a
        // pending home may still be pulling from the copy this filter
        // would have swept.
        for (data_tier, hash) in walked {
            if self.holds_data(&hash) {
                live.insert((data_tier, hash));
            }
        }
        // A resync in flight holds state neither manifest references yet.
        self.mark_pinned(&mut live)?;
        self.store.retain_and_reclaim(&live)
    }

    /// Verify everything: every stored block against its content address,
    /// and every map reference against the store. Repair is phase 2's
    /// business; a truthful report is this one's. A missing *map node* is
    /// an error rather than a report line — with the tree's shape gone,
    /// there is no honest way to say which blocks are missing. A data
    /// block this member does not home is not missing — it is at its
    /// homes, and calling it missing here would page someone for the
    /// design working as designed.
    pub fn scrub(&self) -> Result<ScrubReport> {
        let (blocks_verified, corrupt) = self.store.scrub()?;
        self.scrub_references(blocks_verified, corrupt)
    }

    /// One bounded slice of the store pass — the expensive half of a
    /// scrub, every record read back and verified against its address.
    /// The caller drives the cursor and owns the pacing; taking the pool
    /// between slices is exactly the point, so a scrub shares the engine
    /// with the guests instead of standing in front of them.
    pub fn scrub_chunk(
        &self,
        cursor: Option<crate::store::brick_set::ScrubCursor>,
        budget: usize,
    ) -> Result<(
        u64,
        Vec<BlockHash>,
        Option<crate::store::brick_set::ScrubCursor>,
    )> {
        self.store.scrub_chunk(cursor, budget)
    }

    /// How many records the store pass covers right now — the denominator
    /// a progress bar is honest against. Churn moves it; the bar is a
    /// report, not a promise.
    pub fn scrub_total(&self) -> u64 {
        self.store.scrub_total()
    }

    /// The reference half of a scrub: every dirty entry, tree reference
    /// and snapshot reference checked against the store — the walk
    /// [`Pool::scrub`] runs after its store pass, callable on its own so
    /// a chunked scrub can finish the same way. `verified` and `corrupt`
    /// are the store pass's tallies, carried into the report unchanged.
    pub fn scrub_references(
        &self,
        blocks_verified: u64,
        corrupt: Vec<BlockHash>,
    ) -> Result<ScrubReport> {
        let mut missing = Vec::new();
        let mut ids: Vec<u64> = self.vdisks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let state = &self.vdisks[&id];
            for (index, value) in &state.dirty {
                if let Some(hash) = value {
                    if self.serves_data(hash) && !self.store.contains(state.tier, hash) {
                        missing.push((id, *index));
                    }
                }
            }
            if let Some(root) = &state.root {
                let mut tree_refs = Vec::new();
                map::walk(
                    &self.store.tier_ref(0),
                    root,
                    self.depth_of(state),
                    &mut |item| {
                        if let map::MapItem::Block { index, hash } = item {
                            tree_refs.push((index, hash));
                        }
                    },
                )?;
                for (index, hash) in tree_refs {
                    if !state.dirty.contains_key(&index)
                        && self.serves_data(&hash)
                        && !self.store.contains(state.tier, &hash)
                    {
                        missing.push((id, index));
                    }
                }
            }
        }
        for ((vdisk, _), snap) in &self.snapshots {
            let tier = self.vdisks[vdisk].tier;
            if let Some(root) = &snap.root {
                let mut tree_refs = Vec::new();
                map::walk(
                    &self.store.tier_ref(0),
                    root,
                    self.depth_for_size(snap.size_bytes),
                    &mut |item| {
                        if let map::MapItem::Block { index, hash } = item {
                            tree_refs.push((index, hash));
                        }
                    },
                )?;
                for (index, hash) in tree_refs {
                    if self.serves_data(&hash) && !self.store.contains(tier, &hash) {
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
        self.store.block_size()
    }

    /// The pool identity these bricks were formatted into.
    pub fn pool_uuid(&self) -> [u8; 16] {
        self.store.pool_uuid()
    }

    /// The tiers this node's set carries, ascending.
    pub fn tiers(&self) -> Vec<u8> {
        self.store.tiers()
    }

    /// The store's space in bytes, per tier and per brick — what the
    /// capacity verbs report and the one usable figure is computed from.
    pub fn space_report(&self) -> crate::store::brick_set::SpaceReport {
        self.store.space_report()
    }

    /// How the store's space stands, summed across bricks. A caller that
    /// only ever learns about pressure from [`FsError::Full`] learns too
    /// late: by then every write triggers a collection, and the pool
    /// spends its time collecting rather than storing. This is how policy
    /// sees the cliff coming.
    pub fn space(&self) -> BrickStats {
        self.store.stats()
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

    /// This node is continuing without a member, on a fence verdict the
    /// caller holds: the state that follows belongs to a new generation,
    /// and the fact is anchored before any of that state exists.
    ///
    /// The caller supplies the number rather than this pool computing one,
    /// because the number must be *agreed*: two survivors each computing
    /// `max + 1` from their own vantage can mint different eras for the
    /// same verdict, and the next hello would read the difference as a
    /// second verdict that never happened. The layer that issues verdicts
    /// takes every survivor's floor into one target; this pool only
    /// insists the target actually moves it forward — an era that does
    /// not rise orders nothing.
    pub fn bump_era_to(&mut self, target: u64) -> Result<()> {
        if target <= self.era {
            return Err(FsError::Corrupt(
                "an era bump must clear the era it replaces",
            ));
        }
        self.era = target;
        self.checkpoint()
    }

    /// Hold these roots live through collections for one peer's session,
    /// until [`Pool::unpin_sync`] for that peer. A resync needs this on
    /// both ends: the source keeps writing past the state it offered, so
    /// its next checkpoint orphans the offer's trees while the target is
    /// still fetching them; the target holds a growing top-connected
    /// fragment of the offer that nothing in its own manifest references
    /// yet. Pins are per session because sessions overlap: a node can
    /// source one peer's pull while pulling from another, and neither may
    /// clobber the other's claims.
    pub fn pin_sync(&mut self, peer: NodeId, pins: Vec<(BlockHash, u32, u8)>) {
        self.session_pins.entry(peer).or_default().offer = pins;
    }

    /// This peer's resync is over — however it ended, its offer pins come
    /// off. What they held either got adopted (reachable again) or is
    /// garbage (correctly).
    pub fn unpin_sync(&mut self, peer: NodeId) {
        if let Some(pins) = self.session_pins.get_mut(&peer) {
            pins.offer.clear();
        }
    }

    /// A payload arrived on this peer's session ahead of the op that will
    /// reference it. Until that op lands, nothing durable reaches the
    /// block — a collection in the gap would sweep it, and the op's replay
    /// would then name a block the store does not hold.
    pub fn pin_inflight(&mut self, peer: NodeId, tier: u8, hash: BlockHash) {
        self.session_pins
            .entry(peer)
            .or_default()
            .inflight
            .insert((tier, hash));
    }

    /// The op referencing this payload landed; the block is reachable from
    /// durable state and no longer needs its arrival pin.
    pub fn clear_inflight(&mut self, peer: NodeId, tier: u8, hash: &BlockHash) {
        if let Some(pins) = self.session_pins.get_mut(&peer) {
            pins.inflight.remove(&(tier, *hash));
        }
    }

    /// The peer's session died: every claim it held comes off. Payloads
    /// whose ops will now never arrive become garbage, correctly — the
    /// next session's offer re-ships anything that still matters.
    pub fn drop_session_pins(&mut self, peer: NodeId) {
        self.session_pins.remove(&peer);
    }

    /// Mark everything reachable from every session's pins *through blocks
    /// the store holds*. Skipping an absent subtree is correct on both
    /// ends: on the source everything pinned is present, and on the target
    /// the pull arrives strictly top-down — a held block's parents are
    /// held — so every fetched block sits on a present path from a pinned
    /// root. In-flight payloads are leaves by definition and marked
    /// directly.
    fn mark_pinned(&self, live: &mut HashSet<(u8, BlockHash)>) -> Result<()> {
        let mut pending: Vec<(BlockHash, u32, u8)> = Vec::new();
        for pins in self.session_pins.values() {
            pending.extend(pins.offer.iter().copied());
            for (tier, hash) in &pins.inflight {
                if self.store.contains(*tier, hash) {
                    live.insert((*tier, *hash));
                }
            }
        }
        let mut seen: HashSet<(BlockHash, u32, u8)> = HashSet::new();
        while let Some((hash, kind, data_tier)) = pending.pop() {
            // A node lives on tier 0; a kind-0 entry is data on its tier.
            let store_tier = if kind > 0 { 0 } else { data_tier };
            if !seen.insert((hash, kind, data_tier)) || !self.store.contains(store_tier, &hash) {
                continue;
            }
            live.insert((store_tier, hash));
            if kind > 0 {
                let payload = self
                    .store
                    .get(0, &hash)?
                    .ok_or(FsError::Corrupt("a pinned map node vanished mid-mark"))?;
                for child in map::children(&payload) {
                    pending.push((child, kind - 1, data_tier));
                }
            }
        }
        Ok(())
    }

    /// Store a payload at a tier without mapping it anywhere — a
    /// replicated block arriving ahead of the operation that references
    /// it, on the tier that operation will reference it at. Hashes the
    /// payload: what arrives off the wire is verified by addressing.
    pub fn put_block(&mut self, tier: u8, payload: &[u8]) -> Result<BlockHash> {
        self.store.put(tier, payload)
    }

    /// The same store, for a caller that already computed the address —
    /// the local write path, which hashed the block to place it.
    pub fn put_block_prehashed(&mut self, tier: u8, hash: BlockHash, payload: &[u8]) -> Result<()> {
        self.store.put_prehashed(tier, hash, payload)
    }

    /// The batched form — a run of prehashed blocks in as few disk
    /// writes as they have segment runs. See
    /// [`crate::BrickSet::put_prehashed_batch`].
    pub fn put_blocks_prehashed(&mut self, tier: u8, items: &[(BlockHash, &[u8])]) -> Result<()> {
        self.store.put_prehashed_batch(tier, items)
    }

    /// Whether the store holds a block, by tier and address.
    pub fn has_block(&self, tier: u8, hash: &BlockHash) -> bool {
        self.store.contains(tier, hash)
    }

    /// A stored block's payload, by tier and address — what a resync
    /// source serves.
    pub fn block_payload(&self, tier: u8, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        self.store.get(tier, hash)
    }

    /// A map write whose payload is already in the store — the replicated
    /// form of [`Pool::write_block`]. Refusing an absent block is what
    /// keeps a reordered or truncated stream from mapping a promise the
    /// store cannot keep — on a *home*. A non-home maps the block without
    /// holding it: the map entry is metadata, and metadata lives here
    /// even when the bytes do not.
    pub fn write_block_prehashed(&mut self, vdisk: u64, index: u64, hash: BlockHash) -> Result<()> {
        let state = self
            .vdisks
            .get(&vdisk)
            .ok_or(FsError::UnknownVdisk(vdisk))?;
        let capacity = self.capacity_of(state);
        let tier = state.tier;
        if index >= capacity {
            return Err(FsError::OutOfRange { index, capacity });
        }
        if self.holds_data(&hash) && !self.store.contains(tier, &hash) {
            return Err(FsError::Corrupt(
                "a replicated write names a block the store does not hold",
            ));
        }
        let entry = WalEntry::MapWrite { vdisk, index, hash };
        self.wal
            .append(self.store.wal_brick_mut(), &entry.encode())?;
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
            .map(|(id, state)| (*id, state.size_bytes, state.tier, state.root))
            .collect();
        vdisks.sort_unstable_by_key(|(id, _, _, _)| *id);
        let snapshots = self
            .snapshots
            .iter()
            .map(|((vdisk, snapshot), state)| (*vdisk, *snapshot, state.size_bytes, state.root))
            .collect();
        (self.era, vdisks, snapshots)
    }

    /// Become the offered state: replace what the offer names, adopt its
    /// era, and anchor it all. The blocks under every root must already be
    /// in the store — the resync's tree walk is what put them there — and
    /// whatever the adoption displaces becomes garbage for the next
    /// collection. This is how a stale node stops being stale, and the
    /// discard of its divergent unacknowledged history is the point, not a
    /// side effect.
    ///
    /// [`AdoptScope::Whole`] discards every vdisk, snapshot, and lease the
    /// offer does not name — one source speaks for the whole pool, the
    /// two-node resync. [`AdoptScope::Named`] replaces only what the offer
    /// carries and leaves the rest standing, which is the shape a
    /// three-member rejoin needs: each vdisk adopted from its own lease
    /// holder's offer, because no single peer can speak for ops another
    /// writer originated.
    pub fn adopt_sync(
        &mut self,
        era: u64,
        vdisks: &[VdiskOffer],
        snapshots: &[SnapshotOffer],
        leases: &[(u64, Lease)],
        scope: AdoptScope,
    ) -> Result<()> {
        for (_, _, tier, root) in vdisks {
            // A vdisk on a tier this set lacks has nowhere for its data —
            // the asymmetric-brick-set deployment error, refused by name
            // before any state is replaced.
            if !self.store.has_tier(*tier) {
                return Err(FsError::NoSuchTier(*tier));
            }
            if let Some(root) = root {
                if !self.store.contains(0, root) {
                    return Err(FsError::Corrupt("adopting a root the store does not hold"));
                }
            }
        }
        for (_, _, _, root) in snapshots {
            if let Some(root) = root {
                if !self.store.contains(0, root) {
                    return Err(FsError::Corrupt("adopting a root the store does not hold"));
                }
            }
        }
        if scope == AdoptScope::Whole {
            self.vdisks.clear();
            self.snapshots.clear();
            self.leases.clear();
        } else {
            // Named: the offer's vdisks are replaced outright, which
            // includes every snapshot pinned under them — a snapshot's
            // truth comes from its vdisk's holder along with the vdisk.
            for (id, _, _, _) in vdisks {
                self.vdisks.remove(id);
                self.snapshots.retain(|(vdisk, _), _| vdisk != id);
                self.leases.remove(id);
            }
        }
        for (id, size_bytes, tier, root) in vdisks {
            self.vdisks.insert(
                *id,
                VdiskState {
                    size_bytes: *size_bytes,
                    tier: *tier,
                    root: *root,
                    dirty: BTreeMap::new(),
                },
            );
        }
        for (vdisk, snapshot, size_bytes, root) in snapshots {
            self.snapshots.insert(
                (*vdisk, *snapshot),
                SnapshotState {
                    size_bytes: *size_bytes,
                    root: *root,
                },
            );
        }
        // Who may write is part of the state being adopted, not a separate
        // negotiation: a node that has just taken someone else's history
        // has no standing to keep its own opinion about who holds the pen.
        for (vdisk, lease) in leases {
            self.leases.insert(*vdisk, *lease);
        }
        self.era = era;
        self.checkpoint()
    }

    /// Keep exactly these vdisks; drop the rest with their snapshots and
    /// leases, and anchor the result. The rejoin cleanup: per-vdisk
    /// adoption never removes anything, so a vdisk deleted while this
    /// member was away survives every offer — until the union of what the
    /// live members still carry says it should not.
    pub fn retain_vdisks(&mut self, keep: &HashSet<u64>) -> Result<()> {
        let before = self.vdisks.len();
        self.vdisks.retain(|id, _| keep.contains(id));
        self.snapshots.retain(|(vdisk, _), _| keep.contains(vdisk));
        self.leases.retain(|vdisk, _| keep.contains(vdisk));
        if self.vdisks.len() != before {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// One vdisk's size in bytes.
    pub fn vdisk_size(&self, id: u64) -> Result<u64> {
        self.vdisks
            .get(&id)
            .map(|state| state.size_bytes)
            .ok_or(FsError::UnknownVdisk(id))
    }

    /// The tier one vdisk's data lives on.
    pub fn vdisk_tier(&self, id: u64) -> Result<u8> {
        self.vdisks
            .get(&id)
            .map(|state| state.tier)
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

    /// Take a single-brick pool apart — the crash-simulation reopen path.
    /// Panics on a multi-brick set; those go through [`Pool::into_bricks`].
    pub fn into_brick(self) -> Brick<D> {
        let mut bricks = self.store.into_bricks();
        assert_eq!(bricks.len(), 1, "into_brick on a multi-brick pool");
        bricks.pop().unwrap()
    }

    /// Take the whole set apart.
    pub fn into_bricks(self) -> Vec<Brick<D>> {
        self.store.into_bricks()
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
    leases: &BTreeMap<u64, Lease>,
    map: Option<&SliceMap>,
) -> Vec<u8> {
    let map_len = if map.is_some() { MANIFEST_MAP_LEN } else { 0 };
    let version = if map.is_some() {
        MANIFEST_VERSION_PLACED
    } else {
        MANIFEST_VERSION
    };
    let mut buf = vec![
        0u8;
        MANIFEST_HEADER_LEN
            + ids.len() * MANIFEST_ENTRY_LEN
            + snapshots.len() * MANIFEST_SNAPSHOT_LEN
            + leases.len() * MANIFEST_LEASE_LEN
            + map_len
    ];
    buf[0..8].copy_from_slice(MANIFEST_MAGIC);
    buf[8..12].copy_from_slice(&version.to_le_bytes());
    buf[12..16].copy_from_slice(&(ids.len() as u32).to_le_bytes());
    buf[16..20].copy_from_slice(&(snapshots.len() as u32).to_le_bytes());
    buf[20..24].copy_from_slice(&(leases.len() as u32).to_le_bytes());
    let mut at = MANIFEST_HEADER_LEN;
    for id in ids {
        let state = &vdisks[id];
        buf[at..at + 8].copy_from_slice(&id.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&state.size_bytes.to_le_bytes());
        buf[at + 16] = state.tier;
        // bytes 17..24 stay zero — padding to keep the root aligned
        buf[at + 24..at + 56].copy_from_slice(&root_bytes(&state.root));
        at += MANIFEST_ENTRY_LEN;
    }
    for ((vdisk, snapshot), state) in snapshots {
        buf[at..at + 8].copy_from_slice(&vdisk.to_le_bytes());
        buf[at + 8..at + 16].copy_from_slice(&snapshot.to_le_bytes());
        buf[at + 16..at + 24].copy_from_slice(&state.size_bytes.to_le_bytes());
        buf[at + 24..at + 56].copy_from_slice(&root_bytes(&state.root));
        at += MANIFEST_SNAPSHOT_LEN;
    }
    for (vdisk, lease) in leases {
        buf[at..at + 8].copy_from_slice(&vdisk.to_le_bytes());
        buf[at + 8] = lease.holder;
        buf[at + 9] = lease.handing_to.unwrap_or(NO_NODE);
        buf[at + 16..at + 24].copy_from_slice(&lease.era.to_le_bytes());
        at += MANIFEST_LEASE_LEN;
    }
    if let Some(map) = map {
        buf[at..at + 8].copy_from_slice(&map.version().to_le_bytes());
        at += 8;
        for homes in map.to_pairs() {
            buf[at] = homes[0];
            buf[at + 1] = homes[1];
            at += 2;
        }
    }
    buf
}

struct DecodedManifest {
    vdisks: Vec<(u64, VdiskState)>,
    snapshots: Vec<((u64, u64), SnapshotState)>,
    leases: Vec<(u64, Lease)>,
    map: Option<SliceMap>,
}

fn decode_manifest(buf: &[u8]) -> Result<DecodedManifest> {
    if buf.len() < MANIFEST_HEADER_LEN || &buf[0..8] != MANIFEST_MAGIC {
        return Err(FsError::Corrupt("the manifest block has the wrong shape"));
    }
    let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    if version != MANIFEST_VERSION && version != MANIFEST_VERSION_PLACED {
        return Err(FsError::Corrupt(
            "the manifest block is a version this build does not speak",
        ));
    }
    let map_len = if version == MANIFEST_VERSION_PLACED {
        MANIFEST_MAP_LEN
    } else {
        0
    };
    let vdisk_count = u32::from_le_bytes(buf[12..16].try_into().unwrap()) as usize;
    let snapshot_count = u32::from_le_bytes(buf[16..20].try_into().unwrap()) as usize;
    let lease_count = u32::from_le_bytes(buf[20..24].try_into().unwrap()) as usize;
    if buf.len()
        < MANIFEST_HEADER_LEN
            + vdisk_count * MANIFEST_ENTRY_LEN
            + snapshot_count * MANIFEST_SNAPSHOT_LEN
            + lease_count * MANIFEST_LEASE_LEN
            + map_len
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
        let tier = buf[at + 16];
        let root = root_from_bytes(buf[at + 24..at + 56].try_into().unwrap());
        vdisks.push((
            id,
            VdiskState {
                size_bytes,
                tier,
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
    let mut leases = Vec::with_capacity(lease_count);
    for _ in 0..lease_count {
        let vdisk = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        let handing = buf[at + 9];
        leases.push((
            vdisk,
            Lease {
                holder: buf[at + 8],
                handing_to: (handing != NO_NODE).then_some(handing),
                era: u64::from_le_bytes(buf[at + 16..at + 24].try_into().unwrap()),
            },
        ));
        at += MANIFEST_LEASE_LEN;
    }
    let map = if version == MANIFEST_VERSION_PLACED {
        let map_version = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
        at += 8;
        let mut pairs = Vec::with_capacity(crate::repl::slice::SLICES);
        for _ in 0..crate::repl::slice::SLICES {
            pairs.push([buf[at], buf[at + 1]]);
            at += 2;
        }
        Some(SliceMap::from_pairs(map_version, pairs)?)
    } else {
        None
    };
    Ok(DecodedManifest {
        vdisks,
        snapshots,
        leases,
        map,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::sim::SimDisk;
    use crate::store::brick::BrickParams;

    const KIB: u64 = 1024;
    const BLOCK: usize = 4 * KIB as usize;

    fn params() -> BrickParams {
        BrickParams {
            pool_uuid: [0xAA; 16],
            brick_uuid: [0xBB; 16],
            block_size: BLOCK as u32,
            segment_size: 128 * KIB,
            wal_size: 32 * KIB,
            tier: 0,
            wal_holder: true,
        }
    }

    fn pool(seed: u64) -> Pool<SimDisk> {
        Pool::create(Brick::format(SimDisk::new(8 * KIB * KIB, seed), params()).unwrap()).unwrap()
    }

    fn reopen(pool: Pool<SimDisk>) -> Pool<SimDisk> {
        Pool::open(Brick::open(pool.into_brick().into_disk()).unwrap()).unwrap()
    }

    /// A real file, because the two-phase checkpoint only exists where a
    /// disk can offer a detached barrier — the simulator cannot, by
    /// design, and takes the single-phase path.
    struct FileScratch(std::path::PathBuf);
    impl Drop for FileScratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn file_pool(name: &str) -> (Pool<crate::FileDisk>, FileScratch) {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lumen-fs-twophase-{}-{name}.brick",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(8 * KIB * KIB).unwrap();
        drop(file);
        let disk = crate::FileDisk::open(&path).unwrap();
        let pool = Pool::create(Brick::format(disk, params()).unwrap()).unwrap();
        (pool, FileScratch(path))
    }

    fn drain(ticket: &mut CheckpointTicket) {
        for flush in std::mem::take(&mut ticket.flushers) {
            flush().unwrap();
        }
    }

    #[test]
    fn a_two_phase_checkpoint_lands_like_a_single_phase_one() {
        let (mut pool, _scratch) = file_pool("lands");
        pool.create_vdisk(7, 40 * BLOCK as u64, 0).unwrap();
        pool.write_block(7, 3, b"two-phase payload").unwrap();
        let mut ticket = pool
            .checkpoint_begin()
            .unwrap()
            .expect("files offer barriers");
        drain(&mut ticket);
        assert!(pool.checkpoint_commit(ticket).unwrap());
        let pool = reopen_file(pool);
        assert_eq!(
            pool.read_block(7, 3).unwrap().unwrap(),
            b"two-phase payload"
        );
    }

    #[test]
    fn a_ticket_crossed_by_another_checkpoint_is_refused() {
        let (mut pool, _scratch) = file_pool("stale");
        pool.create_vdisk(7, 40 * BLOCK as u64, 0).unwrap();
        pool.write_block(7, 0, b"first").unwrap();
        let mut ticket = pool
            .checkpoint_begin()
            .unwrap()
            .expect("files offer barriers");
        // Another checkpoint lands between the phases — the ticket's
        // snapshot no longer describes the anchored history.
        pool.checkpoint().unwrap();
        drain(&mut ticket);
        assert!(!pool.checkpoint_commit(ticket).unwrap());
        // Refused, not corrupted: the pool still reopens on the
        // crossing checkpoint's anchor.
        let pool = reopen_file(pool);
        assert_eq!(pool.read_block(7, 0).unwrap().unwrap(), b"first");
    }

    #[test]
    fn a_dropped_ticket_loses_nothing() {
        let (mut pool, _scratch) = file_pool("dropped");
        pool.create_vdisk(7, 40 * BLOCK as u64, 0).unwrap();
        pool.write_block(7, 1, b"before the begin").unwrap();
        let ticket = pool
            .checkpoint_begin()
            .unwrap()
            .expect("files offer barriers");
        // A failed drain drops the ticket on the floor: no anchor moved,
        // no WAL retired — the old anchor plus replay still owns it all.
        drop(ticket);
        pool.write_block(7, 2, b"after the drop").unwrap();
        pool.flush().unwrap();
        let pool = reopen_file(pool);
        assert_eq!(pool.read_block(7, 1).unwrap().unwrap(), b"before the begin");
        assert_eq!(pool.read_block(7, 2).unwrap().unwrap(), b"after the drop");
    }

    fn reopen_file(pool: Pool<crate::FileDisk>) -> Pool<crate::FileDisk> {
        Pool::open(Brick::open(pool.into_brick().into_disk()).unwrap()).unwrap()
    }

    #[test]
    fn a_write_survives_reopen_through_the_wal_alone() {
        let mut pool = pool(1);
        pool.create_vdisk(7, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.write_block(1, 9, b"through the tree").unwrap();
        pool.checkpoint().unwrap();
        // The checkpoint retired the WAL: reopen leans on the manifest.
        let pool = reopen(pool);
        assert_eq!(pool.read_block(1, 9).unwrap().unwrap(), b"through the tree");
    }

    #[test]
    fn an_overwrite_reads_back_newest_before_and_after_checkpoint() {
        let mut pool = pool(3);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 200 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.create_vdisk(2, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 4000 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 10 * BLOCK as u64, 0).unwrap();
        assert_eq!(
            pool.create_vdisk(1, 10 * BLOCK as u64, 0).unwrap_err(),
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 10 * BLOCK as u64, 0).unwrap();
        pool.create_vdisk(2, 10 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 20 * BLOCK as u64, 0).unwrap();
        assert_eq!(pool.read_block(1, 0).unwrap(), None);
    }

    #[test]
    fn a_collection_reclaims_overwritten_history_and_current_data_survives() {
        let mut pool = pool(11);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.create_vdisk(2, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 200 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, capacity * BLOCK as u64, 0).unwrap();
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
    fn a_lease_survives_a_restart() {
        // The whole reason it lives in the pool: a node that restarts must
        // know whether it may write before anyone tells it.
        let mut pool = pool(30);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.claim_lease(1, 7).unwrap();
        assert!(pool.may_write(1, 7));
        assert!(!pool.may_write(1, 8));
        pool.flush().unwrap();
        // Through the WAL...
        let mut pool = reopen(pool);
        assert!(pool.may_write(1, 7));
        pool.checkpoint().unwrap();
        // ...and through the manifest.
        let pool = reopen(pool);
        assert!(pool.may_write(1, 7));
        assert_eq!(pool.lease(1).unwrap().holder, 7);
    }

    #[test]
    fn a_handover_moves_the_pen_exactly_once() {
        let mut pool = pool(31);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.claim_lease(1, 0).unwrap();

        // The window: the guest still runs on 0, so 0 still writes and 1
        // still may not. This is the invariant a two-primaries window is
        // always one mistake away from breaking.
        pool.begin_handover(1, 0, 1).unwrap();
        assert!(pool.may_write(1, 0), "the source stopped writing too early");
        assert!(
            !pool.may_write(1, 1),
            "the destination wrote during the window"
        );

        // The instant of handover.
        pool.relinquish_lease(1, 0, 1).unwrap();
        assert!(!pool.may_write(1, 0));
        assert!(pool.may_write(1, 1));
        assert_eq!(pool.lease(1).unwrap().handing_to, None);
    }

    #[test]
    fn an_abandoned_migration_leaves_the_disk_where_it_was() {
        let mut pool = pool(32);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.claim_lease(1, 0).unwrap();
        pool.begin_handover(1, 0, 1).unwrap();
        pool.abort_handover(1, 0).unwrap();
        assert!(pool.may_write(1, 0));
        assert!(!pool.may_write(1, 1));
        // And with the window closed, the destination cannot take it.
        assert_eq!(
            pool.relinquish_lease(1, 0, 1).unwrap_err(),
            FsError::NoSuchHandover(1)
        );
    }

    #[test]
    fn a_held_lease_is_refused_to_everyone_but_its_holder() {
        let mut pool = pool(33);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.claim_lease(1, 0).unwrap();
        assert_eq!(
            pool.claim_lease(1, 1).unwrap_err(),
            FsError::LeaseHeld {
                vdisk: 1,
                holder: 0
            }
        );
        // The holder may re-claim its own without ceremony.
        pool.claim_lease(1, 0).unwrap();
        // And nobody may hand over what they do not hold.
        assert_eq!(
            pool.begin_handover(1, 1, 0).unwrap_err(),
            FsError::NotWriter(1)
        );
    }

    #[test]
    fn a_new_era_retires_the_leases_of_the_one_it_replaced() {
        // A survivor that has been told its peer is dead is not bound by
        // the arrangement it had with the peer — otherwise a failover
        // could never take the pen, and the vdisk would be unwritable
        // exactly when someone needed to save it.
        let mut pool = pool(34);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.claim_lease(1, 0).unwrap();
        assert!(pool.may_write(1, 0));

        pool.bump_era_to(2).unwrap();
        assert!(!pool.may_write(1, 0), "a stale lease still bound the pool");
        pool.claim_lease(1, 1).unwrap();
        assert!(pool.may_write(1, 1));
        let pool = reopen(pool);
        assert!(pool.may_write(1, 1));
    }

    #[test]
    fn an_era_bump_moves_forward_or_not_at_all() {
        // The bump's number is agreed above this pool (two survivors
        // minting independently could disagree), so what the pool itself
        // enforces is only that eras rise: a target at or below the
        // current era orders nothing and is refused.
        let mut pool = pool(35);
        assert_eq!(pool.era(), 1);
        pool.bump_era_to(2).unwrap();
        assert_eq!(pool.era(), 2);
        assert!(pool.bump_era_to(2).is_err(), "a flat bump was accepted");
        assert!(
            pool.bump_era_to(1).is_err(),
            "a backwards bump was accepted"
        );
        pool.bump_era_to(8).unwrap();
        assert_eq!(pool.era(), 8, "the agreed number was not adopted");
        let pool = reopen(pool);
        assert_eq!(pool.era(), 8, "the era bump was not anchored");
    }

    #[test]
    fn sync_pins_hold_the_offered_state_through_a_collection() {
        // A resync source keeps writing past the checkpoint it offered, so
        // its own next collection would sweep the offer's trees — unless
        // the pins hold them exactly as live as the present.
        let mut pool = pool(36);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        let old_payloads: Vec<Vec<u8>> = (0..20u64)
            .map(|index| {
                let mut data = vec![0u8; 600];
                data[0..8].copy_from_slice(&index.to_le_bytes());
                data
            })
            .collect();
        for (index, data) in old_payloads.iter().enumerate() {
            pool.write_block(1, index as u64, data).unwrap();
        }
        pool.checkpoint().unwrap();
        let (_, vdisks, _) = pool.sync_manifest();
        let root = vdisks[0].3.expect("a checkpointed vdisk has a root");
        let depth = map::depth_for(
            (40 * BLOCK as u64).div_ceil(BLOCK as u64),
            map::entries_per_node(BLOCK as u32),
        );
        pool.pin_sync(1, vec![(root, depth, 0)]);

        // The offer becomes history: every index rewritten, checkpointed,
        // collected.
        for index in 0..20u64 {
            let mut data = vec![0u8; 600];
            data[0..8].copy_from_slice(&index.to_le_bytes());
            data[8] = 0xEE;
            pool.write_block(1, index, &data).unwrap();
        }
        pool.checkpoint().unwrap();
        pool.collect_garbage().unwrap();
        for data in &old_payloads {
            assert!(
                pool.has_block(0, &hash_block(data)),
                "a collection swept a pinned offer's block"
            );
        }
        assert!(pool.has_block(0, &root), "the pinned root itself was swept");

        // Unpinned, the offer is garbage like any other history.
        pool.unpin_sync(1);
        pool.collect_garbage().unwrap();
        for data in &old_payloads {
            assert!(
                !pool.has_block(0, &hash_block(data)),
                "an unpinned offer survived collection"
            );
        }
    }

    #[test]
    fn an_inflight_payload_survives_a_collection_until_its_op_lands() {
        // A replicated payload arrives ahead of the op that references
        // it. In that gap nothing durable reaches the block, so an
        // unpinned collection would sweep it — and the op's replay would
        // then name a block the store does not hold. The window was
        // reachable at two nodes; the simulation had simply never
        // collected inside it.
        let mut pool = pool(37);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
        pool.write_block(1, 0, b"traffic to collect around")
            .unwrap();
        pool.checkpoint().unwrap();
        let payload = vec![0x5A; 600];
        let hash = pool.put_block(0, &payload).unwrap();
        pool.pin_inflight(9, 0, hash);
        pool.collect_garbage().unwrap();
        assert!(
            pool.has_block(0, &hash),
            "a collection swept a payload still waiting on its op"
        );
        // The op lands; the pin comes off; the block is now reachable
        // through the map instead.
        pool.write_block_prehashed(1, 1, hash).unwrap();
        pool.clear_inflight(9, 0, &hash);
        pool.checkpoint().unwrap();
        pool.collect_garbage().unwrap();
        assert!(pool.has_block(0, &hash), "a mapped block was swept");
        // And a pin whose op never arrives dies with its session.
        let orphan = pool.put_block(0, &vec![0x5B; 600]).unwrap();
        pool.pin_inflight(9, 0, orphan);
        pool.drop_session_pins(9);
        pool.collect_garbage().unwrap();
        assert!(
            !pool.has_block(0, &orphan),
            "a dead session's inflight pin outlived it"
        );
    }

    #[test]
    fn a_healthy_pool_scrubs_clean() {
        let mut pool = pool(13);
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 10 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
        pool.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
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
