//! The measurement rig behind the vSAN performance work: a real synced
//! pair over loopback, timed at the [`GuestHandle`] — the exact surface
//! the ublk export calls, with replication, leases, and the flush
//! contract all live underneath.
//!
//! Not a test of correctness and not run by default: `#[ignore]`d, run by
//! hand in release mode when a change wants a number:
//!
//! ```text
//! cargo test --release --test perf -- --ignored --nocapture
//! ```
//!
//! The numbers are for COMPARISON, before a change and after it, on the
//! same machine in the same sitting. They are not the hardware's numbers:
//! the bricks are files on whatever filesystem tempdir lands on, and the
//! peer link is loopback — so what this isolates is everything pool-bench
//! measured minus the disks and the wire: engine CPU, lock behaviour,
//! per-block protocol costs, and stalls the engine inflicts on its
//! callers. pool-bench.sh on a node remains the truth about the hardware.

use std::time::{Duration, Instant};

use lumen_fsd::daemon::GuestHandle;
use lumen_fsd::{format_brick, Config, Daemon};

// Room to spare, but not extravagance: a brick tight around the vdisk
// fills mid-measurement, and a full PEER brick refuses the replication
// message and bounces the session — a real finding (the peer apply path
// has no make-room retry, unlike the guest path's `with_room`), but not
// this rig's subject. Two of these must also fit the machine's tempdir.
const DISK_BYTES: u64 = 3 << 30;
const VDISK_BYTES: u64 = 256 << 20;
const POOL: [u8; 16] = [0x9E; 16];
/// The vdisk the format mints, same as the nbd bootstrap's.
const VDISK: u64 = 1;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let mut path = std::env::temp_dir();
        path.push(format!("lumen-fsd-perf-{}-{name}.brick", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn synced_pair() -> (Daemon, Daemon, Scratch, Scratch) {
    let brick_a = Scratch::new("a");
    let brick_b = Scratch::new("b");
    for (brick, uuid) in [(&brick_a, [0xE0; 16]), (&brick_b, [0xE1; 16])] {
        format_brick(
            &brick.0,
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
        listen: Some("127.0.0.1:0".parse().unwrap()),
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while a.status().state != lumen_fs::ReplState::Synced
        || b.status().state != lumen_fs::ReplState::Synced
    {
        assert!(Instant::now() < deadline, "the pair never synced");
        std::thread::sleep(Duration::from_millis(10));
    }
    (a, b, brick_a, brick_b)
}

/// Deterministic bytes that neither dedupe nor a zero-detector can
/// flatter, without a rand dependency: an LCG stirred into every word.
fn fill_random(buffer: &mut [u8], seed: &mut u64) {
    for chunk in buffer.chunks_mut(8) {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = seed.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// A pseudo-random aligned offset inside the vdisk for `len`-byte ops.
fn random_offset(len: u64, seed: &mut u64) -> u64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let slots = VDISK_BYTES / len;
    (*seed >> 16) % slots * len
}

fn mbps(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / 1e6 / elapsed.as_secs_f64()
}

fn iops(ops: u64, elapsed: Duration) -> f64 {
    ops as f64 / elapsed.as_secs_f64()
}

/// One timed pass of `op`, run until the budget elapses; returns (ops, time).
fn run_for(budget: Duration, mut op: impl FnMut(u64)) -> (u64, Duration) {
    let started = Instant::now();
    let mut ops = 0u64;
    while started.elapsed() < budget {
        op(ops);
        ops += 1;
    }
    (ops, started.elapsed())
}

fn per_op(elapsed: Duration, ops: u64) -> Duration {
    if ops == 0 {
        return Duration::ZERO;
    }
    elapsed / ops as u32
}

#[test]
#[ignore = "a measurement, not a test: cargo test --release --test perf -- --ignored --nocapture"]
fn measure_the_guest_path() {
    let budget = Duration::from_secs(5);
    let (a, _b, _ba, _bb) = synced_pair();
    let guest: GuestHandle = a.guest();
    guest.claim_writer(VDISK).unwrap();
    let mut seed = 0x517E_C0DEu64;

    // Fill first, sequentially — 1M at a time, the whole vdisk, so reads
    // read data rather than the zero fast-path, exactly as pool-bench
    // preconditions the scratch device.
    let mut big = vec![0u8; 1 << 20];
    let fill_started = Instant::now();
    let mut offset = 0u64;
    while offset < VDISK_BYTES {
        fill_random(&mut big, &mut seed);
        guest.write(VDISK, offset, &big).unwrap();
        offset += big.len() as u64;
    }
    guest.flush().unwrap();
    let fill_elapsed = fill_started.elapsed();
    eprintln!(
        "fill      1M seq write {:>8.1} MB/s   ({:.0?}/op incl. final flush)",
        mbps(VDISK_BYTES, fill_elapsed),
        per_op(fill_elapsed, VDISK_BYTES / big.len() as u64),
    );

    // Sequential overwrite: steady state, past first allocation.
    let (ops, elapsed) = run_for(budget, |i| {
        fill_random(&mut big, &mut seed);
        let offset = i * (1 << 20) % VDISK_BYTES;
        guest.write(VDISK, offset, &big).unwrap();
    });
    eprintln!(
        "seqwrite  1M overwrite {:>9.1} MB/s   ({:.0?}/op)",
        mbps(ops << 20, elapsed),
        per_op(elapsed, ops),
    );

    // Sequential read, 1M ops.
    let (ops, elapsed) = run_for(budget, |i| {
        let offset = i * (1 << 20) % VDISK_BYTES;
        let data = guest.read(VDISK, offset, 1 << 20).unwrap();
        assert_eq!(data.len(), 1 << 20);
    });
    eprintln!(
        "seqread   1M read      {:>9.1} MB/s   ({:.0?}/op)",
        mbps(ops << 20, elapsed),
        per_op(elapsed, ops),
    );

    // Random 4k reads — the op pool-bench showed healthy; the baseline the
    // others are judged against.
    let (ops, elapsed) = run_for(budget, |_| {
        let offset = random_offset(4096, &mut seed);
        guest.read(VDISK, offset, 4096).unwrap();
    });
    eprintln!(
        "randread  4k           {:>9.0} iops   ({:.0?}/op)",
        iops(ops, elapsed),
        per_op(elapsed, ops),
    );

    // Random 4k writes, no flush: the sustained-write cap pool-bench saw
    // at 411 iops. WAL pressure and checkpoint stalls land HERE.
    let mut small = vec![0u8; 4096];
    let (ops, elapsed) = run_for(budget, |_| {
        fill_random(&mut small, &mut seed);
        let offset = random_offset(4096, &mut seed);
        guest.write(VDISK, offset, &small).unwrap();
    });
    eprintln!(
        "randwrite 4k           {:>9.0} iops   ({:.0?}/op)",
        iops(ops, elapsed),
        per_op(elapsed, ops),
    );

    // Write + flush at queue depth 1: the durability path, fsync's analog.
    let (ops, elapsed) = run_for(budget, |_| {
        fill_random(&mut small, &mut seed);
        let offset = random_offset(4096, &mut seed);
        guest.write(VDISK, offset, &small).unwrap();
        guest.flush().unwrap();
    });
    eprintln!(
        "syncwrite 4k + flush   {:>9.0} iops   ({:.0?}/op)",
        iops(ops, elapsed),
        per_op(elapsed, ops),
    );

    let (sent, durable, applied) = a.status().stream;
    eprintln!("stream    sent={sent} durable={durable} applied={applied}");
}
