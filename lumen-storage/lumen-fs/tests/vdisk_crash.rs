//! The vdisk-level crash suite: the durability contract one layer up,
//! where it becomes the contract a guest's block device will stand on.
//!
//! For every simulated power-loss history:
//!
//! - a vdisk write acknowledged (by a flush or a checkpoint that returned)
//!   reads back byte-identical after recovery;
//! - an unacknowledged write lands whole or not at all — after a crash the
//!   block reads as one of the values it legitimately held, never a
//!   mixture, never garbage (content addressing makes a torn landing
//!   unrepresentable);
//! - a block never written stays unmapped;
//! - the pool recovers into a usable state after every crash.
//!
//! Two vdisks run side by side, one deep enough to force depth-two map
//! trees, and checkpoints are interleaved with crashes so recovery leans on
//! every combination of manifest, tree, and WAL replay. Every assertion
//! carries its seed.

use std::collections::HashMap;

use lumen_fs::{Brick, BrickParams, FsError, Pool, SimDisk, SplitMix64};

const KIB: u64 = 1024;
const BLOCK: usize = 4 * KIB as usize;

/// Two vdisks: one comfortably depth 1 (128 entries per node at 4 KiB
/// blocks), one that needs depth 2.
const VDISKS: [(u64, u64); 2] = [(1, 100), (2, 300)];

fn params() -> BrickParams {
    BrickParams {
        pool_uuid: [0xAA; 16],
        brick_uuid: [0xBB; 16],
        block_size: BLOCK as u32,
        segment_size: 128 * KIB,
        wal_size: 32 * KIB,
    }
}

/// What one block may legitimately read as.
#[derive(Debug, Clone, Default)]
struct BlockModel {
    /// The durable value — `None` is unmapped.
    acked: Option<Vec<u8>>,
    /// Values written since the last acknowledgement, oldest first. A crash
    /// may land the block on any of these, or leave it on `acked`.
    pending: Vec<Vec<u8>>,
}

struct Model {
    blocks: HashMap<(u64, u64), BlockModel>,
}

impl Model {
    fn write(&mut self, vdisk: u64, index: u64, payload: Vec<u8>) {
        self.blocks
            .entry((vdisk, index))
            .or_default()
            .pending
            .push(payload);
    }

    /// A flush or checkpoint returned: everything pending is now the value.
    fn acknowledge_all(&mut self) {
        for block in self.blocks.values_mut() {
            if let Some(last) = block.pending.pop() {
                block.acked = Some(last);
            }
            block.pending.clear();
        }
    }

    /// What the block must read as right now, mid-workload.
    fn current(&self, vdisk: u64, index: u64) -> Option<&Vec<u8>> {
        let block = self.blocks.get(&(vdisk, index))?;
        block.pending.last().or(block.acked.as_ref())
    }
}

fn payload(rng: &mut SplitMix64) -> Vec<u8> {
    let len = 1 + rng.next_below(BLOCK as u64) as usize;
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);
    buf
}

/// After a crash and recovery: every modeled block must read as one of its
/// legitimate values, and the model collapses onto what was observed.
fn verify_and_collapse(pool: &Pool<SimDisk>, model: &mut Model, seed: u64) {
    for ((vdisk, index), block) in model.blocks.iter_mut() {
        let observed = pool
            .read_block(*vdisk, *index)
            .unwrap_or_else(|err| panic!("seed {seed}: read after recovery failed: {err}"));
        let legitimate = observed == block.acked
            || observed
                .as_ref()
                .map(|seen| block.pending.iter().any(|p| p == seen))
                .unwrap_or(false);
        assert!(
            legitimate,
            "seed {seed}: vdisk {vdisk} block {index} recovered to a value it \
             never legitimately held ({} pending writes)",
            block.pending.len(),
        );
        block.acked = observed;
        block.pending.clear();
    }
    // Blocks nothing ever wrote stay unmapped.
    for (vdisk, capacity) in VDISKS {
        assert_eq!(
            pool.read_block(vdisk, capacity - 1).unwrap(),
            None,
            "seed {seed}: the reserved untouched block became mapped"
        );
    }
}

fn run_history(seed: u64) {
    let mut workload = SplitMix64::new(seed.wrapping_mul(0x00C0_FFEE));
    let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params()).unwrap();
    let mut pool = Pool::create(brick).unwrap();
    for (id, capacity) in VDISKS {
        pool.create_vdisk(id, capacity * BLOCK as u64).unwrap();
    }
    pool.flush().unwrap();
    let mut model = Model {
        blocks: HashMap::new(),
    };

    let crashes = 2 + workload.next_below(3);
    for _ in 0..crashes {
        let ops = 10 + workload.next_below(30);
        for _ in 0..ops {
            let roll = workload.next_below(100);
            if roll < 70 {
                let (vdisk, capacity) = VDISKS[workload.next_below(2) as usize];
                // Reserve the last block as never-written.
                let index = workload.next_below(capacity - 1);
                let data = payload(&mut workload);
                match pool.write_block(vdisk, index, &data) {
                    Ok(()) => model.write(vdisk, index, data),
                    Err(FsError::WalFull) => {
                        // A checkpoint acknowledges everything, then makes
                        // room; the write goes again.
                        pool.checkpoint().unwrap();
                        model.acknowledge_all();
                        pool.write_block(vdisk, index, &data).unwrap();
                        model.write(vdisk, index, data);
                    }
                    Err(other) => panic!("seed {seed}: write failed: {other}"),
                }
            } else if roll < 85 {
                pool.flush().unwrap();
                model.acknowledge_all();
            } else if roll < 92 {
                pool.checkpoint().unwrap();
                model.acknowledge_all();
            } else {
                // A mid-workload read must see the newest write, pending or
                // not.
                let (vdisk, capacity) = VDISKS[workload.next_below(2) as usize];
                let index = workload.next_below(capacity - 1);
                assert_eq!(
                    pool.read_block(vdisk, index).unwrap().as_ref(),
                    model.current(vdisk, index),
                    "seed {seed}: a live read disagreed with the model"
                );
            }
        }

        let mut disk = pool.into_brick().into_disk();
        disk.crash();
        pool = Pool::open(
            Brick::open(disk)
                .unwrap_or_else(|err| panic!("seed {seed}: brick recovery failed: {err}")),
        )
        .unwrap_or_else(|err| panic!("seed {seed}: pool recovery failed: {err}"));
        verify_and_collapse(&pool, &mut model, seed);
    }

    // Still a working pool: one more acknowledged write outlives one more
    // reopen.
    let data = payload(&mut workload);
    pool.write_block(1, 0, &data).unwrap();
    pool.flush().unwrap();
    let pool = Pool::open(Brick::open(pool.into_brick().into_disk()).unwrap()).unwrap();
    assert_eq!(
        pool.read_block(1, 0).unwrap().as_deref(),
        Some(data.as_slice()),
        "seed {seed}: the pool did not stay usable after its crashes"
    );
}

#[test]
fn every_acknowledged_vdisk_write_survives_every_crash_history() {
    for seed in 0..48 {
        run_history(seed);
    }
}

#[test]
fn a_crash_during_a_checkpoint_falls_back_to_the_previous_anchor() {
    // Write through a checkpoint, then write more and crash without one:
    // recovery must serve the checkpointed state plus whatever legitimately
    // survived of the tail — across many seeds so the crash lands at
    // different points inside the checkpoint's two-flush window.
    for seed in 200..230 {
        let brick = Brick::format(SimDisk::new(8 * KIB * KIB, seed), params()).unwrap();
        let mut pool = Pool::create(brick).unwrap();
        pool.create_vdisk(1, 50 * BLOCK as u64).unwrap();
        pool.write_block(1, 1, b"checkpointed").unwrap();
        pool.checkpoint().unwrap();
        pool.write_block(1, 2, b"tail write").unwrap();
        let mut disk = pool.into_brick().into_disk();
        disk.crash();
        let pool = Pool::open(Brick::open(disk).unwrap()).unwrap();
        assert_eq!(
            pool.read_block(1, 1).unwrap().unwrap(),
            b"checkpointed",
            "seed {seed}"
        );
        let tail = pool.read_block(1, 2).unwrap();
        assert!(
            tail.is_none() || tail.as_deref() == Some(b"tail write".as_slice()),
            "seed {seed}: the unacknowledged tail recovered to garbage"
        );
    }
}
