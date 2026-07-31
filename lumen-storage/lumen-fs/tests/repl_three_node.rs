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
    let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params(id)).unwrap();
    let mut node = ReplNode::new(Pool::create(brick).unwrap(), id);
    node.set_placement(&MEMBERS).unwrap();
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
