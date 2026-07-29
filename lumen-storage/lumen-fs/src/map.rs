//! The vdisk map: block index → content address, as a copy-on-write radix
//! tree whose nodes are ordinary pool blocks.
//!
//! This is the design doc's central elegance made concrete: because a node
//! is just a block, the map inherits everything blocks already have —
//! checksums, dedupe (two vdisks with identical regions share map nodes),
//! and eventually replication — with no second metadata format. A snapshot,
//! when it arrives, is nothing but a retained root hash.
//!
//! Shape: fixed-arity tree, each node an array of `block_size / 32` child
//! hashes. The all-zeros entry means "unmapped" — the one address no real
//! block can practically have. A vdisk's depth is fixed at create from its
//! size; level 0 nodes are leaves whose entries are data-block addresses.
//!
//! Updates are path copies: [`fold`] takes a batch of mutations (the pool's
//! dirty set at checkpoint) — a `Some` maps an index, a `None` unmaps it —
//! rewrites each touched path bottom-up, and returns the new root. A node
//! folded down to all zeros is not written at all; its parent entry goes to
//! zero, and a tree emptied entirely folds to no root. Nothing is modified
//! in place, so the old root — and every consistent state under it — stays
//! readable until GC decides otherwise.
//!
//! [`walk`] visits every node and mapped block under a root — the mark
//! phase of garbage collection and the reachability half of scrub, which
//! must agree with `fold` about the tree's shape and so live beside it.

use std::collections::BTreeMap;

use crate::brick::Brick;
use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::BlockHash;

const ENTRY_LEN: usize = 32;

const ZERO_ENTRY: [u8; ENTRY_LEN] = [0; ENTRY_LEN];

/// Child entries per node for a pool's block size. Block sizes are
/// validated at format to be sector multiples, so this always divides
/// cleanly and is always at least 128.
pub fn entries_per_node(block_size: u32) -> u64 {
    block_size as u64 / ENTRY_LEN as u64
}

/// The smallest depth whose capacity covers `capacity` blocks. Always at
/// least 1 — an empty vdisk still has a shape.
pub fn depth_for(capacity: u64, entries_per_node: u64) -> u32 {
    let mut depth = 1u32;
    let mut reach = entries_per_node as u128;
    while reach < capacity as u128 {
        reach *= entries_per_node as u128;
        depth += 1;
    }
    depth
}

fn load_node<D: Disk>(brick: &Brick<D>, hash: &BlockHash, block_size: u32) -> Result<Vec<u8>> {
    match brick.get(hash)? {
        Some(node) if node.len() == block_size as usize => Ok(node),
        Some(_) => Err(FsError::Corrupt("a map node has the wrong shape")),
        None => Err(FsError::Corrupt("a map node vanished from the store")),
    }
}

fn entry(node: &[u8], slot: u64) -> Option<BlockHash> {
    let at = slot as usize * ENTRY_LEN;
    let bytes: [u8; ENTRY_LEN] = node[at..at + ENTRY_LEN].try_into().unwrap();
    if bytes == ZERO_ENTRY {
        None
    } else {
        Some(BlockHash::from_bytes(bytes))
    }
}

fn set_entry(node: &mut [u8], slot: u64, value: Option<&BlockHash>) {
    let at = slot as usize * ENTRY_LEN;
    match value {
        Some(hash) => node[at..at + ENTRY_LEN].copy_from_slice(hash.as_bytes()),
        None => node[at..at + ENTRY_LEN].copy_from_slice(&ZERO_ENTRY),
    }
}

/// Resolve one block index through the tree. `Ok(None)` is "unmapped" — a
/// region never written, which the consumer renders as zeros.
pub fn lookup<D: Disk>(
    brick: &Brick<D>,
    root: &BlockHash,
    depth: u32,
    index: u64,
) -> Result<Option<BlockHash>> {
    let epn = entries_per_node(brick.block_size());
    let mut hash = *root;
    for level in (0..depth).rev() {
        let node = load_node(brick, &hash, brick.block_size())?;
        let slot = (index / epn.pow(level)) % epn;
        match entry(&node, slot) {
            Some(child) => hash = child,
            None => return Ok(None),
        }
    }
    Ok(Some(hash))
}

/// Apply a batch of mutations to the tree rooted at `root` (or to the
/// empty tree), writing new nodes along every touched path, and return the
/// new root — `None` if the tree folded away entirely.
pub fn fold<D: Disk>(
    brick: &mut Brick<D>,
    root: Option<&BlockHash>,
    depth: u32,
    mutations: &BTreeMap<u64, Option<BlockHash>>,
) -> Result<Option<BlockHash>> {
    let entries: Vec<(u64, Option<BlockHash>)> = mutations.iter().map(|(i, h)| (*i, *h)).collect();
    fold_level(brick, root, depth - 1, &entries)
}

fn fold_level<D: Disk>(
    brick: &mut Brick<D>,
    node_hash: Option<&BlockHash>,
    level: u32,
    mutations: &[(u64, Option<BlockHash>)],
) -> Result<Option<BlockHash>> {
    let block_size = brick.block_size();
    let epn = entries_per_node(block_size);
    let mut node = match node_hash {
        Some(hash) => load_node(brick, hash, block_size)?,
        None => vec![0u8; block_size as usize],
    };

    if level == 0 {
        for (index, value) in mutations {
            set_entry(&mut node, index % epn, value.as_ref());
        }
    } else {
        let stride = epn.pow(level);
        let mut children: BTreeMap<u64, Vec<(u64, Option<BlockHash>)>> = BTreeMap::new();
        for (index, value) in mutations {
            children
                .entry((index / stride) % epn)
                .or_default()
                .push((*index, *value));
        }
        for (slot, child_mutations) in children {
            let existing = entry(&node, slot);
            let new_child = fold_level(brick, existing.as_ref(), level - 1, &child_mutations)?;
            set_entry(&mut node, slot, new_child.as_ref());
        }
    }

    // A node of nothing is not a node: prune it, and let the parent's entry
    // go to zero. The whole tree can fold away this way.
    if node.iter().all(|b| *b == 0) {
        Ok(None)
    } else {
        Ok(Some(brick.put(&node)?))
    }
}

/// The non-zero child hashes of a raw node payload — what a peer resyncing
/// by tree diff needs, without knowing anything else about the node. The
/// caller tracks levels; a level-0 node's children are data blocks.
pub fn children(node: &[u8]) -> Vec<BlockHash> {
    node.chunks_exact(ENTRY_LEN)
        .filter_map(|chunk| {
            let bytes: [u8; ENTRY_LEN] = chunk.try_into().unwrap();
            if bytes == ZERO_ENTRY {
                None
            } else {
                Some(BlockHash::from_bytes(bytes))
            }
        })
        .collect()
}

/// What [`walk`] visits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapItem {
    /// A tree node, by its content address.
    Node(BlockHash),
    /// A mapped data block: which index points at which address.
    Block { index: u64, hash: BlockHash },
}

/// Visit every node and mapped block under a root — GC's mark phase and
/// scrub's reachability sweep.
pub fn walk<D: Disk>(
    brick: &Brick<D>,
    root: &BlockHash,
    depth: u32,
    visit: &mut dyn FnMut(MapItem),
) -> Result<()> {
    walk_level(brick, root, depth - 1, 0, visit)
}

fn walk_level<D: Disk>(
    brick: &Brick<D>,
    hash: &BlockHash,
    level: u32,
    base: u64,
    visit: &mut dyn FnMut(MapItem),
) -> Result<()> {
    visit(MapItem::Node(*hash));
    let node = load_node(brick, hash, brick.block_size())?;
    let epn = entries_per_node(brick.block_size());
    let stride = epn.pow(level);
    for slot in 0..epn {
        let child = match entry(&node, slot) {
            Some(child) => child,
            None => continue,
        };
        if level == 0 {
            visit(MapItem::Block {
                index: base + slot,
                hash: child,
            });
        } else {
            walk_level(brick, &child, level - 1, base + slot * stride, visit)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brick::BrickParams;
    use crate::sim::SimDisk;

    const KIB: u64 = 1024;

    fn brick(seed: u64) -> Brick<SimDisk> {
        Brick::format(
            SimDisk::new(4 * KIB * KIB, seed),
            BrickParams {
                pool_uuid: [1; 16],
                brick_uuid: [2; 16],
                // 4 KiB blocks: 128 entries per node, so depth 2 starts at
                // index 128 — reachable in a fast test.
                block_size: 4 * KIB as u32,
                segment_size: 128 * KIB,
                wal_size: 16 * KIB,
            },
        )
        .unwrap()
    }

    fn map_one(index: u64, hash: BlockHash) -> BTreeMap<u64, Option<BlockHash>> {
        let mut muts = BTreeMap::new();
        muts.insert(index, Some(hash));
        muts
    }

    #[test]
    fn depth_grows_exactly_when_capacity_outruns_a_level() {
        assert_eq!(depth_for(1, 128), 1);
        assert_eq!(depth_for(128, 128), 1);
        assert_eq!(depth_for(129, 128), 2);
        assert_eq!(depth_for(128 * 128, 128), 2);
        assert_eq!(depth_for(128 * 128 + 1, 128), 3);
    }

    #[test]
    fn a_folded_mutation_is_found_and_the_rest_stay_unmapped() {
        let mut brick = brick(1);
        let data = brick.put(b"the data block").unwrap();
        let root = fold(&mut brick, None, 1, &map_one(5, data))
            .unwrap()
            .unwrap();
        assert_eq!(lookup(&brick, &root, 1, 5).unwrap(), Some(data));
        assert_eq!(lookup(&brick, &root, 1, 6).unwrap(), None);
    }

    #[test]
    fn a_depth_two_tree_routes_indexes_to_the_right_leaves() {
        let mut brick = brick(2);
        let a = brick.put(b"a").unwrap();
        let b = brick.put(b"b").unwrap();
        let mut muts = BTreeMap::new();
        muts.insert(3u64, Some(a)); // leaf 0
        muts.insert(130u64, Some(b)); // leaf 1 (128 entries per node)
        let root = fold(&mut brick, None, 2, &muts).unwrap().unwrap();
        assert_eq!(lookup(&brick, &root, 2, 3).unwrap(), Some(a));
        assert_eq!(lookup(&brick, &root, 2, 130).unwrap(), Some(b));
        assert_eq!(lookup(&brick, &root, 2, 129).unwrap(), None);
    }

    #[test]
    fn a_second_fold_preserves_untouched_entries_and_the_old_root() {
        let mut brick = brick(3);
        let a = brick.put(b"first").unwrap();
        let b = brick.put(b"second").unwrap();
        let root_one = fold(&mut brick, None, 1, &map_one(1, a)).unwrap().unwrap();
        let root_two = fold(&mut brick, Some(&root_one), 1, &map_one(2, b))
            .unwrap()
            .unwrap();
        // The new root sees both writes; the old root still sees only the
        // first — that is the copy, and it is what a snapshot will pin.
        assert_eq!(lookup(&brick, &root_two, 1, 1).unwrap(), Some(a));
        assert_eq!(lookup(&brick, &root_two, 1, 2).unwrap(), Some(b));
        assert_eq!(lookup(&brick, &root_one, 1, 2).unwrap(), None);
    }

    #[test]
    fn identical_folds_produce_identical_roots() {
        let mut brick_one = brick(4);
        let mut brick_two = brick(5);
        let mut muts = BTreeMap::new();
        for i in 0..40u64 {
            let hash = brick_one.put(&i.to_le_bytes()).unwrap();
            brick_two.put(&i.to_le_bytes()).unwrap();
            muts.insert(i * 7, Some(hash));
        }
        let root_one = fold(&mut brick_one, None, 2, &muts).unwrap();
        let root_two = fold(&mut brick_two, None, 2, &muts).unwrap();
        // Content addressing makes map state canonical: same mappings, same
        // root, on any brick anywhere. Replication will lean on this.
        assert_eq!(root_one, root_two);
        assert!(root_one.is_some());
    }

    #[test]
    fn unmapping_the_last_entry_folds_the_tree_away() {
        let mut brick = brick(6);
        let data = brick.put(b"soon gone").unwrap();
        let root = fold(&mut brick, None, 2, &map_one(200, data))
            .unwrap()
            .unwrap();
        let mut unmap = BTreeMap::new();
        unmap.insert(200u64, None);
        let root = fold(&mut brick, Some(&root), 2, &unmap).unwrap();
        assert_eq!(root, None);
    }

    #[test]
    fn unmapping_one_leaf_prunes_it_without_touching_its_siblings() {
        let mut brick = brick(7);
        let a = brick.put(b"stays").unwrap();
        let b = brick.put(b"goes").unwrap();
        let mut muts = BTreeMap::new();
        muts.insert(3u64, Some(a)); // leaf 0
        muts.insert(300u64, Some(b)); // leaf 2
        let root = fold(&mut brick, None, 2, &muts).unwrap().unwrap();
        let mut unmap = BTreeMap::new();
        unmap.insert(300u64, None);
        let root = fold(&mut brick, Some(&root), 2, &unmap).unwrap().unwrap();
        assert_eq!(lookup(&brick, &root, 2, 3).unwrap(), Some(a));
        assert_eq!(lookup(&brick, &root, 2, 300).unwrap(), None);
        // The pruned leaf is gone from the tree's shape, not merely zeroed:
        // the walk sees one leaf node, not two.
        let mut nodes = 0;
        walk(&brick, &root, 2, &mut |item| {
            if matches!(item, MapItem::Node(_)) {
                nodes += 1;
            }
        })
        .unwrap();
        assert_eq!(nodes, 2, "root and one leaf");
    }

    #[test]
    fn the_walk_visits_every_node_and_every_mapping_exactly_once() {
        let mut brick = brick(8);
        let mut muts = BTreeMap::new();
        let mut expected = Vec::new();
        for i in [0u64, 5, 127, 128, 129, 256 * 128 - 1] {
            let hash = brick.put(&i.to_le_bytes()).unwrap();
            muts.insert(i, Some(hash));
            expected.push((i, hash));
        }
        let root = fold(&mut brick, None, 3, &muts).unwrap().unwrap();
        let mut blocks = Vec::new();
        let mut nodes = Vec::new();
        walk(&brick, &root, 3, &mut |item| match item {
            MapItem::Node(hash) => nodes.push(hash),
            MapItem::Block { index, hash } => blocks.push((index, hash)),
        })
        .unwrap();
        blocks.sort_unstable();
        assert_eq!(blocks, expected);
        let unique: std::collections::HashSet<_> = nodes.iter().collect();
        assert_eq!(unique.len(), nodes.len(), "no node visited twice");
    }
}
