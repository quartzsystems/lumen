//! The threaded half of the reserve/write/publish suite: what only real
//! parallelism can exercise (docs/lumenfs-lock-sharding.md). The sim
//! histories in lumen-fs/tests/reserve_publish.rs order the phases
//! adversarially one thread at a time; this suite runs many guest
//! writers' detached pwrites genuinely concurrently — against each
//! other, against the peer's apply stream, and against collections
//! forced into the windows — then audits every acknowledged byte and
//! scrubs the platters.
//!
//! Deliberately duplicate-heavy: half the payloads repeat across
//! writers, so the dedupe-hit pin path runs under contention, not just
//! in a lab history.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_fsd::{export::nbd::VDISK, format_brick, Config, Daemon};

const DISK_BYTES: u64 = 128 << 20;
const VDISK_BYTES: u64 = 32 << 20;
const POOL: [u8; 16] = [0x5A; 16];
const BLOCK: usize = 16 * 1024;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "lumen-fsd-stress-{}-{name}.brick",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn synced_pair(name: &str) -> (Daemon, Daemon, Scratch, Scratch) {
    let brick_a = Scratch::new(&format!("{name}-a"));
    let brick_b = Scratch::new(&format!("{name}-b"));
    for (path, uuid) in [(&brick_a.0, [0xC0; 16]), (&brick_b.0, [0xC1; 16])] {
        format_brick(
            path,
            Some(DISK_BYTES),
            0,
            true,
            Vec::new(),
            Some(VDISK_BYTES),
            POOL,
            uuid,
        )
        .unwrap();
    }
    let a = Daemon::start(Config {
        node: 0,
        bricks: vec![brick_a.0.clone()],
        listen: Some("127.0.0.1:0".parse::<SocketAddr>().unwrap()),
        dials: Vec::new(),
        members: Vec::new(),
    })
    .unwrap();
    let b = Daemon::start(Config {
        node: 1,
        bricks: vec![brick_b.0.clone()],
        listen: None,
        dials: vec![a.peer_addr().unwrap()],
        members: Vec::new(),
    })
    .unwrap();
    wait_until("the pair to sync", || {
        a.status().state == lumen_fs::ReplState::Synced
            && b.status().state == lumen_fs::ReplState::Synced
    });
    (a, b, brick_a, brick_b)
}

/// The payload a `(writer, round, block)` triple deterministically
/// writes. `dup` folds half of them onto shared bytes, so concurrent
/// writers race the same hashes through reserve's dedupe constantly.
fn payload(writer: usize, round: usize, block: usize) -> Vec<u8> {
    let dup = (writer + round + block).is_multiple_of(2);
    let mut data = vec![0u8; BLOCK];
    if dup {
        data[0..8].copy_from_slice(&(((round * 7 + block) % 5) as u64).to_le_bytes());
        data[8] = 0xDD;
    } else {
        data[0..8].copy_from_slice(
            &((writer as u64) << 32 | (round * 1000 + block) as u64).to_le_bytes(),
        );
        data[8] = 0xFF;
    }
    data
}

/// Many writers, disjoint stripes of one vdisk, several rounds each —
/// with collections and checkpoints forced from another thread the
/// whole while. Then: every stripe's final round read back on both
/// members, and both platters scrubbed end to end.
#[test]
fn concurrent_writers_with_forced_collections_lose_nothing() {
    let (a, b, _ba, _bb) = synced_pair("writers");
    let guest = a.guest();
    guest.claim_writer(VDISK).unwrap();

    const WRITERS: usize = 6;
    const ROUNDS: usize = 5;
    const BLOCKS_PER_WRITE: usize = 16; // 256 KiB runs
    const WRITES_PER_ROUND: usize = 4;
    let stripe = (BLOCKS_PER_WRITE * WRITES_PER_ROUND * BLOCK) as u64;

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        let guest = a.guest();
        std::thread::spawn(move || {
            // The adversary thread: keep collections and flush barriers
            // landing inside other threads' reserve→publish windows.
            let mut n = 0u32;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = guest.flush();
                n += 1;
                if n.is_multiple_of(3) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
    };

    std::thread::scope(|scope| {
        for writer in 0..WRITERS {
            let guest = a.guest();
            scope.spawn(move || {
                let base = writer as u64 * stripe;
                for round in 0..ROUNDS {
                    for chunk in 0..WRITES_PER_ROUND {
                        let mut data = Vec::with_capacity(BLOCKS_PER_WRITE * BLOCK);
                        for block in 0..BLOCKS_PER_WRITE {
                            data.extend_from_slice(&payload(
                                writer,
                                round,
                                chunk * BLOCKS_PER_WRITE + block,
                            ));
                        }
                        let offset = base + (chunk * BLOCKS_PER_WRITE * BLOCK) as u64;
                        guest.write(VDISK, offset, &data).unwrap();
                    }
                    guest.flush().unwrap();
                }
            });
        }
    });
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    churn.join().unwrap();

    // Force the reclaim path over everything the rounds orphaned.
    a.collect_garbage().unwrap();
    b.collect_garbage().unwrap();

    // Every writer's final round, byte for byte, on both members.
    let reader_a = a.guest();
    let reader_b = b.guest();
    for writer in 0..WRITERS {
        let base = writer as u64 * stripe;
        for chunk in 0..WRITES_PER_ROUND {
            for block in 0..BLOCKS_PER_WRITE {
                let expected = payload(writer, ROUNDS - 1, chunk * BLOCKS_PER_WRITE + block);
                let offset = base + ((chunk * BLOCKS_PER_WRITE + block) * BLOCK) as u64;
                let got_a = reader_a.read(VDISK, offset, BLOCK as u64).unwrap();
                assert_eq!(
                    got_a, expected,
                    "writer {writer} chunk {chunk} block {block} on A"
                );
                let got_b = reader_b.read(VDISK, offset, BLOCK as u64).unwrap();
                assert_eq!(
                    got_b, expected,
                    "writer {writer} chunk {chunk} block {block} on B"
                );
            }
        }
    }

    // The platters themselves: every record verifies, every reference
    // resolves.
    for (name, daemon) in [("A", &a), ("B", &b)] {
        let total = daemon.start_scrub().unwrap();
        wait_until(&format!("node {name}'s scrub"), || {
            !daemon.scrub_progress().0
        });
        let (_, _, _, last) = daemon.scrub_progress();
        let (_, verified, corrupt, missing) = last.expect("the scrub finished");
        assert_eq!(corrupt, 0, "node {name}: scrub found corrupt records");
        assert_eq!(missing, 0, "node {name}: scrub found missing references");
        assert!(
            verified >= total.min(1),
            "node {name}: scrub verified nothing"
        );
    }

    b.shutdown();
    a.shutdown();
}
