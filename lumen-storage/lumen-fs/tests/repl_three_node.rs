//! The three-member suite: phase 5's placement, fetched reads, and
//! multi-source rejoin, under the same deterministic harness discipline
//! as the two-node suite.
//!
//! The contract, held through every scenario:
//!
//! - a data block lives on exactly its slice's two homes, and metadata —
//!   map trees, manifests, leases — lives on every member;
//! - a write acknowledges on its two data homes, and a member that is
//!   home to nothing in a flush gates nothing;
//! - every member serves every read — a non-home fetches on demand and
//!   keeps nothing;
//! - an acknowledged write survives any single member's death, and a
//!   returning member adopts each vdisk from its lease holder's offer —
//!   no single peer speaks for ops another writer originated;
//! - eras agree across live members after every verdict and every rejoin.
//!
//! The harness is the network: one FIFO queue per ordered pair — the
//! delivery-order guarantee payload-before-op safety rides on — and a
//! pump that drains addressed effects until the trio goes quiet.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use lumen_fs::{
    Brick, BrickParams, BrickSet, Effect, FsError, PeerMessage, Pool, ReplNode, ReplState, SimDisk,
    SliceMap, SplitMix64,
};

const KIB: u64 = 1024;
const BLOCK: usize = 4 * KIB as usize;
const VDISK: u64 = 1;
const CAPACITY: u64 = 60;
const MEMBERS: [u8; 3] = [0, 1, 2];

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
        tier: 0,
        wal_holder: true,
    }
}

fn fresh_node(seed: u64, id: u8) -> ReplNode<SimDisk> {
    fresh_node_of(seed, id, &MEMBERS)
}

fn fresh_node_of(seed: u64, id: u8, members: &[u8]) -> ReplNode<SimDisk> {
    let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params(id)).unwrap();
    let mut node = ReplNode::new(Pool::create(brick).unwrap(), id);
    node.set_placement(members).unwrap();
    node
}

/// Reopen a crashed member's disk with its seat handed in at open — WAL
/// replay's home-awareness depends on it, which is exactly what this
/// exercises.
fn reopen_placed(mut disk: SimDisk, id: u8) -> ReplNode<SimDisk> {
    disk.crash();
    let set = BrickSet::single(Brick::open(disk).unwrap()).unwrap();
    let map = SliceMap::for_members(1, &MEMBERS).unwrap();
    ReplNode::new(Pool::open_set_placed(set, id, map).unwrap(), id)
}

/// The wire: one FIFO per ordered pair, and a set of dead members whose
/// links drop everything.
#[derive(Default)]
struct Net {
    queues: BTreeMap<(u8, u8), VecDeque<PeerMessage>>,
    down: HashSet<u8>,
}

impl Net {
    fn kill(&mut self, member: u8) {
        self.down.insert(member);
        self.queues
            .retain(|(from, to), _| *from != member && *to != member);
    }

    fn revive(&mut self, member: u8) {
        self.down.remove(&member);
    }
}

/// What the guests have been told, per member.
#[derive(Default)]
struct Guests {
    done: Vec<HashSet<u64>>,
    failed: Vec<HashSet<u64>>,
    reads_done: Vec<HashSet<u64>>,
    reads_failed: Vec<HashSet<u64>>,
}

impl Guests {
    fn new() -> Guests {
        Guests {
            done: vec![HashSet::new(); 3],
            failed: vec![HashSet::new(); 3],
            reads_done: vec![HashSet::new(); 3],
            reads_failed: vec![HashSet::new(); 3],
        }
    }
}

fn drain_effects(nodes: &mut [ReplNode<SimDisk>; 3], net: &mut Net, guests: &mut Guests) -> bool {
    let mut moved = false;
    for (i, node) in nodes.iter_mut().enumerate() {
        let from = node.node();
        for effect in node.take_effects() {
            moved = true;
            match effect {
                Effect::Send(to, message) => {
                    if !net.down.contains(&from) && !net.down.contains(&to) {
                        net.queues.entry((from, to)).or_default().push_back(message);
                    }
                }
                Effect::FlushDone(ticket) => {
                    guests.done[i].insert(ticket);
                }
                Effect::FlushFailed(ticket) => {
                    guests.failed[i].insert(ticket);
                }
                Effect::ReadDone(ticket) => {
                    guests.reads_done[i].insert(ticket);
                }
                Effect::ReadFailed(ticket) => {
                    guests.reads_failed[i].insert(ticket);
                }
            }
        }
    }
    moved
}

/// One pump iteration: drain, then deliver at most one message per
/// ordered pair, in pair order — deterministic, and fine-grained enough
/// for a test to act mid-resync.
fn pump_step(nodes: &mut [ReplNode<SimDisk>; 3], net: &mut Net, guests: &mut Guests) -> bool {
    let mut moved = drain_effects(nodes, net, guests);
    let pairs: Vec<(u8, u8)> = net.queues.keys().copied().collect();
    for pair in pairs {
        let Some(queue) = net.queues.get_mut(&pair) else {
            continue;
        };
        if let Some(message) = queue.pop_front() {
            let (from, to) = pair;
            nodes[to as usize].handle(from, message).unwrap();
            moved = true;
        }
    }
    moved
}

fn pump(nodes: &mut [ReplNode<SimDisk>; 3], net: &mut Net, guests: &mut Guests) {
    while pump_step(nodes, net, guests) {}
}

/// Read a block from one member, fetching across the wire when the member
/// is not a home. This is the daemon's read loop, worn by the harness.
fn read_anywhere(
    nodes: &mut [ReplNode<SimDisk>; 3],
    net: &mut Net,
    guests: &mut Guests,
    who: usize,
    vdisk: u64,
    index: u64,
) -> Option<Vec<u8>> {
    for _ in 0..8 {
        match nodes[who].read_block(vdisk, index) {
            Ok(answer) => return answer,
            Err(FsError::BlockElsewhere { tier, hash }) => {
                nodes[who].fetch_block(tier, hash).unwrap();
                pump(nodes, net, guests);
            }
            Err(err) => panic!("member {who} could not read {vdisk}/{index}: {err}"),
        }
    }
    panic!("member {who} fetched {vdisk}/{index} eight times and never landed it");
}

/// Bring a fresh trio to Synced with the shared vdisk, member 0 as writer.
fn synced_trio(seed: u64) -> ([ReplNode<SimDisk>; 3], Net, Guests) {
    let mut nodes = [
        fresh_node(seed, 0),
        fresh_node(seed.wrapping_add(1000), 1),
        fresh_node(seed.wrapping_add(2000), 2),
    ];
    let mut net = Net::default();
    let mut guests = Guests::new();
    for i in 0..3u8 {
        for j in 0..3u8 {
            if i != j {
                nodes[i as usize].connect(j);
            }
        }
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }
    nodes[0]
        .create_vdisk(VDISK, CAPACITY * BLOCK as u64, 0)
        .unwrap();
    pump(&mut nodes, &mut net, &mut guests);
    (nodes, net, guests)
}

/// The verdict every survivor adopts: one number, computed from every
/// survivor's floor — the layer above the engine owns this at runtime,
/// and the harness plays that layer.
fn agreed_era(nodes: &[ReplNode<SimDisk>; 3], survivors: &[usize]) -> u64 {
    survivors
        .iter()
        .map(|i| nodes[*i].era_target())
        .max()
        .unwrap()
}

/// Kill one member: links drop, survivors hear the loss and then the
/// verdict, at one agreed era.
fn fence(nodes: &mut [ReplNode<SimDisk>; 3], net: &mut Net, victim: usize, survivors: &[usize]) {
    net.kill(victim as u8);
    for i in survivors {
        nodes[*i].peer_lost(victim as u8);
    }
    let era = agreed_era(nodes, survivors);
    for i in survivors {
        nodes[*i].set_member_fenced(victim as u8, era).unwrap();
    }
}

fn homes_of(payload: &[u8]) -> [u8; 2] {
    let map = SliceMap::for_members(1, &MEMBERS).unwrap();
    map.homes_of(&lumen_fs::hash_block(payload))
}

// ---------------------------------------------------------------------------

#[test]
fn data_lands_on_its_homes_and_metadata_lands_everywhere() {
    let (mut nodes, mut net, mut guests) = synced_trio(1);
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    let mut saw_non_home = false;
    for index in 0..CAPACITY {
        let data = payload(index, 1);
        let hash = lumen_fs::hash_block(&data);
        let homes = homes_of(&data);
        for member in MEMBERS {
            let holds = nodes[member as usize].pool().has_block(0, &hash);
            assert_eq!(
                holds,
                homes.contains(&member),
                "block {index} is misplaced on member {member}: homes {homes:?}"
            );
        }
        if !homes.contains(&0) {
            saw_non_home = true;
        }
    }
    assert!(
        saw_non_home,
        "sixty blocks never left the writer's homes — the map is not spreading"
    );

    // Metadata everywhere: identical mappings fold to identical roots on
    // every member, homes or not.
    let mut roots = Vec::new();
    for node in &mut nodes {
        node.checkpoint().unwrap();
        let (_, vdisks, _) = node.pool().sync_manifest();
        roots.push(vdisks[0].3.expect("checkpointed"));
    }
    assert_eq!(roots[0], roots[1], "map roots diverged");
    assert_eq!(roots[1], roots[2], "map roots diverged");
}

#[test]
fn every_member_serves_every_read_and_a_non_home_keeps_nothing() {
    let (mut nodes, mut net, mut guests) = synced_trio(2);
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    for member in 0..3 {
        for index in 0..CAPACITY {
            let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, index);
            assert_eq!(
                found.as_deref(),
                Some(payload(index, 1).as_slice()),
                "member {member} read block {index} wrong"
            );
        }
    }

    // "Keeps nothing": after serving every read, each data block still
    // lives on exactly its two homes.
    for index in 0..CAPACITY {
        let data = payload(index, 1);
        let hash = lumen_fs::hash_block(&data);
        let homes = homes_of(&data);
        for member in MEMBERS {
            assert_eq!(
                nodes[member as usize].pool().has_block(0, &hash),
                homes.contains(&member),
                "a fetched read left a copy of block {index} on member {member}"
            );
        }
    }
}

#[test]
fn a_flush_waits_on_the_blocks_homes_and_not_on_the_third() {
    let (mut nodes, mut net, mut guests) = synced_trio(3);
    // Settle the create's metadata first — metadata ops owe every member,
    // and an unflushed create would put the third member into this
    // flush's needs for exactly the right reason.
    let settle = nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);
    assert!(guests.done[0].contains(&settle));
    // A block whose homes are the writer and exactly one peer.
    let (index, data, other_home, bystander) = (0..CAPACITY)
        .find_map(|index| {
            let data = payload(index, 1);
            let homes = homes_of(&data);
            if homes.contains(&0) {
                let other = homes.iter().copied().find(|h| *h != 0).unwrap();
                let bystander = MEMBERS
                    .iter()
                    .copied()
                    .find(|m| !homes.contains(m))
                    .unwrap();
                Some((index, data, other, bystander))
            } else {
                None
            }
        })
        .expect("sixty blocks include one homed on the writer");

    nodes[0].write_block(VDISK, index, &data).unwrap();
    let ticket = nodes[0].flush().unwrap();
    drain_effects(&mut nodes, &mut net, &mut guests);

    // Deliver only the co-home's traffic, both directions; the bystander
    // hears nothing at all.
    loop {
        let mut moved = false;
        for pair in [(0u8, other_home), (other_home, 0u8)] {
            if let Some(queue) = net.queues.get_mut(&pair) {
                if let Some(message) = queue.pop_front() {
                    nodes[pair.1 as usize].handle(pair.0, message).unwrap();
                    moved = true;
                }
            }
        }
        if !drain_effects(&mut nodes, &mut net, &mut guests) && !moved {
            break;
        }
    }
    assert!(
        guests.done[0].contains(&ticket),
        "a data flush waited on a member that is home to nothing in it"
    );
    // The bystander's queue still holds its undelivered copy of the op —
    // the stream is dense to everyone even when durability is not owed.
    assert!(
        net.queues
            .get(&(0, bystander))
            .is_some_and(|q| !q.is_empty()),
        "the op stream skipped the non-home member"
    );
    pump(&mut nodes, &mut net, &mut guests);
}

#[test]
fn an_acknowledged_write_survives_any_single_members_death() {
    for victim in 0..3usize {
        let (mut nodes, mut net, mut guests) = synced_trio(10 + victim as u64);
        for index in 0..CAPACITY {
            nodes[0]
                .write_block(VDISK, index, &payload(index, 1))
                .unwrap();
        }
        nodes[0].flush().unwrap();
        pump(&mut nodes, &mut net, &mut guests);

        let survivors: Vec<usize> = (0..3).filter(|i| *i != victim).collect();
        let disk = {
            let node = std::mem::replace(&mut nodes[victim], fresh_node(999, victim as u8));
            node.into_pool().into_brick().into_disk()
        };
        fence(&mut nodes, &mut net, victim, &survivors);

        // The writer keeps going — or a survivor claims, if the writer died.
        let writer = if victim == 0 { survivors[0] } else { 0 };
        if victim == 0 {
            nodes[writer].claim_writer(VDISK).unwrap();
        }
        for index in 0..CAPACITY / 2 {
            nodes[writer]
                .write_block(VDISK, index, &payload(index, 2))
                .unwrap();
        }
        let ticket = nodes[writer].flush().unwrap();
        pump(&mut nodes, &mut net, &mut guests);
        assert!(
            guests.done[writer].contains(&ticket),
            "victim {victim}: the survivors could not acknowledge"
        );

        // Every acknowledged write readable from both survivors, fetched
        // where a dead home makes that the only way.
        for index in 0..CAPACITY {
            let generation = if index < CAPACITY / 2 { 2 } else { 1 };
            for member in &survivors {
                let data = payload(index, generation);
                let homes = homes_of(&data);
                // A block whose only live home is the *other* survivor
                // fetches; one whose homes are both dead cannot be read —
                // but at three members with one death every slice keeps a
                // live home, so everything answers.
                let _ = homes;
                let found = read_anywhere(&mut nodes, &mut net, &mut guests, *member, VDISK, index);
                assert_eq!(
                    found.as_deref(),
                    Some(payload(index, generation).as_slice()),
                    "victim {victim}: member {member} lost block {index}"
                );
            }
        }

        // The victim returns, pulls from both survivors, and converges.
        nodes[victim] = reopen_placed(disk, victim as u8);
        net.revive(victim as u8);
        for other in 0..3u8 {
            if other as usize != victim {
                nodes[victim].connect(other);
                nodes[other as usize].connect(victim as u8);
            }
        }
        pump(&mut nodes, &mut net, &mut guests);
        for node in &nodes {
            assert_eq!(node.state(), ReplState::Synced, "victim {victim}");
        }
        let eras: Vec<u64> = nodes.iter().map(|n| n.pool().era()).collect();
        assert_eq!(eras[0], eras[1], "victim {victim}: eras diverged");
        assert_eq!(eras[1], eras[2], "victim {victim}: eras diverged");
        for index in 0..CAPACITY {
            let generation = if index < CAPACITY / 2 { 2 } else { 1 };
            for member in 0..3 {
                let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, index);
                assert_eq!(
                    found.as_deref(),
                    Some(payload(index, generation).as_slice()),
                    "victim {victim}: member {member} disagrees at {index} after rejoin"
                );
            }
        }
        // And each home holds exactly its share again.
        for index in 0..CAPACITY {
            let generation = if index < CAPACITY / 2 { 2 } else { 1 };
            let data = payload(index, generation);
            let hash = lumen_fs::hash_block(&data);
            let homes = homes_of(&data);
            for member in MEMBERS {
                assert_eq!(
                    nodes[member as usize].pool().has_block(0, &hash),
                    homes.contains(&member),
                    "victim {victim}: block {index} misplaced on {member} after rejoin"
                );
            }
        }
    }
}

#[test]
fn a_rejoin_takes_each_vdisks_truth_from_its_own_holder() {
    // Two writers on two vdisks; the third member dies and returns. Its
    // rejoin pulls from both survivors concurrently, and each vdisk's
    // truth comes from its holder's offer — a whole-state adoption from
    // either single source would hand back that source's stale copy of
    // the other's vdisk.
    let (mut nodes, mut net, mut guests) = synced_trio(30);
    const VDISK_B: u64 = 2;
    nodes[1]
        .create_vdisk(VDISK_B, CAPACITY * BLOCK as u64, 0)
        .unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    // Node 0 holds VDISK, node 1 holds VDISK_B. Node 2 dies.
    let disk = {
        let node = std::mem::replace(&mut nodes[2], fresh_node(999, 2));
        node.into_pool().into_brick().into_disk()
    };
    fence(&mut nodes, &mut net, 2, &[0, 1]);

    // Both writers advance their own vdisks while the third is away.
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 5))
            .unwrap();
        nodes[1]
            .write_block(VDISK_B, index, &payload(index, 6))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    nodes[1].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    nodes[2] = reopen_placed(disk, 2);
    net.revive(2);
    for other in [0u8, 1u8] {
        nodes[2].connect(other);
        nodes[other as usize].connect(2);
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }

    for index in 0..CAPACITY {
        let a = read_anywhere(&mut nodes, &mut net, &mut guests, 2, VDISK, index);
        assert_eq!(
            a.as_deref(),
            Some(payload(index, 5).as_slice()),
            "the rejoiner's copy of the first writer's vdisk is stale at {index}"
        );
        let b = read_anywhere(&mut nodes, &mut net, &mut guests, 2, VDISK_B, index);
        assert_eq!(
            b.as_deref(),
            Some(payload(index, 6).as_slice()),
            "the rejoiner's copy of the second writer's vdisk is stale at {index}"
        );
    }
}

#[test]
fn a_vdisk_deleted_while_a_member_was_away_stays_deleted() {
    let (mut nodes, mut net, mut guests) = synced_trio(40);
    const VDISK_B: u64 = 2;
    nodes[0]
        .create_vdisk(VDISK_B, CAPACITY * BLOCK as u64, 0)
        .unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    let disk = {
        let node = std::mem::replace(&mut nodes[2], fresh_node(999, 2));
        node.into_pool().into_brick().into_disk()
    };
    fence(&mut nodes, &mut net, 2, &[0, 1]);
    nodes[0].delete_vdisk(VDISK_B).unwrap();
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    nodes[2] = reopen_placed(disk, 2);
    net.revive(2);
    for other in [0u8, 1u8] {
        nodes[2].connect(other);
        nodes[other as usize].connect(2);
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }
    assert!(
        nodes[2].pool().vdisk_size(VDISK_B).is_err(),
        "a deleted vdisk resurrected on the returning member"
    );
}

/// Run every member's incoming moves to completion, pumping between
/// steps — the workflow's loop, worn by the harness.
fn run_moves(
    nodes: &mut [ReplNode<SimDisk>; 3],
    net: &mut Net,
    guests: &mut Guests,
    who: &[usize],
) {
    for _ in 0..64 {
        let mut owed = 0;
        for member in who {
            owed += nodes[*member].step_reassign().unwrap();
        }
        pump(nodes, net, guests);
        if owed == 0 {
            return;
        }
    }
    panic!("sixty-four move rounds and blocks are still owed");
}

fn assert_exact_homes(
    nodes: &mut [ReplNode<SimDisk>; 3],
    members: &[u8],
    map: &SliceMap,
    written: &[(u64, u8)],
) {
    for (index, generation) in written {
        let data = payload(*index, *generation);
        let hash = lumen_fs::hash_block(&data);
        let homes = map.homes_of(&hash);
        for member in members {
            assert_eq!(
                nodes[*member as usize].pool().has_block(0, &hash),
                homes.contains(member),
                "block {index} gen {generation} misplaced on member {member}"
            );
        }
    }
}

#[test]
fn growing_to_a_third_member_rebalances_and_serves_throughout() {
    // Two members, then a third: the 2→3 scale-out at the engine level.
    let mut nodes = [
        fresh_node_of(60, 0, &[0, 1]),
        fresh_node_of(61, 1, &[0, 1]),
        fresh_node_of(62, 2, &[0, 1]),
    ];
    let mut net = Net::default();
    net.kill(2);
    let mut guests = Guests::new();
    nodes[0].connect(1);
    nodes[1].connect(0);
    pump(&mut nodes, &mut net, &mut guests);
    nodes[0]
        .create_vdisk(VDISK, CAPACITY * BLOCK as u64, 0)
        .unwrap();
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    // The newcomer arrives, resyncs metadata, and the reassignment opens.
    net.revive(2);
    for other in [0u8, 1u8] {
        nodes[2].connect(other);
        nodes[other as usize].connect(2);
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }
    for node in &mut nodes {
        node.prepare_reassign(2, &[0, 1, 2]).unwrap();
    }
    // A guest writes mid-move: the union regime must land it somewhere
    // the commit will not orphan.
    nodes[0].step_reassign().unwrap();
    let owed_before = nodes[2].step_reassign().unwrap();
    assert!(owed_before > 0, "the newcomer owes itself nothing to pull");
    pump(&mut nodes, &mut net, &mut guests);
    for index in 0..4 {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 2))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1, 2]);
    for node in &mut nodes {
        node.commit_reassign().unwrap();
    }

    // The new map governs: version 2 everywhere, and after a collection
    // every block sits on exactly its new homes — including the ones
    // written mid-move.
    let grown = SliceMap::for_members(1, &[0, 1])
        .unwrap()
        .reassigned(2, &[0, 1, 2])
        .unwrap()
        .map;
    for node in &mut nodes {
        assert_eq!(node.pool().placement().unwrap().1.version(), 2);
        node.collect_garbage().unwrap();
        let report = node.pool().scrub().unwrap();
        assert_eq!(report.corrupt, vec![]);
        assert_eq!(report.missing, vec![]);
    }
    let written: Vec<(u64, u8)> = (0..CAPACITY)
        .map(|index| (index, if index < 4 { 2 } else { 1 }))
        .collect();
    assert_exact_homes(&mut nodes, &MEMBERS, &grown, &written);
    for member in 0..3 {
        for (index, generation) in &written {
            let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, *index);
            assert_eq!(
                found.as_deref(),
                Some(payload(*index, *generation).as_slice())
            );
        }
    }

    // The committed map survives a crash: the manifest carries it, and it
    // wins over whatever stale seed the reopener supplies.
    let mut disk = {
        let node = std::mem::replace(&mut nodes[2], fresh_node(999, 2));
        node.into_pool().into_brick().into_disk()
    };
    net.kill(2);
    disk.crash();
    let set = BrickSet::single(Brick::open(disk).unwrap()).unwrap();
    let stale_seed = SliceMap::for_members(1, &[0, 1]).unwrap();
    let reopened = Pool::open_set_placed(set, 2, stale_seed).unwrap();
    assert_eq!(
        reopened.placement().unwrap().1.version(),
        2,
        "the anchored map lost to a stale seed"
    );
}

#[test]
fn a_leaving_members_copies_drop_only_after_the_commit() {
    let (mut nodes, mut net, mut guests) = synced_trio(70);
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    // A block member 2 homes today and will not tomorrow.
    let displaced = (0..CAPACITY)
        .find(|index| {
            let hash = lumen_fs::hash_block(&payload(*index, 1));
            let now = SliceMap::for_members(1, &MEMBERS).unwrap();
            now.holds(2, &hash)
        })
        .expect("sixty blocks and none on member 2");
    let displaced_hash = lumen_fs::hash_block(&payload(displaced, 1));

    for node in &mut nodes {
        node.prepare_reassign(2, &[0, 1]).unwrap();
    }
    // Before the commit, a collection must keep the displaced copy: a
    // pending home may still be pulling from it.
    nodes[2].collect_garbage().unwrap();
    assert!(
        nodes[2].pool().has_block(0, &displaced_hash),
        "a pre-commit collection swept a displaced block"
    );
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1, 2]);
    for node in &mut nodes {
        node.commit_reassign().unwrap();
    }
    nodes[2].collect_garbage().unwrap();
    assert!(
        !nodes[2].pool().has_block(0, &displaced_hash),
        "the commit did not license the displacement drop"
    );
    // Both remaining members hold everything; the leaver serves by fetch.
    for index in 0..CAPACITY {
        let hash = lumen_fs::hash_block(&payload(index, 1));
        assert!(nodes[0].pool().has_block(0, &hash));
        assert!(nodes[1].pool().has_block(0, &hash));
        let found = read_anywhere(&mut nodes, &mut net, &mut guests, 2, VDISK, index);
        assert_eq!(found.as_deref(), Some(payload(index, 1).as_slice()));
    }
}

#[test]
fn re_protection_heals_a_death_and_the_revenant_adopts_the_map_it_missed() {
    let (mut nodes, mut net, mut guests) = synced_trio(80);
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    // Member 2 dies; the survivors re-protect: every slice back to two
    // copies instead of running degraded until repair — the thing neither
    // two nodes nor DRBD can ever do.
    let disk = {
        let node = std::mem::replace(&mut nodes[2], fresh_node(999, 2));
        node.into_pool().into_brick().into_disk()
    };
    fence(&mut nodes, &mut net, 2, &[0, 1]);
    for member in [0, 1] {
        nodes[member].prepare_reassign(2, &[0, 1]).unwrap();
    }
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1]);
    for member in [0, 1] {
        nodes[member].commit_reassign().unwrap();
    }
    for index in 0..CAPACITY {
        let hash = lumen_fs::hash_block(&payload(index, 1));
        assert!(
            nodes[0].pool().has_block(0, &hash) && nodes[1].pool().has_block(0, &hash),
            "block {index} is not back to two copies"
        );
    }

    // The revenant returns two map versions behind; the hello refuses to
    // elect roles across the gap, the map ships whole, and the resync
    // runs under the version it adopted.
    nodes[2] = reopen_placed(disk, 2);
    net.revive(2);
    for other in [0u8, 1u8] {
        nodes[2].connect(other);
        nodes[other as usize].connect(2);
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }
    assert_eq!(
        nodes[2].pool().placement().unwrap().1.version(),
        2,
        "the revenant never adopted the map it missed"
    );
    // Under map v2 it homes nothing; a collection proves it keeps nothing,
    // and every read still answers by fetch.
    nodes[2].collect_garbage().unwrap();
    for index in 0..CAPACITY {
        let found = read_anywhere(&mut nodes, &mut net, &mut guests, 2, VDISK, index);
        assert_eq!(found.as_deref(), Some(payload(index, 1).as_slice()));
    }

    // And the pool grows back to three, full circle.
    for node in &mut nodes {
        node.prepare_reassign(3, &[0, 1, 2]).unwrap();
    }
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1, 2]);
    for node in &mut nodes {
        node.commit_reassign().unwrap();
    }
    let full = nodes[0].pool().placement().unwrap().1.clone();
    for node in &mut nodes {
        node.collect_garbage().unwrap();
    }
    let written: Vec<(u64, u8)> = (0..CAPACITY).map(|index| (index, 1)).collect();
    assert_exact_homes(&mut nodes, &MEMBERS, &full, &written);
}

#[test]
fn a_joiner_dying_mid_move_costs_nothing_and_the_retry_lands() {
    let (mut nodes, mut net, mut guests) = synced_trio(90);
    for index in 0..CAPACITY {
        nodes[0]
            .write_block(VDISK, index, &payload(index, 1))
            .unwrap();
    }
    nodes[0].flush().unwrap();
    pump(&mut nodes, &mut net, &mut guests);

    // Shrink to [0,1] and grow back — so member 2 is mid-JOIN when it
    // dies: it owes itself blocks it has not pulled.
    for node in &mut nodes {
        node.prepare_reassign(2, &[0, 1]).unwrap();
    }
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1, 2]);
    for node in &mut nodes {
        node.commit_reassign().unwrap();
        node.collect_garbage().unwrap();
    }
    for node in &mut nodes {
        node.prepare_reassign(3, &[0, 1, 2]).unwrap();
    }
    let owed = nodes[2].step_reassign().unwrap();
    assert!(
        owed > 0,
        "the joiner owes itself nothing — the test is inert"
    );
    pump(&mut nodes, &mut net, &mut guests);

    // The joiner dies mid-move. Nothing is owed to anyone: the committed
    // map still governs, its homes still hold everything, and no one
    // committed a map whose homes are missing.
    let disk = {
        let node = std::mem::replace(&mut nodes[2], fresh_node(999, 2));
        node.into_pool().into_brick().into_disk()
    };
    fence(&mut nodes, &mut net, 2, &[0, 1]);
    for index in 0..CAPACITY {
        for member in [0usize, 1] {
            let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, index);
            assert_eq!(found.as_deref(), Some(payload(index, 1).as_slice()));
        }
    }

    // It returns; the reassignment is re-delivered (the layer above owns
    // that), the moves re-run idempotently, and the commit lands.
    nodes[2] = reopen_placed(disk, 2);
    net.revive(2);
    for other in [0u8, 1u8] {
        nodes[2].connect(other);
        nodes[other as usize].connect(2);
    }
    pump(&mut nodes, &mut net, &mut guests);
    for node in &nodes {
        assert_eq!(node.state(), ReplState::Synced);
    }
    for node in &mut nodes {
        match node.reassign_pending() {
            Some(3) => {}
            _ => node.prepare_reassign(3, &[0, 1, 2]).unwrap(),
        }
    }
    run_moves(&mut nodes, &mut net, &mut guests, &[0, 1, 2]);
    for node in &mut nodes {
        node.commit_reassign().unwrap();
        node.collect_garbage().unwrap();
    }
    let full = nodes[0].pool().placement().unwrap().1.clone();
    let written: Vec<(u64, u8)> = (0..CAPACITY).map(|index| (index, 1)).collect();
    assert_exact_homes(&mut nodes, &MEMBERS, &full, &written);
    for member in 0..3 {
        for index in 0..CAPACITY {
            let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, index);
            assert_eq!(found.as_deref(), Some(payload(index, 1).as_slice()));
        }
    }
}

#[test]
fn a_change_that_would_orphan_slices_is_refused_by_name() {
    let (mut nodes, _net, _guests) = synced_trio(95);
    // Every slice's homes are among {0,1,2}; a set naming none of them
    // strands every slice at once — RF=2 exhausted, refused rather than
    // papered over with homes that hold nothing.
    let err = nodes[0].prepare_reassign(2, &[3, 4]).unwrap_err();
    assert!(
        matches!(err, FsError::Placement(_)),
        "an orphaning change was accepted: {err}"
    );
    // A commit with nothing pending is equally refused.
    assert!(nodes[0].commit_reassign().is_err());
}

/// The macro-event fuzz: acked writes, single-member deaths healed before
/// the next event, fetched reads, maintenance — seeded, with the model
/// asserting acked-survives, placement exactness, and era agreement at
/// every stable point.
#[test]
fn three_node_histories_converge_without_losing_acks() {
    for seed in 0..6u64 {
        run_history(seed);
    }
}

fn run_history(seed: u64) {
    let mut rng = SplitMix64::new(seed.wrapping_mul(0x00FA_CADE).wrapping_add(3));
    let (mut nodes, mut net, mut guests) = synced_trio(9000 + seed);
    let mut writer: usize = 0;
    let mut acked: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut issued: HashSet<(usize, u64)> = HashSet::new();

    for event in 0..12 {
        match rng.next_below(10) {
            0..=4 => {
                let index = rng.next_below(CAPACITY);
                let mut data = vec![0u8; 1 + rng.next_below(BLOCK as u64) as usize];
                rng.fill(&mut data);
                nodes[writer].write_block(VDISK, index, &data).unwrap();
                let ticket = nodes[writer].flush().unwrap();
                issued.insert((writer, ticket));
                pump(&mut nodes, &mut net, &mut guests);
                assert!(
                    guests.done[writer].contains(&ticket),
                    "seed {seed} event {event}: a synced flush never completed"
                );
                acked.insert(index, data);
            }
            5..=6 => {
                // A death and a full heal: the victim is never the writer
                // half the time, and is the writer the other half.
                let victim = rng.next_below(3) as usize;
                let survivors: Vec<usize> = (0..3).filter(|i| *i != victim).collect();
                let disk = {
                    let node = std::mem::replace(
                        &mut nodes[victim],
                        fresh_node(50_000 + event, victim as u8),
                    );
                    node.into_pool().into_brick().into_disk()
                };
                fence(&mut nodes, &mut net, victim, &survivors);
                if victim == writer {
                    writer = survivors[0];
                    nodes[writer].claim_writer(VDISK).unwrap();
                }
                let index = rng.next_below(CAPACITY);
                let mut data = vec![0u8; 1 + rng.next_below(BLOCK as u64) as usize];
                rng.fill(&mut data);
                nodes[writer].write_block(VDISK, index, &data).unwrap();
                let ticket = nodes[writer].flush().unwrap();
                issued.insert((writer, ticket));
                pump(&mut nodes, &mut net, &mut guests);
                assert!(
                    guests.done[writer].contains(&ticket),
                    "seed {seed} event {event}: a degraded flush never completed"
                );
                acked.insert(index, data);

                nodes[victim] = reopen_placed(disk, victim as u8);
                net.revive(victim as u8);
                for other in 0..3u8 {
                    if other as usize != victim {
                        nodes[victim].connect(other);
                        nodes[other as usize].connect(victim as u8);
                    }
                }
                pump(&mut nodes, &mut net, &mut guests);
                for node in &nodes {
                    assert_eq!(
                        node.state(),
                        ReplState::Synced,
                        "seed {seed} event {event}: the trio never healed"
                    );
                }
            }
            7 => {
                let member = rng.next_below(3) as usize;
                let index = rng.next_below(CAPACITY);
                let expected = acked.get(&index).cloned();
                let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, index);
                if let Some(expected) = expected {
                    assert_eq!(
                        found.as_deref(),
                        Some(expected.as_slice()),
                        "seed {seed} event {event}: member {member} misread {index}"
                    );
                }
            }
            8 => {
                for node in &mut nodes {
                    node.checkpoint().unwrap();
                }
            }
            _ => {
                for node in &mut nodes {
                    node.collect_garbage().unwrap();
                }
            }
        }

        // The invariants at every stable point.
        let eras: Vec<u64> = nodes.iter().map(|n| n.pool().era()).collect();
        assert!(
            eras[0] == eras[1] && eras[1] == eras[2],
            "seed {seed} event {event}: eras diverged {eras:?}"
        );
        for (index, data) in &acked {
            for member in 0..3 {
                let found = read_anywhere(&mut nodes, &mut net, &mut guests, member, VDISK, *index);
                assert_eq!(
                    found.as_ref(),
                    Some(data),
                    "seed {seed} event {event}: acked block {index} regressed on member {member}"
                );
            }
        }
        for (index, data) in &acked {
            let hash = lumen_fs::hash_block(data);
            let homes = homes_of(data);
            for member in MEMBERS {
                assert_eq!(
                    nodes[member as usize].pool().has_block(0, &hash),
                    homes.contains(&member),
                    "seed {seed} event {event}: block {index} misplaced on member {member}"
                );
            }
        }
        // Ticket accounting: everything issued resolved exactly once.
        for (member, ticket) in &issued {
            let done = guests.done[*member].contains(ticket);
            let failed = guests.failed[*member].contains(ticket);
            assert!(
                done ^ failed,
                "seed {seed} event {event}: ticket {ticket} on member {member} resolved oddly \
                 (done {done}, failed {failed})"
            );
        }
    }
}
