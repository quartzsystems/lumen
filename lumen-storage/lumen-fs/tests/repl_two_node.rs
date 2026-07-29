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

/// Drain effects and deliver messages until nothing moves.
fn pump(a: &mut ReplNode<SimDisk>, b: &mut ReplNode<SimDisk>, net: &mut Net, guests: &mut Guests) {
    loop {
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
        if let Some(message) = net.to_a.pop_front() {
            a.handle(message).unwrap();
            moved = true;
        }
        if let Some(message) = net.to_b.pop_front() {
            b.handle(message).unwrap();
            moved = true;
        }
        if !moved {
            break;
        }
    }
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

/// Pump until a batch of resync data lands on `a`, then stop — the shape
/// of a link that dies partway through a walk, leaving the target holding
/// an interior node whose children never arrived.
fn pump_until_sync_data_lands_on_a(
    a: &mut ReplNode<SimDisk>,
    b: &mut ReplNode<SimDisk>,
    net: &mut Net,
    guests: &mut Guests,
) -> bool {
    loop {
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
        if let Some(message) = net.to_a.pop_front() {
            let was_data = matches!(message, PeerMessage::SyncData(_));
            a.handle(message).unwrap();
            if was_data {
                return true;
            }
            moved = true;
        }
        if let Some(message) = net.to_b.pop_front() {
            b.handle(message).unwrap();
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
        let interrupted = pump_until_sync_data_lands_on_a(&mut a, &mut b, &mut net, &mut guests);
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
                // Half the time, cut the link once partway through the
                // walk before healing for real — a resync rarely gets an
                // uninterrupted network just because it would like one.
                if rng.chance(50)
                    && pump_until_sync_data_lands_on_a(&mut a, &mut b, &mut net, &mut guests)
                {
                    net.partition();
                    a.peer_lost();
                    b.peer_lost();
                    net.up = true;
                    a.connect();
                    b.connect();
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
