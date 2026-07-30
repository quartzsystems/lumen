//! The multi-brick pool under fire: phase 4's "many bricks, one node",
//! held to the same durability contract as one brick.
//!
//! What these histories pin, beyond what the single-brick suites already
//! prove (and must keep proving, unchanged):
//!
//! - acknowledged writes survive crashes that hit **every brick of the set
//!   independently** — each disk keeps, drops, or tears its own pending
//!   writes, and recovery must still surface every acknowledged block;
//! - the two-tier shape holds: data on its vdisk's tier, map nodes on
//!   tier 0, the WAL on its one holder;
//! - per-tier liveness: the same bytes on two tiers are two blocks, and
//!   collecting one vdisk's history never eats the other tier's copy;
//! - a set that lost or grew a brick refuses to open, and a pair whose
//!   brick sets disagree about tiers refuses the vdisk rather than
//!   diverging.

use std::collections::HashMap;

use lumen_fs::{
    Brick, BrickParams, BrickSet, Effect, FsError, Pool, ReplNode, ReplState, SimDisk, SplitMix64,
};

const KIB: u64 = 1024;
const BLOCK: usize = 4 * KIB as usize;
const FAST: u64 = 1; // vdisk on tier 0
const SLOW: u64 = 2; // vdisk on tier 1
const CAPACITY: u64 = 40;

fn params(uuid: u8, tier: u8, wal_holder: bool) -> BrickParams {
    BrickParams {
        pool_uuid: [0xAA; 16],
        brick_uuid: [uuid; 16],
        block_size: BLOCK as u32,
        segment_size: 128 * KIB,
        wal_size: if wal_holder { 32 * KIB } else { 0 },
        tier,
        wal_holder,
    }
}

/// Three disks: two tier-0 bricks (one holding the WAL) and a tier-1
/// brick, formatted together.
fn fresh_pool(seed: u64) -> Pool<SimDisk> {
    let set = BrickSet::format(vec![
        (SimDisk::new(4 * KIB * KIB, seed), params(1, 0, true)),
        (
            SimDisk::new(4 * KIB * KIB, seed.wrapping_add(1)),
            params(2, 0, false),
        ),
        (
            SimDisk::new(4 * KIB * KIB, seed.wrapping_add(2)),
            params(3, 1, false),
        ),
    ])
    .unwrap();
    let mut pool = Pool::create_set(set).unwrap();
    pool.create_vdisk(FAST, CAPACITY * BLOCK as u64, 0).unwrap();
    pool.create_vdisk(SLOW, CAPACITY * BLOCK as u64, 1).unwrap();
    pool.flush().unwrap();
    pool
}

/// Crash every disk of the set — independently, which is the point — and
/// reopen.
fn crash_and_reopen(pool: Pool<SimDisk>) -> Pool<SimDisk> {
    let disks: Vec<SimDisk> = pool
        .into_bricks()
        .into_iter()
        .map(|brick| {
            let mut disk = brick.into_disk();
            disk.crash();
            disk
        })
        .collect();
    Pool::open_set(BrickSet::open(disks).unwrap()).unwrap()
}

/// A payload no other `(vdisk, index, generation)` produces.
fn payload(vdisk: u64, index: u64, generation: u32) -> Vec<u8> {
    let mut data = vec![0u8; 600];
    data[0..8].copy_from_slice(&vdisk.to_le_bytes());
    data[8..16].copy_from_slice(&index.to_le_bytes());
    data[16..20].copy_from_slice(&generation.to_le_bytes());
    data
}

/// WalFull means checkpoint and retry; Full means collect and retry —
/// the daemon's own policy, restated for the harness.
fn with_room(
    pool: &mut Pool<SimDisk>,
    mut op: impl FnMut(&mut Pool<SimDisk>) -> Result<(), FsError>,
) {
    for _ in 0..3 {
        match op(pool) {
            Err(FsError::WalFull) => pool.checkpoint().unwrap(),
            Err(FsError::Full) => {
                pool.collect_garbage().unwrap();
            }
            other => return other.unwrap(),
        }
    }
    op(pool).unwrap()
}

#[test]
fn a_two_tier_pool_serves_both_tiers_and_survives_reopen() {
    let mut pool = fresh_pool(100);
    for index in 0..CAPACITY {
        pool.write_block(FAST, index, &payload(FAST, index, 1))
            .unwrap();
        pool.write_block(SLOW, index, &payload(SLOW, index, 1))
            .unwrap();
    }
    pool.flush().unwrap();
    pool.checkpoint().unwrap();

    let disks: Vec<SimDisk> = pool
        .into_bricks()
        .into_iter()
        .map(Brick::into_disk)
        .collect();
    let pool = Pool::open_set(BrickSet::open(disks).unwrap()).unwrap();
    for index in 0..CAPACITY {
        assert_eq!(
            pool.read_block(FAST, index).unwrap().unwrap(),
            payload(FAST, index, 1)
        );
        assert_eq!(
            pool.read_block(SLOW, index).unwrap().unwrap(),
            payload(SLOW, index, 1)
        );
    }
    let report = pool.scrub().unwrap();
    assert!(report.corrupt.is_empty() && report.missing.is_empty());
    assert_eq!(pool.vdisk_tier(FAST).unwrap(), 0);
    assert_eq!(pool.vdisk_tier(SLOW).unwrap(), 1);
}

/// The phase-1 exit test's shape, over a set: every acknowledged write
/// survives every crash history, when every brick crashes independently.
#[test]
fn every_acknowledged_write_survives_crashes_across_the_set() {
    for seed in 0..12u64 {
        let mut rng = SplitMix64::new(0x0B41_C5E7 ^ seed);
        let mut pool = fresh_pool(200 + seed * 10);
        // (vdisk, index) -> the acknowledged generation.
        let mut acked: HashMap<(u64, u64), u32> = HashMap::new();
        let mut generation = 0u32;

        for _round in 0..8 {
            generation += 1;
            // A batch across both tiers.
            let mut pending: Vec<(u64, u64)> = Vec::new();
            for _ in 0..6 {
                let vdisk = if rng.next_u64().is_multiple_of(2) {
                    FAST
                } else {
                    SLOW
                };
                let index = rng.next_u64() % CAPACITY;
                let gen = generation;
                with_room(&mut pool, |p| {
                    p.write_block(vdisk, index, &payload(vdisk, index, gen))
                });
                pending.push((vdisk, index));
            }
            let crash_before_flush = rng.next_u64().is_multiple_of(3);
            if crash_before_flush {
                pool = crash_and_reopen(pool);
                // The batch was never acknowledged: each touched index may
                // hold the old value or the new one — never garbage.
                for (vdisk, index) in pending {
                    let read = pool.read_block(vdisk, index).unwrap();
                    let old = acked
                        .get(&(vdisk, index))
                        .map(|gen| payload(vdisk, index, *gen));
                    let new = payload(vdisk, index, generation);
                    assert!(
                        read == old || read.as_deref() == Some(new.as_slice()),
                        "seed {seed}: unacked write at ({vdisk},{index}) came back as neither old nor new"
                    );
                    // Whatever survived is now the value to hold it to.
                    if read.as_deref() == Some(new.as_slice()) {
                        acked.insert((vdisk, index), generation);
                    }
                }
            } else {
                pool.flush().unwrap();
                for (vdisk, index) in pending {
                    acked.insert((vdisk, index), generation);
                }
                if rng.next_u64().is_multiple_of(2) {
                    pool.checkpoint().unwrap();
                }
                if rng.next_u64().is_multiple_of(4) {
                    pool.collect_garbage().unwrap();
                }
                pool = crash_and_reopen(pool);
            }
            // Every acknowledged write is intact, whatever just happened.
            for ((vdisk, index), gen) in &acked {
                assert_eq!(
                    pool.read_block(*vdisk, *index).unwrap().unwrap(),
                    payload(*vdisk, *index, *gen),
                    "seed {seed}: an acknowledged write at ({vdisk},{index}) did not survive"
                );
            }
        }
        let report = pool.scrub().unwrap();
        assert!(
            report.corrupt.is_empty() && report.missing.is_empty(),
            "seed {seed}: the surviving pool does not scrub clean"
        );
    }
}

/// The same bytes on two tiers are two blocks: deleting one vdisk's
/// history collects its tier's copy and leaves the other tier serving.
#[test]
fn per_tier_liveness_collects_one_tiers_copy_and_keeps_the_others() {
    let mut pool = fresh_pool(300);
    // Identical content on both tiers — the dedupe-never-crosses case.
    for index in 0..10 {
        let same = payload(99, index, 7);
        pool.write_block(FAST, index, &same).unwrap();
        pool.write_block(SLOW, index, &same).unwrap();
    }
    pool.flush().unwrap();
    pool.checkpoint().unwrap();

    // Retire the fast vdisk's content entirely.
    for index in 0..10 {
        pool.trim_block(FAST, index).unwrap();
    }
    pool.flush().unwrap();
    pool.collect_garbage().unwrap();

    // The slow tier still serves every byte.
    for index in 0..10 {
        assert_eq!(
            pool.read_block(SLOW, index).unwrap().unwrap(),
            payload(99, index, 7)
        );
        assert_eq!(pool.read_block(FAST, index).unwrap(), None);
    }
    let report = pool.scrub().unwrap();
    assert!(report.corrupt.is_empty() && report.missing.is_empty());
}

#[test]
fn a_vdisk_on_a_tier_the_set_lacks_is_refused() {
    let mut pool = fresh_pool(400);
    assert_eq!(
        pool.create_vdisk(9, BLOCK as u64, 5).unwrap_err(),
        FsError::NoSuchTier(5)
    );
}

// ---------------------------------------------------------------------------
// Asymmetric brick sets across a replicating pair.

fn single_tier_node(seed: u64, id: u8) -> ReplNode<SimDisk> {
    let brick = Brick::format(
        SimDisk::new(8 * KIB * KIB, seed),
        BrickParams {
            brick_uuid: [id + 10; 16],
            ..params(1, 0, true)
        },
    )
    .unwrap();
    ReplNode::new(Pool::create(brick).unwrap(), id)
}

fn two_tier_node(seed: u64, id: u8) -> ReplNode<SimDisk> {
    let set = BrickSet::format(vec![
        (
            SimDisk::new(4 * KIB * KIB, seed),
            BrickParams {
                brick_uuid: [id + 20; 16],
                ..params(0, 0, true)
            },
        ),
        (
            SimDisk::new(4 * KIB * KIB, seed.wrapping_add(1)),
            BrickParams {
                brick_uuid: [id + 30; 16],
                ..params(0, 1, false)
            },
        ),
    ])
    .unwrap();
    ReplNode::new(Pool::create_set(set).unwrap(), id)
}

/// Deliver every queued message both ways until quiet; sends are captured
/// from effects. Returns an error the moment either side refuses one.
fn pump(a: &mut ReplNode<SimDisk>, b: &mut ReplNode<SimDisk>) -> Result<(), FsError> {
    loop {
        let mut moved = false;
        let mut to_b = Vec::new();
        for effect in a.take_effects() {
            if let Effect::Send(message) = effect {
                to_b.push(message);
            }
        }
        let mut to_a = Vec::new();
        for effect in b.take_effects() {
            if let Effect::Send(message) = effect {
                to_a.push(message);
            }
        }
        for message in to_b {
            moved = true;
            b.handle(message)?;
        }
        for message in to_a {
            moved = true;
            a.handle(message)?;
        }
        if !moved {
            return Ok(());
        }
    }
}

#[test]
fn a_tier_the_peer_lacks_is_refused_at_create_not_discovered_at_replay() {
    let mut a = two_tier_node(500, 0);
    let mut b = single_tier_node(600, 1);
    a.connect();
    b.connect();
    pump(&mut a, &mut b).unwrap();
    assert_eq!(a.state(), ReplState::Synced);

    // Tier 0 exists on both: fine.
    a.create_vdisk(1, 40 * BLOCK as u64, 0).unwrap();
    pump(&mut a, &mut b).unwrap();

    // Tier 1 exists only here: the create refuses up front, so the peer's
    // replay never sees an op it cannot apply.
    assert_eq!(
        a.create_vdisk(2, 40 * BLOCK as u64, 1).unwrap_err(),
        FsError::TierNotOnPeer(1)
    );
}

#[test]
fn an_offer_naming_a_tier_the_target_lacks_is_refused_by_name() {
    // A runs alone under a verdict and creates a tier-1 vdisk; B, whose
    // set has no tier 1, then returns. The resync must refuse before a
    // block moves, not adopt a vdisk it cannot store.
    let mut a = two_tier_node(700, 0);
    let mut b = single_tier_node(800, 1);
    a.connect();
    b.connect();
    pump(&mut a, &mut b).unwrap();

    a.peer_lost();
    b.peer_lost();
    a.set_peer_fenced().unwrap();
    a.create_vdisk(5, 40 * BLOCK as u64, 1).unwrap();
    a.write_block(5, 0, b"tier one only").unwrap();
    a.flush().unwrap();

    a.connect();
    b.connect();
    let outcome = pump(&mut a, &mut b);
    assert_eq!(outcome.unwrap_err(), FsError::NoSuchTier(1));
}
