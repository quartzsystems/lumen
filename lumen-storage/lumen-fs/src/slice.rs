//! Placement: which members hold a block, and what moves when that
//! changes. The phase-5 core of docs/lumenfs.md, as pure arithmetic.
//!
//! A block's home is derived from its content address and nothing else:
//! **hash → slice → an ordered pair of members**. That single sentence is
//! why dedupe and placement are one mechanism rather than two — identical
//! bytes hash identically, so they land on the same pair and the second
//! copy is a refcount there instead of a write. Nothing consults a
//! directory, and no block's location is remembered anywhere.
//!
//! There are 256 slices per tier, and a slice is one byte of the hash.
//! BLAKE3 is uniform, so the first byte is as good a shard key as any
//! function of the whole digest and costs nothing to compute.
//!
//! ## The map, and why a ring
//!
//! Each slice names two members, ordered: the first is the read preference
//! and the resync source, the second is the mirror. A member set of `n`
//! gets the `n` ring pairs — `(m0,m1), (m1,m2), … (m[n-1],m0)` — dealt
//! round-robin across slices. Every member appears in exactly two ring
//! pairs, so seats come out even to within one, and at three members the
//! pattern is `(A,B), (B,C), (C,A)` exactly as the design names it. At two
//! members the ring degenerates to both members on every slice, which is
//! the appliance's center of gravity: every read is local because every
//! node is a home.
//!
//! What this costs at three is worth stating plainly, because it surprises
//! people: with RF=2 and three members, **each member holds about two
//! thirds of the unique data**, not one third. Total seats are `2 × 256`
//! however many members there are, so three members split 512 seats about
//! 171 each out of 256 slices. That is also the price of adding the third
//! member — it must be filled to two thirds before the pool is balanced.
//! In exchange the pool's capacity exceeds any single node, which is what
//! makes a vdisk larger than a node possible at all.
//!
//! ## Changing the map safely
//!
//! [`SliceMap::reassigned`] is the one operation that moves data, and it
//! is built to be interruptible: it keeps a slice's existing homes wherever
//! the balance allows, and never takes both homes of a slice away in one
//! step. So every slice always retains a member that already holds its
//! blocks — a source for the copy, and a reader for anyone asking meanwhile.
//! Growing to a third member, shrinking back to two, and re-protecting
//! after a death are all the same call with a different member set; there
//! is no separate re-protection algorithm to keep honest.
//!
//! The one case the arithmetic cannot rescue is a slice whose *both* homes
//! left at once. That is RF=2 exhausted, and it is reported by name
//! ([`Reassignment::orphaned`]) rather than papered over with a fresh
//! assignment that would quietly point at members holding nothing.

use std::collections::BTreeMap;

use crate::error::{FsError, Result};
use crate::hash::BlockHash;
use crate::repl::NodeId;

/// Slices per tier, as designed. One byte of the hash.
pub const SLICES: usize = 256;

/// The two members holding a slice: read preference first, mirror second.
pub type Homes = [NodeId; 2];

/// Which slice a block belongs to — the whole of placement's input.
pub fn slice_of(hash: &BlockHash) -> usize {
    hash.as_bytes()[0] as usize
}

/// One copy a reassignment requires: `slice`'s blocks must reach `target`,
/// and `source` is a member that still holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceMove {
    pub slice: usize,
    pub source: NodeId,
    pub target: NodeId,
}

/// What a reassignment decided: the new map, the copies it implies, and
/// any slice whose data is simply gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reassignment {
    pub map: SliceMap,
    /// Copies to perform, in slice order. Until a move completes, the
    /// slice is readable from `source`; after it, the displaced member may
    /// drop its copy.
    pub moves: Vec<SliceMove>,
    /// Slices that lost both homes at once — RF=2 exhausted. The new map
    /// assigns them so the pool stays well-formed, but no member holds
    /// their blocks and no move can be planned.
    pub orphaned: Vec<usize>,
}

/// Where every slice lives. Rides the membership record — versioned,
/// gossiped, changed only by a workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceMap {
    version: u64,
    slices: Vec<Homes>,
}

impl SliceMap {
    /// A balanced map for `members`, from nothing. Members are sorted, so
    /// the same set always yields the same map on every node.
    pub fn for_members(version: u64, members: &[NodeId]) -> Result<SliceMap> {
        let members = normalize(members)?;
        let ring: Vec<Homes> = (0..members.len())
            .map(|i| [members[i], members[(i + 1) % members.len()]])
            .collect();
        let slices = (0..SLICES).map(|slice| ring[slice % ring.len()]).collect();
        let map = SliceMap { version, slices };
        map.validate()?;
        Ok(map)
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn homes(&self, slice: usize) -> Homes {
        self.slices[slice]
    }

    /// Where a block lives, by content address.
    pub fn homes_of(&self, hash: &BlockHash) -> Homes {
        self.slices[slice_of(hash)]
    }

    /// Whether `node` stores this block. The question a write asks before
    /// putting bytes on its own brick, and a collection asks before
    /// keeping them.
    pub fn holds(&self, node: NodeId, hash: &BlockHash) -> bool {
        self.homes_of(hash).contains(&node)
    }

    /// Which member to read a block from: this node if it is a home —
    /// locality is free and always preferred — otherwise the slice's
    /// read-preference home.
    pub fn read_from(&self, node: NodeId, hash: &BlockHash) -> NodeId {
        let homes = self.homes_of(hash);
        if homes.contains(&node) {
            node
        } else {
            homes[0]
        }
    }

    /// Every member the map mentions, sorted.
    pub fn members(&self) -> Vec<NodeId> {
        let mut all: Vec<NodeId> = self.slices.iter().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// How many slices a member holds — its share of the pool's bytes.
    pub fn seats(&self, node: NodeId) -> usize {
        self.slices
            .iter()
            .filter(|homes| homes.contains(&node))
            .count()
    }

    /// Move to a new member set with as little copying as the balance
    /// allows: add a member, remove one, or replace one, all the same call.
    ///
    /// The invariant that makes this safe to run against a live pool is
    /// that no slice loses both homes in one step — see the module header.
    pub fn reassigned(&self, version: u64, members: &[NodeId]) -> Result<Reassignment> {
        if version <= self.version {
            return Err(FsError::Placement(
                "a reassignment must carry a higher version than the map it replaces",
            ));
        }
        let members = normalize(members)?;
        let targets = seat_targets(&members);

        // What each slice keeps: its old homes that are still members.
        let kept: Vec<Vec<NodeId>> = self
            .slices
            .iter()
            .map(|homes| {
                homes
                    .iter()
                    .copied()
                    .filter(|node| members.contains(node))
                    .collect()
            })
            .collect();

        let mut seats: BTreeMap<NodeId, usize> = members.iter().map(|n| (*n, 0)).collect();
        let mut placed: Vec<Vec<NodeId>> = kept.clone();
        for homes in &placed {
            for node in homes {
                *seats.get_mut(node).unwrap() += 1;
            }
        }

        // Slices short of two homes take the emptiest member that is not
        // already one of theirs. Emptiest-first is what keeps a newcomer
        // from being handed everything at once.
        let mut orphaned = Vec::new();
        for (slice, homes) in placed.iter_mut().enumerate() {
            if homes.is_empty() {
                orphaned.push(slice);
            }
            while homes.len() < 2 {
                let pick = members
                    .iter()
                    .copied()
                    .filter(|node| !homes.contains(node))
                    .min_by_key(|node| (seats[node], *node))
                    .ok_or(FsError::Placement("a slice needs two distinct members"))?;
                homes.push(pick);
                *seats.get_mut(&pick).unwrap() += 1;
            }
        }

        // Now balance by swapping one home at a time, never twice on the
        // same slice: that is what leaves every slice a member that
        // already holds its blocks.
        let mut touched = vec![false; SLICES];
        for &under in &members {
            // Bounded by the seats that exist; a pass that cannot find a
            // donor leaves the imbalance for validate() to reject rather
            // than spinning.
            while seats[&under] < targets[&under] {
                let Some((slice, donor)) = (0..SLICES)
                    .filter(|slice| !touched[*slice] && !placed[*slice].contains(&under))
                    .filter_map(|slice| {
                        placed[slice]
                            .iter()
                            .copied()
                            .filter(|node| seats[node] > targets[node])
                            // The fuller donor first, so members shed evenly.
                            .max_by_key(|node| (seats[node], std::cmp::Reverse(*node)))
                            .map(|donor| (slice, donor))
                    })
                    .next()
                else {
                    break;
                };
                let at = placed[slice].iter().position(|n| *n == donor).unwrap();
                placed[slice][at] = under;
                *seats.get_mut(&donor).unwrap() -= 1;
                *seats.get_mut(&under).unwrap() += 1;
                touched[slice] = true;
            }
        }

        let slices: Vec<Homes> = placed
            .iter()
            .map(|homes| [homes[0], homes[1]])
            .collect::<Vec<_>>();
        let map = SliceMap { version, slices };
        map.validate()?;

        // The copies fall out of comparing the two maps: any new home
        // needs the blocks, and any member that kept them can serve.
        let mut moves = Vec::new();
        for (slice, (before, after)) in self.slices.iter().zip(&map.slices).enumerate() {
            for target in *after {
                if before.contains(&target) {
                    continue;
                }
                if let Some(source) = kept[slice].first().copied() {
                    moves.push(SliceMove {
                        slice,
                        source,
                        target,
                    });
                }
            }
        }
        Ok(Reassignment {
            map,
            moves,
            orphaned,
        })
    }

    /// Every rule a map must satisfy, checked rather than assumed: two
    /// distinct homes per slice, and seats even to within one.
    pub fn validate(&self) -> Result<()> {
        if self.slices.len() != SLICES {
            return Err(FsError::Placement("a map must cover every slice"));
        }
        for homes in &self.slices {
            if homes[0] == homes[1] {
                return Err(FsError::Placement(
                    "a slice's two homes must be different members",
                ));
            }
        }
        let members = self.members();
        if members.len() < 2 {
            return Err(FsError::Placement("a pool needs at least two members"));
        }
        let targets = seat_targets(&members);
        for node in members {
            let seats = self.seats(node);
            if seats.abs_diff(targets[&node]) > 1 {
                return Err(FsError::Placement("a map must spread slices evenly"));
            }
        }
        Ok(())
    }
}

fn normalize(members: &[NodeId]) -> Result<Vec<NodeId>> {
    let mut members = members.to_vec();
    members.sort_unstable();
    members.dedup();
    if members.len() < 2 {
        return Err(FsError::Placement("a pool needs at least two members"));
    }
    Ok(members)
}

/// How many slices each member should hold: `2 × SLICES` seats split as
/// evenly as integers allow, the remainder going to the lowest ids so
/// every node computes the same answer.
fn seat_targets(members: &[NodeId]) -> BTreeMap<NodeId, usize> {
    let n = members.len();
    let base = 2 * SLICES / n;
    let extra = 2 * SLICES % n;
    members
        .iter()
        .enumerate()
        .map(|(i, node)| (*node, base + usize::from(i < extra)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_with_slice(slice: u8) -> BlockHash {
        let mut bytes = [0u8; 32];
        bytes[0] = slice;
        BlockHash::from_bytes(bytes)
    }

    #[test]
    fn two_members_hold_everything_so_every_read_is_local() {
        let map = SliceMap::for_members(1, &[0, 1]).unwrap();
        for slice in 0..SLICES {
            let homes = map.homes(slice);
            assert!(homes.contains(&0) && homes.contains(&1), "slice {slice}");
        }
        assert_eq!(map.seats(0), SLICES);
        assert_eq!(map.seats(1), SLICES);
        for slice in 0..=255u8 {
            let hash = hash_with_slice(slice);
            assert_eq!(map.read_from(0, &hash), 0);
            assert_eq!(map.read_from(1, &hash), 1);
        }
    }

    #[test]
    fn three_members_split_the_ring_evenly() {
        let map = SliceMap::for_members(1, &[0, 1, 2]).unwrap();
        // 512 seats across three members: 171, 171, 170.
        let seats: Vec<usize> = (0..3).map(|n| map.seats(n)).collect();
        assert_eq!(seats.iter().sum::<usize>(), 2 * SLICES);
        for count in &seats {
            assert!(
                count.abs_diff(2 * SLICES / 3) <= 1,
                "seats were {seats:?}, which is not even"
            );
        }
        // The design names this pattern: (A,B), (B,C), (C,A).
        assert_eq!(map.homes(0), [0, 1]);
        assert_eq!(map.homes(1), [1, 2]);
        assert_eq!(map.homes(2), [2, 0]);
        assert_eq!(map.homes(3), [0, 1]);
    }

    #[test]
    fn placement_follows_the_hash_so_duplicates_land_together() {
        let map = SliceMap::for_members(1, &[0, 1, 2]).unwrap();
        let payload = crate::hash::hash_block(b"the same bytes twice");
        assert_eq!(map.homes_of(&payload), map.homes_of(&payload));
        // And the slices are actually used: every one of the 256 is the
        // home of some hash, so nothing is dead space.
        let mut seen = [false; SLICES];
        for n in 0..4096u32 {
            seen[slice_of(&crate::hash::hash_block(&n.to_le_bytes()))] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "some slice was never selected by any hash"
        );
    }

    #[test]
    fn growing_to_three_fills_the_newcomer_and_never_strands_a_slice() {
        let two = SliceMap::for_members(1, &[0, 1]).unwrap();
        let grown = two.reassigned(2, &[0, 1, 2]).unwrap();

        assert!(grown.orphaned.is_empty(), "nothing should be orphaned");
        // The newcomer's seats are exactly the copies required, each on a
        // different slice — one home at a time, as promised.
        assert_eq!(grown.map.seats(2), grown.moves.len());
        let mut slices: Vec<usize> = grown.moves.iter().map(|m| m.slice).collect();
        let count = slices.len();
        slices.sort_unstable();
        slices.dedup();
        assert_eq!(slices.len(), count, "a slice moved twice in one step");
        assert!(
            grown.moves.iter().all(|m| m.target == 2),
            "only the newcomer should be gaining slices"
        );

        // Every slice keeps a member that already held its blocks: the
        // copy has a source and readers keep reading throughout.
        for slice in 0..SLICES {
            let before = two.homes(slice);
            let after = grown.map.homes(slice);
            assert!(
                after.iter().any(|node| before.contains(node)),
                "slice {slice} lost both of its homes at once"
            );
        }
        // And a move's source really holds the slice.
        for m in &grown.moves {
            assert!(two.homes(m.slice).contains(&m.source));
        }
    }

    #[test]
    fn shrinking_back_to_two_reprotects_every_slice_the_leaver_held() {
        let three = SliceMap::for_members(1, &[0, 1, 2]).unwrap();
        let shrunk = three.reassigned(2, &[0, 1]).unwrap();

        assert!(shrunk.orphaned.is_empty());
        for slice in 0..SLICES {
            let homes = shrunk.map.homes(slice);
            assert!(homes.contains(&0) && homes.contains(&1), "slice {slice}");
        }
        // Exactly the slices that had the departed member need a copy, and
        // each is sourced from a survivor that held it.
        for slice in 0..SLICES {
            let had_leaver = three.homes(slice).contains(&2);
            let moved = shrunk.moves.iter().any(|m| m.slice == slice);
            assert_eq!(had_leaver, moved, "slice {slice} moved when it need not");
        }
        for m in &shrunk.moves {
            assert!(three.homes(m.slice).contains(&m.source));
            assert_ne!(m.source, 2, "a departed member cannot serve a copy");
        }
    }

    #[test]
    fn a_death_among_three_leaves_every_slice_a_source() {
        // Re-protection is the same call: the member set simply lost one.
        let three = SliceMap::for_members(1, &[0, 1, 2]).unwrap();
        for dead in 0..3u8 {
            let survivors: Vec<NodeId> = (0..3u8).filter(|n| *n != dead).collect();
            let after = three.reassigned(2, &survivors).unwrap();
            assert!(
                after.orphaned.is_empty(),
                "a single death cannot orphan a slice at RF=2"
            );
            for m in &after.moves {
                assert_ne!(m.source, dead, "the dead node was asked to serve a copy");
                assert!(three.homes(m.slice).contains(&m.source));
            }
            after.map.validate().unwrap();
        }
    }

    #[test]
    fn losing_both_homes_of_a_slice_is_reported_not_papered_over() {
        // Four members in a ring, then the two that shared a slice both go.
        let four = SliceMap::for_members(1, &[0, 1, 2, 3]).unwrap();
        let doomed: Vec<usize> = (0..SLICES)
            .filter(|slice| four.homes(*slice) == [0, 1])
            .collect();
        assert!(!doomed.is_empty(), "expected some slice homed on 0 and 1");

        let after = four.reassigned(2, &[2, 3]).unwrap();
        assert_eq!(
            after.orphaned, doomed,
            "the slices whose data is gone must be named"
        );
        // The map stays well-formed, and no move pretends to have a source.
        after.map.validate().unwrap();
        for slice in &doomed {
            assert!(!after.moves.iter().any(|m| m.slice == *slice));
        }
    }

    #[test]
    fn reassigning_to_the_same_members_moves_nothing() {
        for members in [vec![0u8, 1], vec![0, 1, 2], vec![0, 1, 2, 3]] {
            let map = SliceMap::for_members(1, &members).unwrap();
            let again = map.reassigned(2, &members).unwrap();
            assert!(
                again.moves.is_empty(),
                "a no-op reassignment copied data for members {members:?}"
            );
            assert_eq!(again.map.slices, map.slices);
        }
    }

    #[test]
    fn the_same_inputs_always_give_the_same_map() {
        // Every node computes placement independently; disagreement would
        // put two nodes' idea of "mine" out of step.
        let a = SliceMap::for_members(7, &[2, 0, 1]).unwrap();
        let b = SliceMap::for_members(7, &[1, 2, 0]).unwrap();
        assert_eq!(a, b, "member order changed the map");
        let grown_once = a.reassigned(8, &[0, 1, 2, 3]).unwrap();
        let grown_twice = b.reassigned(8, &[3, 2, 1, 0]).unwrap();
        assert_eq!(grown_once, grown_twice);
    }

    #[test]
    fn a_pool_needs_two_members_and_a_rising_version() {
        assert!(SliceMap::for_members(1, &[0]).is_err());
        assert!(SliceMap::for_members(1, &[]).is_err());
        let map = SliceMap::for_members(5, &[0, 1]).unwrap();
        assert!(
            map.reassigned(5, &[0, 1, 2]).is_err(),
            "a reassignment at the same version is not orderable"
        );
    }

    #[test]
    fn every_member_count_up_to_eight_stays_balanced_and_valid() {
        let mut previous = SliceMap::for_members(1, &[0, 1]).unwrap();
        for n in 3..=8u8 {
            let members: Vec<NodeId> = (0..n).collect();
            SliceMap::for_members(1, &members)
                .unwrap()
                .validate()
                .unwrap();
            // And reached by growth, one member at a time, from two.
            let grown = previous.reassigned(n as u64, &members).unwrap();
            grown.map.validate().unwrap();
            assert!(grown.orphaned.is_empty());
            for m in &grown.moves {
                assert!(
                    previous.homes(m.slice).contains(&m.source),
                    "a move at {n} members had no real source"
                );
            }
            previous = grown.map;
        }
    }
}
