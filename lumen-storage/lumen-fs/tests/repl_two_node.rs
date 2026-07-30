//! The two-node replication suite: docs/lumenfs.md's phase-2 exit test,
//! run under the simulation instead of a pulled Core cable.
//!
//! The contract, held through every scenario:
//!
//! - a guest flush acknowledges only when both nodes hold the writes, or a
//!   fence verdict says one node is the only node;
//! - a partitioned pair without a verdict suspends — refuses writes,
//!   parks flushes — and never diverges;
//! - an acknowledged write survives the writer's death and is served by
//!   the survivor;
//! - a write never acknowledged may die with its writer, and does not
//!   resurrect onto the survivor;
//! - after every resync, the two pools are byte-identical for everything
//!   either would serve.
//!
//! The harness is the network: two message queues, an up/down switch, and
//! a pump that drains effects and delivers messages until the pair goes
//! quiet. Determinism throughout — a failure replays exactly.

use std::collections::{HashMap, HashSet, VecDeque};

use lumen_fs::{
    Brick, BrickParams, Effect, FsError, PeerMessage, Pool, ReplNode, ReplState, SimDisk,
    SplitMix64,
};

const KIB: u64 = 1024;
const BLOCK: usize = 4 * KIB as usize;
const VDISK: u64 = 1;
const CAPACITY: u64 = 60;
/// A second, larger vdisk for the streaming-resync tests: big enough that
/// a pull takes several SyncNeed round trips, so there is a real "middle"
/// for guests to keep writing through.
const VDISK2: u64 = 2;
const CAP2: u64 = 256;

/// A distinct, recognizable payload for `(index, generation)`.
fn payload(index: u64, generation: u8) -> Vec<u8> {
    let mut data = vec![0u8; 700];
    data[0..8].copy_from_slice(&index.to_le_bytes());
    data[8] = generation;
    data
}

fn params(id: u8) -> BrickParams {
    BrickParams {
        pool_uuid: [0xAA; 16],
        brick_uuid: [id; 16],
        block_size: BLOCK as u32,
        segment_size: 128 * KIB,
        wal_size: 32 * KIB,
    }
}

fn fresh_node(seed: u64, id: u8) -> ReplNode<SimDisk> {
    let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params(id)).unwrap();
    ReplNode::new(Pool::create(brick).unwrap(), id)
}

/// The wire: FIFO per direction, dropped whole on partition — TCP without
/// the ceremony.
#[derive(Default)]
struct Net {
    up: bool,
    to_a: VecDeque<PeerMessage>,
    to_b: VecDeque<PeerMessage>,
}

impl Net {
    fn partition(&mut self) {
        self.up = false;
        self.to_a.clear();
        self.to_b.clear();
    }
}

/// What the guests have been told.
#[derive(Default)]
struct Guests {
    done: [HashSet<u64>; 2],
    failed: [HashSet<u64>; 2],
}

/// Route both nodes' effects: sends onto the wire (if up), flush fates to
/// the guests. Returns whether anything moved.
fn drain_effects(
    a: &mut ReplNode<SimDisk>,
    b: &mut ReplNode<SimDisk>,
    net: &mut Net,
    guests: &mut Guests,
) -> bool {
    let mut moved = false;
    for (side, node) in [(0usize, &mut *a), (1usize, &mut *b)] {
        for effect in node.take_effects() {
            moved = true;
            match effect {
                Effect::Send(message) => {
                    if net.up {
                        if side == 0 {
                            net.to_b.push_back(message);
                        } else {
                            net.to_a.push_back(message);
                        }
                    }
                }
                Effect::FlushDone(ticket) => {
                    guests.done[side].insert(ticket);
                }
                Effect::FlushFailed(ticket) => {
                    guests.failed[side].insert(ticket);
                }
            }
        }
    }
    moved
}

/// One pump iteration: drain effects, then deliver at most one queued
/// message to each side. The fine grain is the point — a test can act
/// between deliveries, which is where a resync is mid-flight.
fn pump_step(
    a: &mut ReplNode<SimDisk>,
    b: &mut ReplNode<SimDisk>,
    net: &mut Net,
    guests: &mut Guests,
) -> bool {
    let mut moved = drain_effects(a, b, net, guests);
    if let Some(message) = net.to_a.pop_front() {
        a.handle(message).unwrap();
        moved = true;
    }
    if let Some(message) = net.to_b.pop_front() {
        b.handle(message).unwrap();
        moved = true;
    }
    moved
}

/// Drain effects and deliver messages until nothing moves.
fn pump(a: &mut ReplNode<SimDisk>, b: &mut ReplNode<SimDisk>, net: &mut Net, guests: &mut Guests) {
    while pump_step(a, b, net, guests) {}
}

/// Bring a fresh pair to Synced with the shared vdisk, A as writer.
fn synced_pair(seed: u64) -> (ReplNode<SimDisk>, ReplNode<SimDisk>, Net, Guests) {
    let mut a = fresh_node(seed, 0);
    let mut b = fresh_node(seed.wrapping_add(1000), 1);
    let mut net = Net {
        up: true,
        ..Net::default()
    };
    let mut guests = Guests::default();
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    a.create_vdisk(VDISK, CAPACITY * BLOCK as u64).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    (a, b, net, guests)
}

fn crash_and_restart(node: ReplNode<SimDisk>, id: u8) -> ReplNode<SimDisk> {
    let mut disk = node.into_pool().into_brick().into_disk();
    disk.crash();
    ReplNode::new(Pool::open(Brick::open(disk).unwrap()).unwrap(), id)
}

/// Every index either node would serve, compared byte for byte.
fn assert_identical(a: &ReplNode<SimDisk>, b: &ReplNode<SimDisk>) {
    for index in 0..CAPACITY {
        assert_eq!(
            a.read_block(VDISK, index).unwrap(),
            b.read_block(VDISK, index).unwrap(),
            "the nodes disagree at block {index}"
        );
    }
    assert_eq!(a.pool().era(), b.pool().era(), "the nodes disagree on era");
}

#[test]
fn a_flush_acknowledges_only_when_both_nodes_hold_the_write() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(1);
    a.write_block(VDISK, 3, b"two copies or no promise")
        .unwrap();
    let ticket = a.flush().unwrap();
    // Nothing delivered yet: the guest has no acknowledgement.
    assert!(!guests.done[0].contains(&ticket));
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert!(guests.done[0].contains(&ticket));
    // And the peer really holds it — not a courtesy ack.
    assert_eq!(
        b.read_block(VDISK, 3).unwrap().unwrap(),
        b"two copies or no promise"
    );
    // The peer holds it but does not own it.
    assert_eq!(
        b.write_block(VDISK, 3, b"mine now").unwrap_err(),
        FsError::NotWriter(VDISK)
    );
}

#[test]
fn an_acknowledged_write_survives_its_writers_death() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(2);
    a.write_block(VDISK, 0, b"must survive").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    // A dies. The cluster fences it; B fails over.
    net.partition();
    b.peer_lost();
    let a_disk = {
        let mut disk = a.into_pool().into_brick().into_disk();
        disk.crash();
        disk
    };
    b.set_peer_fenced().unwrap();
    b.claim_writer(VDISK).unwrap();
    assert_eq!(b.read_block(VDISK, 0).unwrap().unwrap(), b"must survive");

    // Degraded writes acknowledge alone.
    b.write_block(VDISK, 1, b"written alone").unwrap();
    let ticket = b.flush().unwrap();
    pump_one(&mut b, &mut guests, 1);
    assert!(guests.done[1].contains(&ticket));

    // A returns from its crashed disk, stale, and adopts.
    let mut a = ReplNode::new(Pool::open(Brick::open(a_disk).unwrap()).unwrap(), 0);
    net.up = true;
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    assert_eq!(a.read_block(VDISK, 0).unwrap().unwrap(), b"must survive");
    assert_eq!(a.read_block(VDISK, 1).unwrap().unwrap(), b"written alone");
    assert_identical(&a, &b);

    // Lockstep continues, with B the writer now.
    b.write_block(VDISK, 2, b"after the storm").unwrap();
    b.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.read_block(VDISK, 2).unwrap().unwrap(), b"after the storm");
}

/// Pump until a batch of resync data lands on one side, then stop — the
/// shape of a link that dies partway through a walk, leaving the target
/// holding an interior node whose children never arrived; also simply the
/// middle of a pull, where a test wants to act.
fn pump_until_sync_data_lands_on(
    target: usize,
    a: &mut ReplNode<SimDisk>,
    b: &mut ReplNode<SimDisk>,
    net: &mut Net,
    guests: &mut Guests,
) -> bool {
    loop {
        let mut moved = drain_effects(a, b, net, guests);
        if let Some(message) = net.to_a.pop_front() {
            let was_data = matches!(message, PeerMessage::SyncData(_));
            a.handle(message).unwrap();
            if was_data && target == 0 {
                return true;
            }
            moved = true;
        }
        if let Some(message) = net.to_b.pop_front() {
            let was_data = matches!(message, PeerMessage::SyncData(_));
            b.handle(message).unwrap();
            if was_data && target == 1 {
                return true;
            }
            moved = true;
        }
        if !moved {
            return false;
        }
    }
}

/// Drain one node's effects when its peer is gone.
fn pump_one(node: &mut ReplNode<SimDisk>, guests: &mut Guests, side: usize) {
    for effect in node.take_effects() {
        match effect {
            Effect::FlushDone(ticket) => {
                guests.done[side].insert(ticket);
            }
            Effect::FlushFailed(ticket) => {
                guests.failed[side].insert(ticket);
            }
            Effect::Send(_) => {}
        }
    }
}

#[test]
fn a_partition_without_a_verdict_suspends_and_never_diverges() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(3);
    a.write_block(VDISK, 5, b"before the cable").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    // A write leaves A, and the cable is pulled before it lands anywhere.
    a.write_block(VDISK, 6, b"caught mid-air").unwrap();
    let parked = a.flush().unwrap();
    net.partition();
    a.peer_lost();
    b.peer_lost();

    // No verdict: both refuse writes, the flush stays parked.
    assert_eq!(
        a.write_block(VDISK, 7, b"nope").unwrap_err(),
        FsError::Suspended
    );
    assert_eq!(
        b.write_block(VDISK, 7, b"nope").unwrap_err(),
        FsError::Suspended
    );
    pump_one(&mut a, &mut guests, 0);
    assert!(!guests.done[0].contains(&parked));
    assert!(!guests.failed[0].contains(&parked));

    // The cable comes back. No one died, both hold every acknowledged
    // write, and the reconciliation completes the parked flush honestly —
    // the write it covered now stands on both nodes.
    net.up = true;
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    assert!(guests.done[0].contains(&parked));
    assert_eq!(b.read_block(VDISK, 6).unwrap().unwrap(), b"caught mid-air");
    assert_identical(&a, &b);
}

#[test]
fn an_unacknowledged_write_dies_with_its_fenced_writer() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(4);
    a.write_block(VDISK, 0, b"acknowledged").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    // W is written and locally flushed on A — but never reaches B and no
    // guest ever hears an acknowledgement.
    a.write_block(VDISK, 9, b"never promised").unwrap();
    let ticket = a.flush().unwrap();
    assert!(!guests.done[0].contains(&ticket));

    // A dies with W on its platters. B is fenced-verdict survivor.
    net.partition();
    b.peer_lost();
    let a = crash_and_restart(a, 0);
    b.set_peer_fenced().unwrap();
    b.claim_writer(VDISK).unwrap();
    b.write_block(VDISK, 10, b"the survivor's history").unwrap();
    b.flush().unwrap();
    pump_one(&mut b, &mut guests, 1);

    // A rejoins carrying W locally; adoption discards it. The guest was
    // never lied to: no acknowledgement, no survival.
    let mut a = a;
    net.up = true;
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.read_block(VDISK, 9).unwrap(), None);
    assert_eq!(b.read_block(VDISK, 9).unwrap(), None);
    assert_eq!(
        a.read_block(VDISK, 10).unwrap().unwrap(),
        b"the survivor's history"
    );
    assert_identical(&a, &b);
}

#[test]
fn a_live_migration_hands_the_pen_over_without_ever_dropping_it() {
    // The two-primaries window, which is the one moment in a machine's
    // life when both nodes legitimately hold its disk open. Exactly one of
    // them may write at every instant of it.
    let (mut a, mut b, mut net, mut guests) = synced_pair(60);
    a.write_block(VDISK, 1, b"written before the move").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    // A opens the window toward B: the guest is still on A.
    a.begin_handover(VDISK, 1).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.lease(VDISK).unwrap().handing_to, Some(1));
    assert_eq!(
        b.lease(VDISK).unwrap().handing_to,
        Some(1),
        "the peer did not learn the window was open"
    );
    // Still A's disk: it writes, B does not.
    a.write_block(VDISK, 2, b"written during the window")
        .unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(
        b.write_block(VDISK, 3, b"not yours yet").unwrap_err(),
        FsError::NotWriter(VDISK)
    );

    // The instant of handover.
    b.accept_handover(VDISK).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(
        a.write_block(VDISK, 3, b"not yours any more").unwrap_err(),
        FsError::NotWriter(VDISK)
    );
    b.write_block(VDISK, 3, b"written after the move").unwrap();
    let ticket = b.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert!(guests.done[1].contains(&ticket));

    // Everything written on either side of the move is on both nodes, and
    // the move itself survives a crash of the node that now holds it.
    assert_identical(&a, &b);
    let b = crash_and_restart(b, 1);
    assert_eq!(b.lease(VDISK).unwrap().holder, 1, "the lease was forgotten");
    assert_eq!(
        b.read_block(VDISK, 2).unwrap().unwrap(),
        b"written during the window"
    );
}

#[test]
fn an_abandoned_migration_leaves_the_writer_where_it_started() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(61);
    a.begin_handover(VDISK, 1).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    a.abort_handover(VDISK).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    assert_eq!(b.lease(VDISK).unwrap().handing_to, None);
    assert_eq!(
        b.accept_handover(VDISK).unwrap_err(),
        FsError::NoSuchHandover(VDISK)
    );
    // A is still the writer, and still works.
    a.write_block(VDISK, 4, b"still mine").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(b.read_block(VDISK, 4).unwrap().unwrap(), b"still mine");
}

#[test]
fn a_failover_takes_a_lease_the_dead_node_still_held() {
    // The lease says A may write. A dies. B must be able to take it — a
    // lease that outlived its holder would make the vdisk unwritable at
    // exactly the moment somebody needs to rescue it.
    let (mut a, mut b, mut net, mut guests) = synced_pair(62);
    a.write_block(VDISK, 0, b"before the failure").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(b.lease(VDISK).unwrap().holder, 0);
    assert_eq!(
        b.write_block(VDISK, 1, b"not yet").unwrap_err(),
        FsError::NotWriter(VDISK)
    );

    net.partition();
    b.peer_lost();
    let a_disk = {
        let mut disk = a.into_pool().into_brick().into_disk();
        disk.crash();
        disk
    };
    b.set_peer_fenced().unwrap();
    b.claim_writer(VDISK).unwrap();
    b.write_block(VDISK, 1, b"rescued").unwrap();
    let ticket = b.flush().unwrap();
    pump_one(&mut b, &mut guests, 1);
    assert!(guests.done[1].contains(&ticket));

    // A comes back holding a lease from the era it died in, and must not
    // act on it: it adopts B's state, leases included.
    let mut a = ReplNode::new(Pool::open(Brick::open(a_disk).unwrap()).unwrap(), 0);
    net.up = true;
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(
        a.lease(VDISK).unwrap().holder,
        1,
        "the revenant kept the pen"
    );
    assert_eq!(
        a.write_block(VDISK, 2, b"i am still in charge")
            .unwrap_err(),
        FsError::NotWriter(VDISK)
    );
    assert_identical(&a, &b);
}

#[test]
fn a_survivors_own_lease_rides_through_its_own_verdict() {
    // The era bump retires the dead node's leases so a failover can claim
    // them — but the survivor's own guest is still running, and its next
    // write must not find the pen gone. Found by the daemon's integration
    // harness: the sim tests had always re-claimed by habit.
    let (mut a, mut b, mut net, mut guests) = synced_pair(80);
    a.write_block(VDISK, 0, b"mine before").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    net.partition();
    a.peer_lost();
    let _b_gone = crash_and_restart(b, 1);
    a.set_peer_fenced().unwrap();
    // No re-claim: the write must simply work.
    a.write_block(VDISK, 1, b"mine still").unwrap();
    let ticket = a.flush().unwrap();
    pump_one(&mut a, &mut guests, 0);
    assert!(guests.done[0].contains(&ticket));
    assert_eq!(a.lease(VDISK).unwrap().era, a.pool().era());
}

#[test]
fn a_resync_interrupted_partway_resumes_without_adopting_a_hole() {
    // The hazard content addressing tempts you into: a target that already
    // holds an interior node looks like a target that holds its whole
    // subtree. It is not, if an earlier walk was cut off after that node
    // arrived and before its children did — and adopting a root over that
    // hole is silent data loss discovered at the next read.
    for seed in 40..48u64 {
        let (mut a, mut b, mut net, mut guests) = synced_pair(seed);
        a.write_block(VDISK, 0, b"from before").unwrap();
        a.flush().unwrap();
        pump(&mut a, &mut b, &mut net, &mut guests);

        // A dies; B survives under a verdict and writes a whole tree's
        // worth of divergent history.
        net.partition();
        b.peer_lost();
        let a_down = crash_and_restart(a, 0);
        b.set_peer_fenced().unwrap();
        b.claim_writer(VDISK).unwrap();
        for index in 0..CAPACITY {
            let mut data = vec![0u8; 800];
            data[0..8].copy_from_slice(&index.to_le_bytes());
            b.write_block(VDISK, index, &data).unwrap();
        }
        b.flush().unwrap();
        pump_one(&mut b, &mut guests, 1);

        // A returns and starts pulling — and the link dies the moment the
        // first batch lands, stranding a partial subtree on A.
        let mut a = a_down;
        net.up = true;
        a.connect();
        b.connect();
        let interrupted = pump_until_sync_data_lands_on(0, &mut a, &mut b, &mut net, &mut guests);
        assert!(interrupted, "seed {seed}: the pull never carried data");
        net.partition();
        a.peer_lost();
        b.peer_lost();
        assert_eq!(a.state(), ReplState::Suspended);

        // Heal. The second walk must notice what the first one left
        // missing and fetch it, rather than trusting a held node.
        net.up = true;
        a.connect();
        b.connect();
        pump(&mut a, &mut b, &mut net, &mut guests);
        assert_eq!(a.state(), ReplState::Synced, "seed {seed}");
        assert_eq!(b.state(), ReplState::Synced, "seed {seed}");
        for index in 0..CAPACITY {
            let mut expected = vec![0u8; 800];
            expected[0..8].copy_from_slice(&index.to_le_bytes());
            assert_eq!(
                a.read_block(VDISK, index).unwrap().as_deref(),
                Some(expected.as_slice()),
                "seed {seed}: block {index} came back holed"
            );
        }
        assert_identical(&a, &b);
        // And the pool agrees with itself about every reference it holds.
        let report = a.pool().scrub().unwrap();
        assert_eq!(report.corrupt, vec![], "seed {seed}");
        assert_eq!(report.missing, vec![], "seed {seed}");
    }
}

/// Bring the pair to: A dead and stale, B a fenced-verdict survivor at era
/// 2 holding VDISK plus a fully-written VDISK2 — the state every streaming
/// test starts a resync from.
fn survivor_with_history(seed: u64) -> (ReplNode<SimDisk>, ReplNode<SimDisk>, Net, Guests) {
    let (a, mut b, mut net, mut guests) = synced_pair(seed);
    net.partition();
    b.peer_lost();
    let a_down = crash_and_restart(a, 0);
    b.set_peer_fenced().unwrap();
    b.claim_writer(VDISK).unwrap();
    b.create_vdisk(VDISK2, CAP2 * BLOCK as u64).unwrap();
    for index in 0..CAP2 {
        b.write_block(VDISK2, index, &payload(index, 1)).unwrap();
    }
    b.flush().unwrap();
    pump_one(&mut b, &mut guests, 1);
    (a_down, b, net, guests)
}

#[test]
fn a_survivor_keeps_serving_guests_while_its_peer_resyncs() {
    // The whole point of the streaming resync: a returning node must never
    // freeze the guests on the healthy one. B serves and acknowledges
    // through the entire pull; the one exception is the single round trip
    // at the very end, after B has agreed to let A adopt.
    let (a_down, mut b, mut net, mut guests) = survivor_with_history(70);
    let mut a = a_down;
    net.up = true;
    a.connect();
    b.connect();

    let mut acked_mid_resync: u64 = 0;
    let mut refused: u64 = 0;
    loop {
        let moved = pump_step(&mut a, &mut b, &mut net, &mut guests);
        if b.state() == (ReplState::Resyncing { source: true }) {
            let index = acked_mid_resync % CAP2;
            if b.accepts_writes() {
                // A guest write in the middle of the pull: accepted, and
                // acknowledged without waiting for anything remote.
                b.write_block(VDISK2, index, &payload(index, 2)).unwrap();
                let ticket = b.flush().unwrap();
                drain_effects(&mut a, &mut b, &mut net, &mut guests);
                assert!(
                    guests.done[1].contains(&ticket),
                    "a mid-resync flush kept a guest waiting"
                );
                acked_mid_resync += 1;
                continue;
            }
            // The closing window: the target asked to adopt, so writes
            // refuse — honestly, with Suspended — until lockstep resumes.
            assert_eq!(
                b.write_block(VDISK2, index, &payload(index, 2))
                    .unwrap_err(),
                FsError::Suspended
            );
            refused += 1;
        }
        if !moved {
            break;
        }
    }
    assert!(
        acked_mid_resync >= 10,
        "the resync never really overlapped guest writes ({acked_mid_resync} acked)"
    );
    assert!(
        refused >= 1,
        "the closing window never refused a write, so it was never entered"
    );
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);

    // The target converged to the source's LIVE state: the offer plus
    // everything written during the pull.
    for index in 0..CAP2 {
        let generation = if index < acked_mid_resync { 2 } else { 1 };
        let expected = payload(index, generation);
        for node in [&a, &b] {
            assert_eq!(
                node.read_block(VDISK2, index).unwrap().as_deref(),
                Some(expected.as_slice()),
                "block {index} did not carry generation {generation}"
            );
        }
    }
    assert_identical(&a, &b);
    for node in [&a, &b] {
        let report = node.pool().scrub().unwrap();
        assert_eq!(report.corrupt, vec![]);
        assert_eq!(report.missing, vec![]);
    }

    // And lockstep really resumed on the stream's numbering.
    b.write_block(VDISK2, 0, &payload(0, 3)).unwrap();
    let ticket = b.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert!(guests.done[1].contains(&ticket));
    assert_eq!(
        a.read_block(VDISK2, 0).unwrap().as_deref(),
        Some(payload(0, 3).as_slice())
    );
}

#[test]
fn an_equal_era_source_still_suspends_its_guests_for_the_diff() {
    // A tie has no fence verdict, so there is no honest way to acknowledge
    // a write single-copy — the source suspends for the (cheap) diff,
    // exactly as before streaming existed.
    let (mut a, mut b, mut net, mut guests) = synced_pair(71);
    a.write_block(VDISK, 3, b"shared history").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);

    net.partition();
    a.peer_lost();
    b.peer_lost();
    net.up = true;
    a.connect();
    b.connect();
    while a.state() != (ReplState::Resyncing { source: true }) {
        assert!(
            pump_step(&mut a, &mut b, &mut net, &mut guests),
            "the pair never reached the tie diff"
        );
    }
    assert!(!a.accepts_writes());
    assert_eq!(
        a.write_block(VDISK, 4, b"no verdict, no promise")
            .unwrap_err(),
        FsError::Suspended
    );
    let parked = a.flush().unwrap();

    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    assert!(guests.done[0].contains(&parked));
    assert_identical(&a, &b);
}

#[test]
fn a_source_that_dies_mid_stream_cannot_tie_with_the_survivors_next_era() {
    // A sources at era 2 with A the lower node id, dies mid-pull, and B
    // survives on a verdict. B's bump must clear the era it was *adopting*
    // — its own era is still 1 — or it would mint a second era 2, the next
    // hello would tie, and the tie-break (lower id: the dead node) would
    // adopt away B's post-verdict acknowledged writes.
    let (mut a, b, mut net, mut guests) = synced_pair(72);
    net.partition();
    a.peer_lost();
    let b_down = crash_and_restart(b, 1);
    a.set_peer_fenced().unwrap();
    a.claim_writer(VDISK).unwrap();
    a.create_vdisk(VDISK2, CAP2 * BLOCK as u64).unwrap();
    for index in 0..CAP2 {
        a.write_block(VDISK2, index, &payload(index, 1)).unwrap();
    }
    a.flush().unwrap();
    pump_one(&mut a, &mut guests, 0);
    assert_eq!(a.pool().era(), 2);

    // B returns and pulls — and A dies partway through the walk.
    let mut b = b_down;
    net.up = true;
    a.connect();
    b.connect();
    assert!(
        pump_until_sync_data_lands_on(1, &mut a, &mut b, &mut net, &mut guests),
        "the pull never carried data"
    );
    net.partition();
    b.peer_lost();
    let a_down = crash_and_restart(a, 0);

    // The cluster fences A; B continues alone. Its era must be 3, not 2.
    b.set_peer_fenced().unwrap();
    assert_eq!(
        b.pool().era(),
        3,
        "the bump did not clear the witnessed era"
    );
    b.claim_writer(VDISK).unwrap();
    b.write_block(VDISK, 5, b"the second survivor's promise")
        .unwrap();
    let ticket = b.flush().unwrap();
    pump_one(&mut b, &mut guests, 1);
    assert!(guests.done[1].contains(&ticket));

    // A returns; the eras order the lineages and B's history wins. A's own
    // era-2 writes are gone — the cluster fenced the only node that held
    // them, and the floor makes that loss ordered and visible rather than
    // a silent tie resolved toward a dead node's disk.
    let mut a = a_down;
    net.up = true;
    a.connect();
    b.connect();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    assert_eq!(a.pool().era(), 3);
    assert_eq!(
        a.read_block(VDISK, 5).unwrap().unwrap(),
        b"the second survivor's promise"
    );
    assert!(
        a.pool().vdisk_size(VDISK2).is_err(),
        "the dead lineage's vdisk survived adoption"
    );
    assert_identical(&a, &b);
}

#[test]
fn a_collection_on_either_end_cannot_eat_a_resync_in_flight() {
    // Both ends keep collecting while a pull runs. The source has moved
    // past its offer — every index rewritten and checkpointed — so without
    // the pins its collection would sweep the very trees the target is
    // fetching; the target's fetched fragment is referenced by nothing in
    // its own manifest at all.
    let (a_down, mut b, mut net, mut guests) = survivor_with_history(73);
    let mut a = a_down;
    net.up = true;
    a.connect();
    b.connect();
    assert!(
        pump_until_sync_data_lands_on(0, &mut a, &mut b, &mut net, &mut guests),
        "the pull never carried data"
    );

    // The offer becomes history on the source...
    for index in 0..CAP2 {
        b.write_block(VDISK2, index, &payload(index, 2)).unwrap();
    }
    b.flush().unwrap();
    b.checkpoint().unwrap();
    // ...and both ends collect, mid-pull.
    b.collect_garbage().unwrap();
    a.collect_garbage().unwrap();

    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.state(), ReplState::Synced);
    assert_eq!(b.state(), ReplState::Synced);
    for index in 0..CAP2 {
        let expected = payload(index, 2);
        for node in [&a, &b] {
            assert_eq!(
                node.read_block(VDISK2, index).unwrap().as_deref(),
                Some(expected.as_slice()),
                "block {index} lost the post-offer overwrite"
            );
        }
    }
    assert_identical(&a, &b);
    for node in [&a, &b] {
        let report = node.pool().scrub().unwrap();
        assert_eq!(report.corrupt, vec![]);
        assert_eq!(report.missing, vec![]);
    }
}

#[test]
fn snapshots_and_rollback_replicate_in_lockstep() {
    let (mut a, mut b, mut net, mut guests) = synced_pair(5);
    a.write_block(VDISK, 2, b"worth keeping").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    a.snapshot_vdisk(VDISK, 7).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(
        b.read_snapshot_block(VDISK, 7, 2).unwrap().unwrap(),
        b"worth keeping"
    );

    a.write_block(VDISK, 2, b"a regrettable change").unwrap();
    a.flush().unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    a.rollback_vdisk(VDISK, 7).unwrap();
    pump(&mut a, &mut b, &mut net, &mut guests);
    assert_eq!(a.read_block(VDISK, 2).unwrap().unwrap(), b"worth keeping");
    assert_eq!(b.read_block(VDISK, 2).unwrap().unwrap(), b"worth keeping");
    assert_identical(&a, &b);
}

/// The macro-event history: failovers, partitions, crashes, snapshots —
/// seeded, with the model asserting acknowledged-survives and full
/// convergence at every stable point.
#[test]
fn every_history_of_failovers_and_partitions_converges_without_losing_acks() {
    for seed in 0..8u64 {
        run_history(seed);
    }
}

fn run_history(seed: u64) {
    let mut rng = SplitMix64::new(seed.wrapping_mul(0x00AB_CDEF).wrapping_add(7));
    let (mut a, mut b, mut net, mut guests) = synced_pair(9000 + seed);
    // Which node currently holds the writer role.
    let mut writer: usize = 0;
    // What the guests were promised: index → payload.
    let mut acked: HashMap<u64, Vec<u8>> = HashMap::new();

    for event in 0..14 {
        match rng.next_below(10) {
            // Acknowledged write: write, flush, pump to completion.
            0..=4 => {
                let index = rng.next_below(CAPACITY);
                let mut data = vec![0u8; 1 + rng.next_below(BLOCK as u64) as usize];
                rng.fill(&mut data);
                let node = if writer == 0 { &mut a } else { &mut b };
                node.write_block(VDISK, index, &data).unwrap();
                let ticket = node.flush().unwrap();
                pump(&mut a, &mut b, &mut net, &mut guests);
                assert!(
                    guests.done[writer].contains(&ticket),
                    "seed {seed} event {event}: a synced flush never completed"
                );
                acked.insert(index, data);
            }
            // Failover: the writer dies unacknowledged-rich, the survivor
            // takes over, the dead node returns and adopts.
            5..=6 => {
                // An unacknowledged write that may die with the writer.
                let doomed_index = rng.next_below(CAPACITY);
                let node = if writer == 0 { &mut a } else { &mut b };
                node.write_block(VDISK, doomed_index, b"maybe doomed")
                    .unwrap();
                node.flush().unwrap();

                net.partition();
                let survivor = 1 - writer;
                if writer == 0 {
                    b.peer_lost();
                    a = crash_and_restart(a, 0);
                    b.set_peer_fenced().unwrap();
                    b.claim_writer(VDISK).unwrap();
                } else {
                    a.peer_lost();
                    b = crash_and_restart(b, 1);
                    a.set_peer_fenced().unwrap();
                    a.claim_writer(VDISK).unwrap();
                }
                writer = survivor;
                net.up = true;
                a.connect();
                b.connect();
                // Half the time, catch the resync mid-pull. The survivor
                // is a serving source: a guest write in the middle of the
                // walk must be accepted and acknowledged on the spot. And
                // sometimes the link then dies again before healing for
                // real — a resync rarely gets an uninterrupted network
                // just because it would like one — in which case that
                // acknowledged write must ride the next offer instead of
                // the stream that died.
                let target = 1 - writer;
                if rng.chance(50)
                    && pump_until_sync_data_lands_on(target, &mut a, &mut b, &mut net, &mut guests)
                {
                    let index = rng.next_below(CAPACITY);
                    let mut data = vec![0u8; 1 + rng.next_below(BLOCK as u64) as usize];
                    rng.fill(&mut data);
                    let node = if writer == 0 { &mut a } else { &mut b };
                    if node.accepts_writes() {
                        node.write_block(VDISK, index, &data).unwrap();
                        let ticket = node.flush().unwrap();
                        drain_effects(&mut a, &mut b, &mut net, &mut guests);
                        assert!(
                            guests.done[writer].contains(&ticket),
                            "seed {seed} event {event}: a mid-resync flush kept a guest waiting"
                        );
                        acked.insert(index, data);
                    }
                    if rng.chance(50) {
                        net.partition();
                        a.peer_lost();
                        b.peer_lost();
                        net.up = true;
                        a.connect();
                        b.connect();
                    }
                }
                pump(&mut a, &mut b, &mut net, &mut guests);
                assert_eq!(a.state(), ReplState::Synced, "seed {seed} event {event}");
                assert_eq!(b.state(), ReplState::Synced, "seed {seed} event {event}");
            }
            // A clean partition and heal: nothing acknowledged in between,
            // nothing may change.
            7 => {
                net.partition();
                a.peer_lost();
                b.peer_lost();
                net.up = true;
                a.connect();
                b.connect();
                pump(&mut a, &mut b, &mut net, &mut guests);
            }
            // Local maintenance on both, independently.
            8 => {
                a.checkpoint().unwrap();
                b.checkpoint().unwrap();
            }
            // A snapshot on the writer, replicated.
            _ => {
                let snap = 1000 + event as u64;
                let node = if writer == 0 { &mut a } else { &mut b };
                node.snapshot_vdisk(VDISK, snap).unwrap();
                pump(&mut a, &mut b, &mut net, &mut guests);
            }
        }

        // The invariants, at every stable point. The model is exact:
        // acknowledged writes are always fully pumped, and unacknowledged
        // ones always die with their writer's adoption — so every acked
        // index must read back byte-identical, everywhere.
        assert_identical(&a, &b);
        for (index, data) in &acked {
            assert_eq!(
                a.read_block(VDISK, *index).unwrap().as_ref(),
                Some(data),
                "seed {seed} event {event}: acknowledged block {index} regressed"
            );
        }
    }
}
