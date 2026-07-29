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
//! dirty set at checkpoint), rewrites each touched path bottom-up, and
//! returns the new root. Nothing is modified in place, so the old root —
//! and every consistent state under it — remains readable until GC decides
//! otherwise.

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

/// Apply a batch of mutations to the tree rooted at `root` (or to the empty
/// tree), writing new nodes along every touched path, and return the new
/// root. Old nodes are untouched — this is where copy-on-write lives.
pub fn fold<D: Disk>(
    brick: &mut Brick<D>,
    root: Option<&BlockHash>,
    depth: u32,
    mutations: &BTreeMap<u64, BlockHash>,
) -> Result<BlockHash> {
    let entries: Vec<(u64, BlockHash)> = mutations.iter().map(|(i, h)| (*i, *h)).collect();
    fold_level(brick, root, depth - 1, &entries)
}

fn fold_level<D: Disk>(
    brick: &mut Brick<D>,
    node_hash: Option<&BlockHash>,
    level: u32,
    mutations: &[(u64, BlockHash)],
) -> Result<BlockHash> {
    let block_size = brick.block_size();
    let epn = entries_per_node(block_size);
    let mut node = match node_hash {
        Some(hash) => load_node(brick, hash, block_size)?,
        None => vec![0u8; block_size as usize],
    };

    if level == 0 {
        for (index, hash) in mutations {
            let at = (index % epn) as usize * ENTRY_LEN;
            node[at..at + ENTRY_LEN].copy_from_slice(hash.as_bytes());
        }
    } else {
        let stride = epn.pow(level);
        let mut children: BTreeMap<u64, Vec<(u64, BlockHash)>> = BTreeMap::new();
        for (index, hash) in mutations {
            children
                .entry((index / stride) % epn)
                .or_default()
                .push((*index, *hash));
        }
        for (slot, child_mutations) in children {
            let existing = entry(&node, slot);
            let new_child = fold_level(brick, existing.as_ref(), level - 1, &child_mutations)?;
            let at = slot as usize * ENTRY_LEN;
            node[at..at + ENTRY_LEN].copy_from_slice(new_child.as_bytes());
        }
    }
    brick.put(&node)
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
        let mut muts = BTreeMap::new();
        muts.insert(5u64, data);
        let root = fold(&mut brick, None, 1, &muts).unwrap();
        assert_eq!(lookup(&brick, &root, 1, 5).unwrap(), Some(data));
        assert_eq!(lookup(&brick, &root, 1, 6).unwrap(), None);
    }

    #[test]
    fn a_depth_two_tree_routes_indexes_to_the_right_leaves() {
        let mut brick = brick(2);
        let a = brick.put(b"a").unwrap();
        let b = brick.put(b"b").unwrap();
        let mut muts = BTreeMap::new();
        muts.insert(3u64, a); // leaf 0
        muts.insert(130u64, b); // leaf 1 (128 entries per node)
        let root = fold(&mut brick, None, 2, &muts).unwrap();
        assert_eq!(lookup(&brick, &root, 2, 3).unwrap(), Some(a));
        assert_eq!(lookup(&brick, &root, 2, 130).unwrap(), Some(b));
        assert_eq!(lookup(&brick, &root, 2, 129).unwrap(), None);
    }

    #[test]
    fn a_second_fold_preserves_untouched_entries_and_the_old_root() {
        let mut brick = brick(3);
        let a = brick.put(b"first").unwrap();
        let b = brick.put(b"second").unwrap();
        let mut first = BTreeMap::new();
        first.insert(1u64, a);
        let root_one = fold(&mut brick, None, 1, &first).unwrap();
        let mut second = BTreeMap::new();
        second.insert(2u64, b);
        let root_two = fold(&mut brick, Some(&root_one), 1, &second).unwrap();
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
            muts.insert(i * 7, hash);
        }
        let root_one = fold(&mut brick_one, None, 2, &muts).unwrap();
        let root_two = fold(&mut brick_two, None, 2, &muts).unwrap();
        // Content addressing makes map state canonical: same mappings, same
        // root, on any brick anywhere. Replication will lean on this.
        assert_eq!(root_one, root_two);
    }
}
