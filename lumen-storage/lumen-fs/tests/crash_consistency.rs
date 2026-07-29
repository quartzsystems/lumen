//! The crash-consistency suite: many simulated power-loss histories, each
//! replayable from its seed.
//!
//! This is the harness docs/lumenfs.md requires to exist before the engine
//! grows features — the durability contract, held under fire:
//!
//! - every block acknowledged (put before a flush that returned) is present
//!   and byte-identical after recovery, through any number of crashes;
//! - a get never returns wrong bytes for any hash, acknowledged or not;
//! - recovery always yields a usable brick — the workload continues.
//!
//! Unacknowledged blocks may survive a crash or vanish; both are correct,
//! and the suite deliberately asserts nothing about them beyond integrity.
//!
//! Every assertion message carries the seed; a failure here reproduces with
//! a one-line test.

use std::collections::HashMap;

use lumen_fs::{Brick, BrickParams, FsError, SimDisk, SplitMix64};

const KIB: usize = 1024;
const BLOCK_SIZE: u32 = 16 * KIB as u32;
const SEGMENT_SIZE: u64 = 256 * KIB as u64;
const DISK_SIZE: u64 = 4 * 1024 * KIB as u64;

fn params() -> BrickParams {
    BrickParams {
        pool_uuid: [0xAA; 16],
        brick_uuid: [0xBB; 16],
        block_size: BLOCK_SIZE,
        segment_size: SEGMENT_SIZE,
        wal_size: 64 * KIB as u64,
    }
}

/// A deterministic payload: length and bytes derived from the workload rng,
/// unique per (seed, op) so every block is distinguishable.
fn payload(rng: &mut SplitMix64) -> Vec<u8> {
    let len = 1 + rng.next_below(BLOCK_SIZE as u64) as usize;
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);
    buf
}

/// One full history for one seed: a workload of puts and flushes with
/// crashes injected between operations, recovery after each crash, and the
/// contract asserted after every recovery.
fn run_history(seed: u64) {
    let mut workload = SplitMix64::new(seed.wrapping_mul(0x00DD_BA11)); // decides ops
    let disk = SimDisk::new(DISK_SIZE, seed); // decides crash fates
    let mut brick = Brick::format(disk, params()).unwrap();

    // What the contract owes us: payloads acknowledged by a returned flush.
    let mut acked: HashMap<lumen_fs::BlockHash, Vec<u8>> = HashMap::new();
    // Put but not yet flushed — promoted to acked on flush, discarded from
    // expectations on crash (they may survive; we just stop owing them).
    let mut pending: HashMap<lumen_fs::BlockHash, Vec<u8>> = HashMap::new();

    let crashes = 3 + workload.next_below(3);
    for _ in 0..crashes {
        let ops = 10 + workload.next_below(40);
        for _ in 0..ops {
            if workload.chance(75) {
                let data = payload(&mut workload);
                match brick.put(&data) {
                    Ok(hash) => {
                        if !acked.contains_key(&hash) {
                            pending.insert(hash, data);
                        }
                    }
                    Err(FsError::Full) => break,
                    Err(other) => panic!("seed {seed}: put failed: {other}"),
                }
            } else {
                brick.flush().unwrap();
                acked.extend(pending.drain());
            }
        }

        // Power loss, recovery, contract check.
        let mut disk = brick.into_disk();
        disk.crash();
        pending.clear();
        brick = match Brick::open(disk) {
            Ok(brick) => brick,
            Err(err) => panic!("seed {seed}: recovery failed: {err}"),
        };
        for (hash, expected) in &acked {
            match brick.get(hash) {
                Ok(Some(found)) => assert_eq!(
                    &found, expected,
                    "seed {seed}: an acknowledged block came back changed"
                ),
                Ok(None) => panic!("seed {seed}: an acknowledged block vanished ({hash:?})"),
                Err(err) => panic!("seed {seed}: an acknowledged block errored: {err}"),
            }
        }
    }

    // The brick is still a working brick: one more write-flush-reopen round.
    let data = payload(&mut workload);
    let hash = brick.put(&data).expect("post-recovery put");
    brick.flush().unwrap();
    let brick = Brick::open(brick.into_disk()).unwrap();
    assert_eq!(
        brick.get(&hash).unwrap().as_deref(),
        Some(data.as_slice()),
        "seed {seed}: the brick did not stay usable after its crashes"
    );
}

#[test]
fn every_acknowledged_block_survives_every_crash_history() {
    for seed in 0..96 {
        run_history(seed);
    }
}

#[test]
fn a_crash_before_the_first_flush_loses_nothing_that_was_promised() {
    for seed in 100..120 {
        let mut brick = Brick::format(SimDisk::new(DISK_SIZE, seed), params()).unwrap();
        let mut rng = SplitMix64::new(seed);
        for _ in 0..8 {
            let data = payload(&mut rng);
            brick.put(&data).unwrap();
        }
        // No flush: nothing is owed. Crash and recovery must still produce
        // a coherent, usable brick.
        let mut disk = brick.into_disk();
        disk.crash();
        let mut brick = Brick::open(disk).unwrap();
        let hash = brick.put(b"after the storm").unwrap();
        brick.flush().unwrap();
        assert_eq!(
            brick.get(&hash).unwrap().unwrap(),
            b"after the storm",
            "seed {seed}"
        );
    }
}

#[test]
fn a_crash_loop_leaks_no_empty_incarnations() {
    // The crash-loop shape: open, write a little, crash before flushing,
    // again and again. Two things are true at every recovery: an
    // incarnation that landed nothing is reclaimed (never a leaked
    // segment), and every segment still held is held by at least one live
    // block. Unflushed records that happen to survive a crash intact are
    // legitimately indexed and legitimately consume segments — that is
    // fragmentation for GC (later in phase 1), not a leak — so `Full` is a
    // lawful end to the loop, and a wedge would show as `Full` while
    // segments outnumber blocks.
    let seed = 424_242;
    let mut disk = SimDisk::new(DISK_SIZE, seed);
    {
        let brick = Brick::format(disk, params()).unwrap();
        disk = brick.into_disk();
    }
    let mut rng = SplitMix64::new(seed);
    for round in 0..64 {
        let mut brick = Brick::open(disk).unwrap();
        let stats = brick.stats();
        assert!(
            stats.segments_live <= stats.blocks,
            "round {round}: {} live segments but only {} blocks — an empty \
             incarnation leaked",
            stats.segments_live,
            stats.blocks,
        );
        let data = payload(&mut rng);
        match brick.put(&data) {
            Ok(_) => {}
            Err(FsError::Full) => break,
            Err(other) => panic!("round {round}: {other}"),
        }
        disk = brick.into_disk();
        disk.crash();
    }
}

#[test]
fn acknowledged_blocks_accumulate_across_generations_of_crashes() {
    // A longer arc than the per-seed histories: one brick, twelve
    // crash-recover generations, the acked set only ever growing.
    let seed = 31_337;
    let mut workload = SplitMix64::new(seed);
    let mut brick = Brick::format(SimDisk::new(DISK_SIZE, seed), params()).unwrap();
    let mut acked: HashMap<lumen_fs::BlockHash, Vec<u8>> = HashMap::new();

    for generation in 0..12 {
        for _ in 0..6 {
            let data = payload(&mut workload);
            if let Ok(hash) = brick.put(&data) {
                acked.insert(hash, data);
            }
        }
        brick.flush().unwrap();
        let mut disk = brick.into_disk();
        disk.crash();
        brick = Brick::open(disk).unwrap();
        for (hash, expected) in &acked {
            assert_eq!(
                brick.get(hash).unwrap().as_deref(),
                Some(expected.as_slice()),
                "generation {generation}: an acknowledged block regressed"
            );
        }
    }
}
