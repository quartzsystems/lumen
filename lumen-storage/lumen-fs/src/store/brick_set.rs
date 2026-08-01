//! One node's bricks, presented as one store: the multi-disk half of
//! phase 4's "one brick per disk, per tier".
//!
//! The set adds exactly two ideas on top of [`Brick`], and refuses to be
//! anything grander:
//!
//! - **Routing.** Every block lives on exactly one brick, and the set
//!   remembers which in an in-memory route rebuilt from each brick's scan
//!   at open — the same "index is rebuilt, never persisted" position the
//!   brick itself takes. The route is keyed by `(tier, hash)`, because
//!   dedupe does not cross tiers: a block's tier is part of its home
//!   (docs/lumenfs.md), so the same bytes on two tiers are two blocks that
//!   happen to agree.
//! - **Allocation.** A new block goes to the **most-free brick of its
//!   tier**, ties broken by lowest brick uuid. A pure function of durable
//!   state — replayable in simulation, self-leveling on unequal disks (the
//!   big brick absorbs writes until free space equalizes), and with no
//!   counter to persist or desync. When the chosen brick is full the next
//!   most-free takes the block; when every brick of the tier is full, the
//!   set says [`FsError::Full`] and the caller's collection policy runs
//!   exactly as it does for one brick.
//!
//! Everything else stays per-brick and uncoordinated. The WAL ring and the
//! anchors live on the one brick whose superblock says WAL_HOLDER; the
//! anchor's roster names the set, so `open` can refuse a wrong or partial
//! assembly by name instead of serving reads from half a pool.
//!
//! ## Cross-brick duplicates are lawful debris
//!
//! A crash can leave the same `(tier, hash)` on two bricks: an unflushed
//! put to brick A survives the crash while the retry after recovery landed
//! on brick B. The route resolves such a duplicate deterministically — the
//! lowest brick position wins — and the collection passes each brick only
//! the live hashes *routed to it*, so the unrouted copy is swept instead of
//! leaking forever.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::{hash_block, BlockHash};
use crate::store::brick::{BlockRead, BlockWrite, Brick, BrickStats, ByteSpace, GcStats};
use crate::store::format::RosterEntry;

/// How much recently-read payload the set keeps in memory. Enough to hold
/// every hot map node of a busy vdisk with room left for data blocks; a
/// read served from here skips two device reads and a full BLAKE3
/// verification — the map walk's entire per-node cost.
const CACHE_BYTES: usize = 64 << 20;

/// Recently-read payloads by address — correct by construction, with no
/// invalidation anywhere. The address IS the content, so a cached payload
/// can never be stale; and existence is never answered from here, only
/// from the route, so a collected block disappears the moment its route
/// entry does and a stale cache entry is unreachable memory, not a wrong
/// answer. Scrub reads the platter through the brick directly and never
/// passes this way, so rot cannot hide behind it.
///
/// FIFO eviction: the reuse this exists for is the same map nodes fetched
/// again for every block of one large request, which no recency tracking
/// is needed to catch. Behind a mutex because `get` takes `&self`; the
/// engine's own lock means it is never contended.
#[derive(Debug, Default)]
struct PayloadCache {
    map: HashMap<(u8, BlockHash), Vec<u8>>,
    order: VecDeque<(u8, BlockHash)>,
    bytes: usize,
}

impl PayloadCache {
    fn get(&self, key: &(u8, BlockHash)) -> Option<Vec<u8>> {
        self.map.get(key).cloned()
    }

    fn insert(&mut self, key: (u8, BlockHash), payload: &[u8]) {
        if self.map.contains_key(&key) {
            return;
        }
        while self.bytes + payload.len() > CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.map.remove(&oldest) {
                self.bytes -= evicted.len();
            }
        }
        self.bytes += payload.len();
        self.order.push_back(key);
        self.map.insert(key, payload.to_vec());
    }
}

#[derive(Debug)]
pub struct BrickSet<D: Disk> {
    /// Sorted by `(tier, brick_uuid)` so every walk and every tie-break is
    /// deterministic on every node.
    bricks: Vec<Brick<D>>,
    /// The WAL holder's position in `bricks`.
    holder: usize,
    /// `(tier, hash)` → position in `bricks`. Rebuilt from the scans at
    /// open; a block on a brick belongs to that brick's tier by
    /// construction, because allocation never crosses tiers.
    route: HashMap<(u8, BlockHash), usize>,
    /// Bricks written since their last flush. The holder is marked on
    /// every mutable borrow — WAL appends and anchors go around the set's
    /// own write path, and a presumed-dirty holder costs one fsync.
    dirty: Vec<bool>,
    cache: Mutex<PayloadCache>,
}

impl<D: Disk> BrickSet<D> {
    /// Open every disk and assemble the set, refusing by name anything
    /// that is not exactly one node's pool.
    pub fn open(disks: Vec<D>) -> Result<BrickSet<D>> {
        let bricks = disks
            .into_iter()
            .map(Brick::open)
            .collect::<Result<Vec<_>>>()?;
        Self::assemble(bricks)
    }

    /// The single-brick pool, unchanged in everything but type.
    pub fn single(brick: Brick<D>) -> Result<BrickSet<D>> {
        Self::assemble(vec![brick])
    }

    /// Assemble already-opened bricks — the daemon's path, which opens
    /// each brick itself so it can remember which path produced which
    /// brick before the set's sort order takes over.
    pub fn from_bricks(bricks: Vec<Brick<D>>) -> Result<BrickSet<D>> {
        Self::assemble(bricks)
    }

    /// Format one node's set and return it assembled. The holder is
    /// formatted **last**, rostering the whole set: a crash anywhere in
    /// here leaves either no holder (no pool — orphan non-holders are
    /// reformatted by the retry) or a holder whose roster names bricks
    /// that all exist. Never an anchor pointing at bricks that were still
    /// to come.
    pub fn format(specs: Vec<(D, crate::store::brick::BrickParams)>) -> Result<BrickSet<D>> {
        let holders = specs.iter().filter(|(_, p)| p.wal_holder).count();
        if holders != 1 {
            return Err(FsError::BrickSetMismatch(
                "a set is formatted with exactly one WAL holder",
            ));
        }
        let roster: Vec<RosterEntry> = specs
            .iter()
            .map(|(_, p)| RosterEntry {
                brick_uuid: p.brick_uuid,
                tier: p.tier,
            })
            .collect();
        let mut bricks = Vec::with_capacity(specs.len());
        let mut holder_spec = None;
        for (disk, params) in specs {
            if params.wal_holder {
                holder_spec = Some((disk, params));
            } else {
                bricks.push(Brick::format(disk, params)?);
            }
        }
        let (disk, params) = holder_spec.expect("counted above");
        bricks.push(Brick::format_rostered(disk, params, roster)?);
        Self::assemble(bricks)
    }

    fn assemble(mut bricks: Vec<Brick<D>>) -> Result<BrickSet<D>> {
        if bricks.is_empty() {
            return Err(FsError::BrickSetMismatch("a set of no bricks"));
        }
        bricks.sort_by_key(|brick| (brick.tier(), brick.roster_entry().brick_uuid));

        let pool = bricks[0].pool_uuid();
        if bricks.iter().any(|b| b.pool_uuid() != pool) {
            return Err(FsError::BrickSetMismatch(
                "bricks of different pools presented as one set",
            ));
        }
        let block_size = bricks[0].block_size();
        if bricks.iter().any(|b| b.block_size() != block_size) {
            return Err(FsError::BrickSetMismatch(
                "bricks of different block sizes presented as one set",
            ));
        }
        let mut uuids = HashSet::new();
        if !bricks
            .iter()
            .all(|b| uuids.insert(b.roster_entry().brick_uuid))
        {
            return Err(FsError::BrickSetMismatch("the same brick presented twice"));
        }
        let holders: Vec<usize> = bricks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.wal_holder())
            .map(|(i, _)| i)
            .collect();
        let holder = match holders.as_slice() {
            [one] => *one,
            [] => {
                return Err(FsError::BrickSetMismatch(
                    "no brick holds the WAL — the node's ring has nowhere to live",
                ))
            }
            _ => {
                return Err(FsError::BrickSetMismatch(
                    "two bricks claim the WAL — two rings would be two histories",
                ))
            }
        };
        if bricks[holder].tier() != 0 {
            return Err(FsError::BrickSetMismatch(
                "the WAL holder must be a tier-0 brick",
            ));
        }

        // The roster is the durable statement of what this set is; the
        // disks presented must be exactly that, no more and no fewer. A
        // missing brick is data someone unplugged; an extra is a disk that
        // belongs to no anchored history.
        let anchor = bricks[holder]
            .read_best_anchor()?
            .ok_or(FsError::Corrupt("a formatted holder with no valid anchor"))?;
        let mut rostered: Vec<([u8; 16], u8)> = anchor
            .roster
            .iter()
            .map(|e| (e.brick_uuid, e.tier))
            .collect();
        rostered.sort_unstable();
        let mut present: Vec<([u8; 16], u8)> = bricks
            .iter()
            .map(|b| (b.roster_entry().brick_uuid, b.tier()))
            .collect();
        present.sort_unstable();
        if rostered != present {
            return Err(FsError::BrickSetMismatch(
                "the anchor rosters a different set than the bricks presented",
            ));
        }

        let mut route = HashMap::new();
        for (position, brick) in bricks.iter().enumerate() {
            let tier = brick.tier();
            for hash in brick.indexed_hashes() {
                // First in sorted order wins — the deterministic answer to
                // a crash-made duplicate; see the module header.
                route.entry((tier, hash)).or_insert(position);
            }
        }

        let dirty = vec![false; bricks.len()];
        Ok(BrickSet {
            bricks,
            holder,
            route,
            dirty,
            cache: Mutex::new(PayloadCache::default()),
        })
    }

    // -----------------------------------------------------------------
    // The store, per tier.

    /// Store a payload at a tier. Dedupe is set-wide within the tier; the
    /// target brick is the most-free of the tier, next-most-free on Full,
    /// and the tier's exhaustion surfaces as [`FsError::Full`] — never a
    /// silent landing on some other tier.
    pub fn put(&mut self, tier: u8, payload: &[u8]) -> Result<BlockHash> {
        let hash = hash_block(payload);
        self.put_prehashed(tier, hash, payload)?;
        Ok(hash)
    }

    /// Store a payload whose address the caller already computed. The
    /// write path hashes every block once to place it across the members,
    /// and hashing 16 KiB a second time here was a measured cost — so the
    /// hash rides down instead. Trusted exactly as far as its source:
    /// inside this crate, always from `hash_block` over these same bytes.
    pub fn put_prehashed(&mut self, tier: u8, hash: BlockHash, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Err(FsError::EmptyPayload);
        }
        if self.route.contains_key(&(tier, hash)) {
            return Ok(());
        }
        let mut candidates: Vec<usize> = (0..self.bricks.len())
            .filter(|&i| self.bricks[i].tier() == tier)
            .collect();
        if candidates.is_empty() {
            return Err(FsError::NoSuchTier(tier));
        }
        // Most free segments first; positions already order by uuid, and a
        // stable sort keeps that as the tie-break.
        candidates.sort_by_key(|&i| std::cmp::Reverse(self.bricks[i].free_segments()));
        for position in candidates {
            match self.bricks[position].put_hashed(hash, payload) {
                Ok(()) => {
                    self.route.insert((tier, hash), position);
                    self.dirty[position] = true;
                    // Write-through: a fresh block's map node is re-read
                    // on the very next walk.
                    self.cache.lock().unwrap().insert((tier, hash), payload);
                    return Ok(());
                }
                Err(FsError::Full) => continue,
                Err(err) => return Err(err),
            }
        }
        Err(FsError::Full)
    }

    /// A stored block's payload, by tier and address. Existence is the
    /// route's answer; only content may come from the cache.
    pub fn get(&self, tier: u8, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        match self.route.get(&(tier, *hash)) {
            Some(&position) => {
                let key = (tier, *hash);
                if let Some(hit) = self.cache.lock().unwrap().get(&key) {
                    return Ok(Some(hit));
                }
                let read = self.bricks[position].get(hash)?;
                if let Some(payload) = &read {
                    self.cache.lock().unwrap().insert(key, payload);
                }
                Ok(read)
            }
            None => Ok(None),
        }
    }

    /// Whether the tier holds this block.
    pub fn contains(&self, tier: u8, hash: &BlockHash) -> bool {
        self.route.contains_key(&(tier, *hash))
    }

    /// The acknowledgement barrier: everything put since the last flush is
    /// durable when this returns, on every brick it touched.
    pub fn flush(&mut self) -> Result<()> {
        for (position, dirty) in self.dirty.iter_mut().enumerate() {
            if *dirty {
                self.bricks[position].flush()?;
                *dirty = false;
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // The holder: WAL and anchors go through it, and only it.

    /// The WAL-holding brick, mutably — the handle `wal.append` and
    /// `write_anchor` go through. Marked dirty on the way out: a mutable
    /// borrow of the holder is presumed written, and the presumption costs
    /// one fsync where a missed write would cost the contract.
    pub fn wal_brick_mut(&mut self) -> &mut Brick<D> {
        self.dirty[self.holder] = true;
        &mut self.bricks[self.holder]
    }

    /// The WAL-holding brick, for reads.
    pub fn wal_brick(&self) -> &Brick<D> {
        &self.bricks[self.holder]
    }

    // -----------------------------------------------------------------
    // Facts about the set.

    /// The tiers this set carries, ascending.
    pub fn tiers(&self) -> Vec<u8> {
        let mut tiers: Vec<u8> = self.bricks.iter().map(|b| b.tier()).collect();
        tiers.dedup(); // sorted by construction
        tiers
    }

    pub fn has_tier(&self, tier: u8) -> bool {
        self.bricks.iter().any(|b| b.tier() == tier)
    }

    /// Every brick as its roster line, in the set's sorted order — what a
    /// checkpoint writes into the anchor.
    pub fn roster(&self) -> Vec<RosterEntry> {
        self.bricks.iter().map(|b| b.roster_entry()).collect()
    }

    pub fn block_size(&self) -> u32 {
        self.bricks[0].block_size()
    }

    pub fn pool_uuid(&self) -> [u8; 16] {
        self.bricks[0].pool_uuid()
    }

    /// The set's space as one summed figure — the shape `status` has
    /// always reported, kept byte-identical for one brick.
    pub fn stats(&self) -> BrickStats {
        let mut total = BrickStats {
            blocks: 0,
            segments_total: 0,
            segments_live: 0,
            segments_free: 0,
            payload_bytes: 0,
        };
        for brick in &self.bricks {
            let stats = brick.stats();
            total.blocks += stats.blocks;
            total.segments_total += stats.segments_total;
            total.segments_live += stats.segments_live;
            total.segments_free += stats.segments_free;
            total.payload_bytes += stats.payload_bytes;
        }
        total
    }

    /// The set's space in bytes, per tier and per brick — the report the
    /// capacity figure is computed from. Bricks appear in the set's sorted
    /// order; tiers ascend.
    pub fn space_report(&self) -> SpaceReport {
        let mut tiers: Vec<TierSpace> = Vec::new();
        for brick in &self.bricks {
            let space = brick.byte_space();
            let entry = BrickSpace {
                brick_uuid: brick.roster_entry().brick_uuid,
                tier: brick.tier(),
                wal_holder: brick.wal_holder(),
                space,
            };
            match tiers.iter_mut().find(|t| t.tier == brick.tier()) {
                Some(tier) => {
                    tier.space.add(&space);
                    tier.bricks.push(entry);
                }
                None => tiers.push(TierSpace {
                    tier: brick.tier(),
                    space,
                    bricks: vec![entry],
                }),
            }
        }
        SpaceReport { tiers }
    }

    /// Verify every brick's every block — scrub's store half, summed.
    pub fn scrub(&self) -> Result<(u64, Vec<BlockHash>)> {
        let mut verified = 0;
        let mut corrupt = Vec::new();
        for brick in &self.bricks {
            let (count, bad) = brick.scrub()?;
            verified += count;
            corrupt.extend(bad);
        }
        corrupt.sort_unstable();
        corrupt.dedup();
        Ok((verified, corrupt))
    }

    // -----------------------------------------------------------------
    // Collection.

    /// Make every brick's reserve available — a collection is running.
    pub fn open_reserve(&mut self) {
        for brick in &mut self.bricks {
            brick.open_reserve();
        }
    }

    pub fn close_reserve(&mut self) {
        for brick in &mut self.bricks {
            brick.close_reserve();
        }
    }

    /// Sweep every brick against the pool's live set, keyed by tier.
    ///
    /// Each brick keeps exactly the live hashes *routed to it*: a
    /// crash-made duplicate on another brick is not in that brick's retain
    /// set and is reclaimed — the deterministic route is also the garbage
    /// verdict. The route is rebuilt afterwards, because compaction moves
    /// blocks and dropping rewires nothing by itself.
    pub fn retain_and_reclaim(&mut self, live: &HashSet<(u8, BlockHash)>) -> Result<GcStats> {
        let mut per_brick: Vec<HashSet<BlockHash>> = vec![HashSet::new(); self.bricks.len()];
        for &(tier, hash) in live {
            if let Some(&position) = self.route.get(&(tier, hash)) {
                per_brick[position].insert(hash);
            }
        }
        let mut total = GcStats::default();
        for (position, keep) in per_brick.into_iter().enumerate() {
            let stats = self.bricks[position].retain_and_reclaim(&keep)?;
            self.dirty[position] = true;
            total.blocks_dropped += stats.blocks_dropped;
            total.blocks_moved += stats.blocks_moved;
            total.segments_freed += stats.segments_freed;
            total.segments_compacted += stats.segments_compacted;
        }
        self.route.clear();
        for (position, brick) in self.bricks.iter().enumerate() {
            let tier = brick.tier();
            for hash in brick.indexed_hashes() {
                self.route.entry((tier, hash)).or_insert(position);
            }
        }
        Ok(total)
    }

    /// Take the set apart — the crash-simulation reopen path.
    pub fn into_bricks(self) -> Vec<Brick<D>> {
        self.bricks
    }

    /// A tier of the set as a mutable block store — what a map fold writes
    /// tree nodes through.
    pub fn tier_mut(&mut self, tier: u8) -> TierMut<'_, D> {
        TierMut { set: self, tier }
    }

    /// A tier of the set as a read-only block store — lookups and walks.
    pub fn tier_ref(&self, tier: u8) -> TierRef<'_, D> {
        TierRef { set: self, tier }
    }
}

/// The set's space in bytes — see [`BrickSet::space_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceReport {
    /// Ascending by tier; every tier the set carries appears.
    pub tiers: Vec<TierSpace>,
}

/// One tier's space: the summed figures and each brick's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierSpace {
    pub tier: u8,
    pub space: ByteSpace,
    pub bricks: Vec<BrickSpace>,
}

/// One brick's space, with enough identity to say which disk it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrickSpace {
    pub brick_uuid: [u8; 16],
    pub tier: u8,
    pub wal_holder: bool,
    pub space: ByteSpace,
}

/// One tier of a [`BrickSet`], writable — see [`BrickSet::tier_mut`].
pub struct TierMut<'a, D: Disk> {
    set: &'a mut BrickSet<D>,
    tier: u8,
}

impl<D: Disk> BlockRead for TierMut<'_, D> {
    fn get(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        self.set.get(self.tier, hash)
    }
    fn contains(&self, hash: &BlockHash) -> bool {
        self.set.contains(self.tier, hash)
    }
    fn block_size(&self) -> u32 {
        self.set.block_size()
    }
}

impl<D: Disk> BlockWrite for TierMut<'_, D> {
    fn put(&mut self, payload: &[u8]) -> Result<BlockHash> {
        self.set.put(self.tier, payload)
    }
}

/// One tier of a [`BrickSet`], read-only — see [`BrickSet::tier_ref`].
pub struct TierRef<'a, D: Disk> {
    set: &'a BrickSet<D>,
    tier: u8,
}

impl<D: Disk> BlockRead for TierRef<'_, D> {
    fn get(&self, hash: &BlockHash) -> Result<Option<Vec<u8>>> {
        self.set.get(self.tier, hash)
    }
    fn contains(&self, hash: &BlockHash) -> bool {
        self.set.contains(self.tier, hash)
    }
    fn block_size(&self) -> u32 {
        self.set.block_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::sim::SimDisk;
    use crate::store::brick::BrickParams;

    const KIB: u64 = 1024;

    fn params(uuid: u8, tier: u8, wal_holder: bool) -> BrickParams {
        BrickParams {
            pool_uuid: [0xAA; 16],
            brick_uuid: [uuid; 16],
            block_size: 16 * KIB as u32,
            segment_size: 256 * KIB,
            wal_size: if wal_holder { 64 * KIB } else { 0 },
            tier,
            wal_holder,
        }
    }

    fn disk(seed: u64) -> SimDisk {
        SimDisk::new(4 * KIB * KIB, seed)
    }

    fn two_tier_set() -> BrickSet<SimDisk> {
        BrickSet::format(vec![
            (disk(1), params(1, 0, true)),
            (disk(2), params(2, 0, false)),
            (disk(3), params(3, 1, false)),
        ])
        .unwrap()
    }

    #[test]
    fn a_set_formats_together_rosters_everyone_and_reopens() {
        let mut set = two_tier_set();
        let fast = set.put(0, b"fast payload").unwrap();
        let slow = set.put(1, b"slow payload").unwrap();
        set.flush().unwrap();
        assert_eq!(
            set.roster().len(),
            3,
            "every brick appears in the roster view"
        );
        let anchor = set.wal_brick().read_best_anchor().unwrap().unwrap();
        assert_eq!(anchor.roster.len(), 3, "and in the durable anchor");

        let disks = set
            .into_bricks()
            .into_iter()
            .map(Brick::into_disk)
            .collect();
        let set = BrickSet::open(disks).unwrap();
        assert_eq!(set.get(0, &fast).unwrap().unwrap(), b"fast payload");
        assert_eq!(set.get(1, &slow).unwrap().unwrap(), b"slow payload");
        assert_eq!(set.tiers(), vec![0, 1]);
    }

    #[test]
    fn dedupe_is_per_tier_never_across() {
        let mut set = two_tier_set();
        let on_fast = set.put(0, b"the same bytes").unwrap();
        let blocks_before = set.stats().blocks;
        // The same payload again on the same tier: a no-op.
        assert_eq!(set.put(0, b"the same bytes").unwrap(), on_fast);
        assert_eq!(set.stats().blocks, blocks_before);
        // On the other tier: the same address, a second block — its tier
        // is part of its home.
        assert_eq!(set.put(1, b"the same bytes").unwrap(), on_fast);
        assert_eq!(set.stats().blocks, blocks_before + 1);
    }

    #[test]
    fn a_tier_with_no_brick_refuses_instead_of_spilling() {
        let mut set = two_tier_set();
        assert_eq!(set.put(7, b"payload").unwrap_err(), FsError::NoSuchTier(7));
        assert_eq!(set.get(7, &hash_block(b"payload")).unwrap(), None);
    }

    #[test]
    fn allocation_levels_toward_the_most_free_brick() {
        let mut set = BrickSet::format(vec![
            (disk(10), params(1, 0, true)),
            // Twice the disk: the roomier brick, and where writes go
            // until free space evens out.
            (SimDisk::new(8 * KIB * KIB, 11), params(2, 0, false)),
        ])
        .unwrap();
        assert!(
            set.bricks[1].free_segments() > set.bricks[0].free_segments(),
            "the premise of the test"
        );
        set.put(0, &[1u8; 16 * KIB as usize]).unwrap();
        assert_eq!(set.bricks[1].stats().blocks, 1);
        assert_eq!(set.bricks[0].stats().blocks, 0);
    }

    #[test]
    fn a_full_tier_surfaces_full_after_trying_every_brick() {
        // Two tiny tier-0 bricks: a 400 KiB disk holds one 256 KiB segment.
        let mut set = BrickSet::format(vec![
            (SimDisk::new(500 * KIB, 20), params(1, 0, true)),
            (SimDisk::new(400 * KIB, 21), params(2, 0, false)),
        ])
        .unwrap();
        let mut stored = 0;
        loop {
            match set.put(0, &[stored as u8; 16 * KIB as usize]) {
                Ok(_) => stored += 1,
                Err(FsError::Full) => break,
                Err(err) => panic!("unexpected: {err}"),
            }
        }
        assert!(stored > 0, "the set stored something before filling");
    }

    #[test]
    fn assembly_refuses_what_is_not_one_set() {
        // A foreign pool among the bricks.
        let foreign = BrickParams {
            pool_uuid: [0xBB; 16],
            ..params(2, 0, false)
        };
        let bricks = vec![
            Brick::format(disk(30), params(1, 0, true)).unwrap(),
            Brick::format(disk(31), foreign).unwrap(),
        ];
        assert!(matches!(
            BrickSet::assemble(bricks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));

        // No holder anywhere.
        let bricks = vec![Brick::format(disk(32), params(1, 1, false)).unwrap()];
        assert!(matches!(
            BrickSet::assemble(bricks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));

        // Two holders.
        let bricks = vec![
            Brick::format(disk(33), params(1, 0, true)).unwrap(),
            Brick::format(disk(34), params(2, 0, true)).unwrap(),
        ];
        assert!(matches!(
            BrickSet::assemble(bricks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));

        // A holder off tier 0.
        let bricks = vec![Brick::format(disk(35), params(1, 2, true)).unwrap()];
        assert!(matches!(
            BrickSet::assemble(bricks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));
    }

    #[test]
    fn a_missing_brick_fails_the_roster_check_at_open() {
        let mut disks: Vec<SimDisk> = two_tier_set()
            .into_bricks()
            .into_iter()
            .map(Brick::into_disk)
            .collect();
        // One brick left unplugged.
        disks.pop();
        assert!(matches!(
            BrickSet::open(disks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));
    }

    #[test]
    fn an_extra_brick_fails_the_roster_check_at_open() {
        let mut disks: Vec<SimDisk> = two_tier_set()
            .into_bricks()
            .into_iter()
            .map(Brick::into_disk)
            .collect();
        // A formatted brick from nowhere: it belongs to the pool by uuid,
        // but no anchored history rosters it.
        disks.push(
            Brick::format(disk(40), params(9, 1, false))
                .unwrap()
                .into_disk(),
        );
        assert!(matches!(
            BrickSet::open(disks).unwrap_err(),
            FsError::BrickSetMismatch(_)
        ));
    }

    /// The label "usable" cannot drift: the arithmetic is pinned to
    /// numbers computed by hand from the geometry, not to itself.
    #[test]
    fn the_byte_arithmetic_is_pinned_to_hand_computed_numbers() {
        let set = two_tier_set();
        let report = set.space_report();
        // A 16 KiB block spans 20 KiB on disk (64-byte header, padded to
        // the next sector), so 12 fit a 256 KiB segment beside its header
        // sector: 196608 payload bytes per segment. Every disk here ends
        // up with 15 segments, one of them the collection reserve.
        let per_brick = 14 * 196_608u64;
        assert_eq!(report.tiers.len(), 2);
        let tier0 = &report.tiers[0];
        assert_eq!(tier0.tier, 0);
        assert_eq!(tier0.bricks.len(), 2);
        assert_eq!(tier0.space.usable_bytes, 2 * per_brick);
        assert_eq!(
            tier0.space.free_bytes,
            2 * per_brick,
            "a fresh set is all free"
        );
        assert_eq!(tier0.space.payload_bytes, 0);
        let tier1 = &report.tiers[1];
        assert_eq!(tier1.tier, 1);
        assert_eq!(tier1.space.usable_bytes, per_brick);
        assert!(!tier1.bricks[0].wal_holder);
        assert!(
            tier0.bricks.iter().any(|b| b.wal_holder),
            "the holder sits on tier 0"
        );
    }

    #[test]
    fn collection_keeps_what_is_routed_and_reclaims_the_rest() {
        let mut set = two_tier_set();
        let keep = set.put(0, b"keep me").unwrap();
        let drop_hash = set.put(0, b"drop me").unwrap();
        let slow = set.put(1, b"slow keep").unwrap();
        set.flush().unwrap();

        let mut live = HashSet::new();
        live.insert((0u8, keep));
        live.insert((1u8, slow));
        set.open_reserve();
        let stats = set.retain_and_reclaim(&live).unwrap();
        set.close_reserve();
        assert!(stats.blocks_dropped >= 1);
        assert_eq!(set.get(0, &keep).unwrap().unwrap(), b"keep me");
        assert_eq!(set.get(1, &slow).unwrap().unwrap(), b"slow keep");
        assert_eq!(set.get(0, &drop_hash).unwrap(), None);
    }
}
