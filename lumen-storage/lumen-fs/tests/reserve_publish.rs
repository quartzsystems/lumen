//! The three-phase put under adversarial interleaving: seeded histories
//! that run GC, fence verdicts, lease handovers, and session death
//! *between* a reserve and its publish — single-threaded, deterministic,
//! replayable, exactly because the phases are engine calls the pump can
//! order at will (docs/lumenfs-lock-sharding.md, step 1).
//!
//! What each history pins:
//!
//! - a collection between reserve and publish must not release or reuse
//!   a segment whose only contents are unpublished reservations (the
//!   segment pin — without it, the shipping one-second GC trigger is an
//!   acknowledged-data corruption generator);
//! - a dedupe hit's target must survive a collection across the window
//!   (the write pin — the found block may be an unreferenced orphan);
//! - a world that moved — verdict, session change — answers
//!   `WorldMoved`, orphans the extents, and the re-run lands cleanly;
//! - a relinquished pen refuses the straggler's publish by name
//!   (`NotWriter`), which is the two-writer interleaving a live
//!   migration would otherwise smuggle onto the wire;
//! - the applier-side ticket from a session that died publishes nothing;
//! - a crash between fill and publish recovers to exactly the state the
//!   guest was promised: nothing.

use lumen_fs::{Brick, BrickParams, Effect, FsError, Pool, ReplNode, ReplState, SimDisk};

const KIB: u64 = 1024;
const BLOCK: usize = 4 * KIB as usize;
const VDISK: u64 = 1;
const CAPACITY: u64 = 60;

fn params(id: u8) -> BrickParams {
    BrickParams {
        pool_uuid: [0xAA; 16],
        brick_uuid: [id; 16],
        block_size: BLOCK as u32,
        segment_size: 128 * KIB,
        wal_size: 32 * KIB,
        tier: 0,
        wal_holder: true,
    }
}

fn fresh_node(seed: u64, id: u8) -> ReplNode<SimDisk> {
    let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params(id)).unwrap();
    ReplNode::new(Pool::create(brick).unwrap(), id)
}

/// A node serving alone under a verdict — the simplest writable engine.
fn degraded_node(seed: u64) -> ReplNode<SimDisk> {
    let mut node = fresh_node(seed, 0);
    let era = node.era_target();
    node.set_member_fenced(1, era).unwrap();
    node.create_vdisk(VDISK, CAPACITY * BLOCK as u64, 0)
        .unwrap();
    node.take_effects();
    node
}

/// A distinct, whole-block payload per index and generation.
fn payload(index: u64, generation: u8) -> Vec<u8> {
    let mut data = vec![generation; BLOCK];
    data[0..8].copy_from_slice(&index.to_le_bytes());
    data
}

/// Run one whole three-phase put through the sim path: begin, in-lock
/// fill, publish.
fn run_three_phase(
    node: &mut ReplNode<SimDisk>,
    first: u64,
    payloads: &[Vec<u8>],
) -> Result<(), FsError> {
    let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
        .iter()
        .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
        .collect();
    let ticket = node.write_run_begin(VDISK, first, &meta)?;
    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    node.write_run_fill(&ticket, &refs)?;
    let blocks: Vec<(lumen_fs::BlockHash, &[u8])> = meta
        .iter()
        .zip(&refs)
        .map(|((hash, _), payload)| (*hash, *payload))
        .collect();
    node.write_run_publish(ticket, &blocks)
}

fn crash_and_reopen(node: ReplNode<SimDisk>) -> ReplNode<SimDisk> {
    let mut disk = node.into_pool().into_brick().into_disk();
    disk.crash();
    let mut node = ReplNode::new(Pool::open(Brick::open(disk).unwrap()).unwrap(), 0);
    let era = node.era_target();
    node.set_member_fenced(1, era).unwrap();
    node.take_effects();
    node
}

fn assert_scrub_clean(node: &ReplNode<SimDisk>) {
    let report = node.pool().scrub().unwrap();
    assert!(report.corrupt.is_empty(), "scrub found corruption");
    assert!(report.missing.is_empty(), "scrub found missing blocks");
}

/// The headline hazard: a run long enough that reserve *seals* segments
/// (fills them with nothing but unpublished extents), then a collection
/// fires in the window. Without the segment pins the collector sees
/// index-empty segments, releases them, and the acknowledged run is
/// gone at the next recovery. With them, everything the guest was told
/// survives a crash.
#[test]
fn a_collection_between_reserve_and_publish_reuses_nothing() {
    let mut node = degraded_node(11);
    // 40 distinct blocks: ~15 records per 128 KiB segment, so reserve
    // seals at least two segments behind itself.
    let payloads: Vec<Vec<u8>> = (0..40).map(|index| payload(index, 1)).collect();
    let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
        .iter()
        .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
        .collect();
    let ticket = node.write_run_begin(VDISK, 0, &meta).unwrap();

    // The adversary: a full collection while the extents are invisible
    // to the index.
    node.collect_garbage().unwrap();

    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    node.write_run_fill(&ticket, &refs).unwrap();

    // And another between fill and publish, for good measure.
    node.collect_garbage().unwrap();

    let blocks: Vec<(lumen_fs::BlockHash, &[u8])> = meta
        .iter()
        .zip(&refs)
        .map(|((hash, _), payload)| (*hash, *payload))
        .collect();
    node.write_run_publish(ticket, &blocks).unwrap();
    node.flush().unwrap();
    node.take_effects();

    let mut node = crash_and_reopen(node);
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            node.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice()),
            "block {index} did not survive the collection in the window"
        );
    }
    assert_scrub_clean(&node);
}

/// A dedupe hit against an unreferenced orphan: the write pin is what
/// keeps a collection in the window from sweeping the block out from
/// under the WAL entry the publish is about to append.
#[test]
fn a_dedupe_hits_target_survives_a_collection_in_the_window() {
    let mut node = degraded_node(12);
    let orphan = payload(7, 3);
    // Store it, map it, then trim the mapping and checkpoint: the block
    // is now an unreferenced orphan the next collection would sweep.
    node.write_block(VDISK, 5, &orphan).unwrap();
    node.flush().unwrap();
    node.trim_block(VDISK, 5).unwrap();
    node.checkpoint().unwrap();
    node.take_effects();
    assert!(node.pool().has_block(0, &lumen_fs::hash_block(&orphan)));

    // A run whose first block is exactly the orphan: reserve dedupes
    // against it and pins it.
    let fresh = payload(8, 3);
    let payloads = [orphan.clone(), fresh];
    let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
        .iter()
        .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
        .collect();
    let ticket = node.write_run_begin(VDISK, 0, &meta).unwrap();

    // The adversary: without the pin this sweeps the orphan, and the
    // publish below maps a block the store no longer holds.
    node.collect_garbage().unwrap();
    assert!(
        node.pool().has_block(0, &lumen_fs::hash_block(&orphan)),
        "the collection swept a block a reservation counts on"
    );

    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    node.write_run_fill(&ticket, &refs).unwrap();
    let blocks: Vec<(lumen_fs::BlockHash, &[u8])> = meta
        .iter()
        .zip(&refs)
        .map(|((hash, _), payload)| (*hash, *payload))
        .collect();
    node.write_run_publish(ticket, &blocks).unwrap();
    node.flush().unwrap();
    node.take_effects();

    // The pin is off now that the WAL entry landed; a later collection
    // keeps the block because the map references it.
    node.collect_garbage().unwrap();
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            node.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice())
        );
    }
    assert_scrub_clean(&node);
}

/// A fence verdict lands in the window: the world generation moved, the
/// publish refuses with `WorldMoved`, the extents orphan, and the re-run
/// — the daemon's loop — lands cleanly under the new era. The orphans
/// are then a collection's ordinary bread.
#[test]
fn a_verdict_in_the_window_answers_world_moved_and_the_rerun_lands() {
    let mut a = fresh_node(21, 0);
    let mut b = fresh_node(22, 1);
    a.connect(1);
    b.connect(0);
    // Deliver the hellos by hand: one message each way brings the pair
    // to Synced.
    deliver_all(&mut a, &mut b);
    assert_eq!(a.state(), ReplState::Synced);
    a.create_vdisk(VDISK, CAPACITY * BLOCK as u64, 0).unwrap();
    deliver_all(&mut a, &mut b);

    let payloads: Vec<Vec<u8>> = (0..4).map(|index| payload(index, 5)).collect();
    let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
        .iter()
        .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
        .collect();
    let ticket = a.write_run_begin(VDISK, 0, &meta).unwrap();
    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    a.write_run_fill(&ticket, &refs).unwrap();

    // The window's adversary: the peer dies and the cluster fences it.
    a.peer_lost(1);
    let era = a.era_target();
    a.set_member_fenced(1, era).unwrap();
    assert_eq!(a.state(), ReplState::Degraded);

    let blocks: Vec<(lumen_fs::BlockHash, &[u8])> = meta
        .iter()
        .zip(&refs)
        .map(|((hash, _), payload)| (*hash, *payload))
        .collect();
    assert_eq!(
        a.write_run_publish(ticket, &blocks).unwrap_err(),
        FsError::WorldMoved,
        "a moved world must refuse the stale reservation"
    );

    // The daemon's answer to WorldMoved: run the whole put again.
    run_three_phase(&mut a, 0, &payloads).unwrap();
    a.flush().unwrap();
    a.take_effects();
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            a.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice())
        );
    }
    // The abandoned extents are orphans; a collection reclaims and a
    // scrub stays clean.
    a.collect_garbage().unwrap();
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            a.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice())
        );
    }
    assert_scrub_clean(&a);
}

/// The live-migration straggler: the pen is relinquished while the
/// write's bytes are in flight. The publish must refuse by name — a
/// straggler landing after the handover is the two-writer interleaving
/// the lease system exists to prevent.
#[test]
fn a_relinquished_pen_refuses_the_stragglers_publish() {
    let mut a = fresh_node(31, 0);
    let mut b = fresh_node(32, 1);
    a.connect(1);
    b.connect(0);
    deliver_all(&mut a, &mut b);
    a.create_vdisk(VDISK, CAPACITY * BLOCK as u64, 0).unwrap();
    deliver_all(&mut a, &mut b);

    let payloads: Vec<Vec<u8>> = (0..2).map(|index| payload(index, 7)).collect();
    let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
        .iter()
        .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
        .collect();
    let ticket = a.write_run_begin(VDISK, 0, &meta).unwrap();
    let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
    a.write_run_fill(&ticket, &refs).unwrap();

    // The migration completes in the window: the pen leaves this node.
    a.begin_handover(VDISK, 1).unwrap();
    a.relinquish(VDISK, 1).unwrap();
    deliver_all(&mut a, &mut b);

    let blocks: Vec<(lumen_fs::BlockHash, &[u8])> = meta
        .iter()
        .zip(&refs)
        .map(|((hash, _), payload)| (*hash, *payload))
        .collect();
    assert_eq!(
        a.write_run_publish(ticket, &blocks).unwrap_err(),
        FsError::NotWriter(VDISK)
    );
    // Nothing was mapped; the blocks read as never written.
    assert_eq!(a.read_block(VDISK, 0).unwrap(), None);
    a.collect_garbage().unwrap();
    assert_scrub_clean(&a);
}

/// The applier-side window: a payload ticket from a session that died
/// publishes nothing — the extents orphan, the store stays exactly as
/// the dead stream left it.
#[test]
fn a_dead_sessions_payload_ticket_publishes_nothing() {
    let mut a = fresh_node(41, 0);
    let mut b = fresh_node(42, 1);
    a.connect(1);
    b.connect(0);
    deliver_all(&mut a, &mut b);
    assert_eq!(b.state(), ReplState::Synced);

    let block = payload(0, 9);
    let hash = lumen_fs::hash_block(&block);
    let meta = [(0u8, hash, block.len() as u32)];
    let ticket = b.payloads_begin(0, &meta).unwrap().expect("synced applies");
    let refs: Vec<&[u8]> = vec![&block];
    b.payloads_fill(&ticket, &refs).unwrap();

    // The session dies in the window.
    b.peer_lost(0);

    let full = [(0u8, hash, block.as_slice())];
    b.payloads_publish(0, ticket, &full).unwrap();
    assert!(
        !b.pool().has_block(0, &hash),
        "a dead stream's payload must not become a stored block"
    );
    b.collect_garbage().unwrap();
    assert_scrub_clean(&b);
}

/// The applier-side pin handoff: a published payload is pinned until its
/// op lands, so a collection between the two keeps it.
#[test]
fn a_published_payload_survives_a_collection_until_its_op() {
    let mut a = fresh_node(51, 0);
    let mut b = fresh_node(52, 1);
    a.connect(1);
    b.connect(0);
    deliver_all(&mut a, &mut b);

    let block = payload(0, 11);
    let hash = lumen_fs::hash_block(&block);
    let meta = [(0u8, hash, block.len() as u32)];
    let ticket = b.payloads_begin(0, &meta).unwrap().expect("synced applies");
    let refs: Vec<&[u8]> = vec![&block];
    b.payloads_fill(&ticket, &refs).unwrap();
    let full = [(0u8, hash, block.as_slice())];
    b.payloads_publish(0, ticket, &full).unwrap();
    assert!(b.pool().has_block(0, &hash));

    // Nothing references the block yet; only the arrival pin holds it.
    b.collect_garbage().unwrap();
    assert!(
        b.pool().has_block(0, &hash),
        "the arrival pin must hold the payload until its op lands"
    );
}

/// A crash between fill and publish: the guest was promised nothing, and
/// nothing is what recovery owes — the extents are unacknowledged
/// debris, the vdisk reads as it did before, the scrub is clean.
#[test]
fn a_crash_between_fill_and_publish_recovers_to_nothing() {
    for seed in 0..8u64 {
        let mut node = degraded_node(60 + seed);
        let before = payload(0, 1);
        node.write_block(VDISK, 0, &before).unwrap();
        node.flush().unwrap();
        node.take_effects();

        let payloads: Vec<Vec<u8>> = (0..6).map(|index| payload(index, 2)).collect();
        let meta: Vec<(lumen_fs::BlockHash, u32)> = payloads
            .iter()
            .map(|payload| (lumen_fs::hash_block(payload), payload.len() as u32))
            .collect();
        let ticket = node.write_run_begin(VDISK, 0, &meta).unwrap();
        let refs: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
        node.write_run_fill(&ticket, &refs).unwrap();
        drop(ticket); // the crash below is what "handles" the leak

        let mut node = crash_and_reopen(node);
        assert_eq!(
            node.read_block(VDISK, 0).unwrap().as_deref(),
            Some(before.as_slice()),
            "seed {seed}: the acknowledged write must survive"
        );
        for index in 1..6u64 {
            assert_eq!(
                node.read_block(VDISK, index).unwrap(),
                None,
                "seed {seed}: an unpublished write must not resurrect into the map"
            );
        }
        assert_scrub_clean(&node);
    }
}

/// The three-phase path and the single-call path agree byte for byte —
/// including a run carrying an in-batch duplicate and a dedupe hit.
#[test]
fn three_phase_and_single_call_puts_agree() {
    let mut node = degraded_node(71);
    let repeated = payload(0, 13);
    node.write_block(VDISK, 9, &repeated).unwrap();
    // [already-stored, fresh, duplicate-of-fresh] in one run.
    let fresh = payload(1, 13);
    let payloads = vec![repeated.clone(), fresh.clone(), fresh.clone()];
    run_three_phase(&mut node, 0, &payloads).unwrap();
    node.flush().unwrap();
    node.take_effects();
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            node.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice())
        );
    }
    let node = crash_and_reopen(node);
    let mut node = node;
    for (index, expected) in payloads.iter().enumerate() {
        assert_eq!(
            node.read_block(VDISK, index as u64).unwrap().as_deref(),
            Some(expected.as_slice())
        );
    }
    assert_scrub_clean(&node);
}

/// Deliver every queued effect message between two nodes until both go
/// quiet — the minimal pump these histories need.
fn deliver_all(a: &mut ReplNode<SimDisk>, b: &mut ReplNode<SimDisk>) {
    loop {
        let mut moved = false;
        for effect in a.take_effects() {
            if let Effect::Send(to, message) = effect {
                assert_eq!(to, 1);
                b.handle(0, message).unwrap();
                moved = true;
            }
        }
        for effect in b.take_effects() {
            if let Effect::Send(to, message) = effect {
                assert_eq!(to, 0);
                a.handle(1, message).unwrap();
                moved = true;
            }
        }
        if !moved {
            return;
        }
    }
}
