//! The phase-1 smoke tool: a LumenFS pool on a real file, served as an NBD
//! export — so the engine can back an actual block device (`nbd-client`,
//! `qemu-nbd --connect`, or qemu directly) and take a real filesystem on a
//! real machine.
//!
//! This is deliberately a smoke tool and not the daemon. The daemon
//! (lumen-fsd, a later stage) owns io_uring, ublk, policy, and peers; this
//! binary owns exactly enough to prove the engine against reality: a
//! std-only NBD server (fixed-newstyle handshake, one client at a time)
//! over the byte view the library already tests under simulation, and a
//! file-backed [`Disk`] whose flush is fsync. Everything that can corrupt
//! data lives in the library; this file is wire plumbing.
//!
//! ```text
//!   lumen-fs-nbd format   <file> <disk-bytes> <vdisk-bytes>
//!   lumen-fs-nbd serve    <file> <addr>        # e.g. 127.0.0.1:10809
//!   lumen-fs-nbd scrub    <file>
//!   lumen-fs-nbd workload <file> <seed>        # burn-in; runs until killed
//!   lumen-fs-nbd verify   <file> <seed>
//! ```
//!
//! `workload` and `verify` are the real-hardware half of the simulation:
//! the same durability contract, checked against a device's own fsync
//! instead of a modelled one. See the burn-in section of docs/lumenfs.md
//! and `burn-in.sh`.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_fs::disk::file::{is_block_device, FileDisk};
use lumen_fs::{
    hash_block, Brick, BrickParams, ByteView, Disk, FsError, Pool, Result, SplitMix64, SECTOR_SIZE,
};

// ---------------------------------------------------------------------------
// Commands

const VDISK: u64 = 1;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("format") if args.len() == 5 => cmd_format(&args[2], &args[3], &args[4]),
        Some("serve") if args.len() == 4 => cmd_serve(&args[2], &args[3]),
        Some("scrub") if args.len() == 3 => cmd_scrub(&args[2]),
        Some("info") if args.len() == 3 => cmd_info(&args[2]),
        Some("gc") if args.len() == 3 => cmd_gc(&args[2]),
        Some("workload") if args.len() == 4 => cmd_workload(&args[2], &args[3]),
        Some("verify") if args.len() == 4 => cmd_verify(&args[2], &args[3], "0"),
        Some("verify") if args.len() == 5 => cmd_verify(&args[2], &args[3], &args[4]),
        _ => {
            eprintln!("usage: lumen-fs-nbd format   <file> <disk-bytes> <vdisk-bytes>");
            eprintln!("       lumen-fs-nbd serve    <file> <addr>");
            eprintln!("       lumen-fs-nbd scrub    <file>");
            eprintln!("       lumen-fs-nbd info     <file>   # is there a pool here?");
            eprintln!("       lumen-fs-nbd gc       <file>   # one collection, with the numbers");
            eprintln!("       lumen-fs-nbd workload <file> <seed>   # runs until killed");
            eprintln!("       lumen-fs-nbd verify   <file> <seed> [min-watermark]");
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lumen-fs-nbd: {err}");
            ExitCode::FAILURE
        }
    }
}

fn parse_bytes(text: &str) -> std::result::Result<u64, String> {
    text.parse::<u64>()
        .map_err(|_| format!("not a byte count: {text}"))
}

fn cmd_format(path: &str, disk_bytes: &str, vdisk_bytes: &str) -> std::result::Result<(), String> {
    let disk_bytes = parse_bytes(disk_bytes)?;
    let vdisk_bytes = parse_bytes(vdisk_bytes)?;

    // A raw disk already exists and cannot be resized, so it is formatted
    // in place — the caller is expected to have meant it. Anything else
    // must not exist yet: `create_new` is what keeps a stray argument from
    // eating a file somebody wanted.
    if !is_block_device(Path::new(path)) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| format!("cannot create {path}: {err}"))?;
        file.set_len(disk_bytes)
            .map_err(|err| format!("cannot size {path}: {err}"))?;
        drop(file);
    }
    let disk = FileDisk::open(Path::new(path)).map_err(|err| err.to_string())?;
    let disk_bytes = disk.size();

    // The engine has no randomness by design; the tool is the impure shell,
    // so identity comes from here — the wall clock and pid, hashed.
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = hash_block(format!("lumen-fs {clock} {}", std::process::id()).as_bytes());
    let mut pool_uuid = [0u8; 16];
    let mut brick_uuid = [0u8; 16];
    pool_uuid.copy_from_slice(&seed.as_bytes()[0..16]);
    brick_uuid.copy_from_slice(&seed.as_bytes()[16..32]);

    // Small backing files get small geometry; real ones the design defaults.
    let big = disk_bytes >= 1 << 30;
    let params = BrickParams {
        pool_uuid,
        brick_uuid,
        block_size: 16 * 1024,
        segment_size: if big { 64 << 20 } else { 4 << 20 },
        wal_size: if big { 64 << 20 } else { 8 << 20 },
        tier: 0,
        wal_holder: true,
    };
    let brick = Brick::format(disk, params).map_err(|err| err.to_string())?;
    let mut pool = Pool::create(brick).map_err(|err| err.to_string())?;
    pool.create_vdisk(VDISK, vdisk_bytes, 0)
        .map_err(|err| err.to_string())?;
    pool.checkpoint().map_err(|err| err.to_string())?;
    println!("formatted {path}: {disk_bytes} bytes, vdisk {VDISK} of {vdisk_bytes} bytes");
    Ok(())
}

/// Open the pool and say what is in it — and, by succeeding or not, answer
/// the question a script has to ask before it decides whether formatting
/// would be creating something or destroying something. Opening is the
/// cheapest honest probe there is: a scrub reads and hashes every byte,
/// which is no way to ask "is anything here".
fn cmd_info(path: &str) -> std::result::Result<(), String> {
    let pool = open_pool(path)?;
    println!(
        "pool on {path}: era {}, block size {}, {} vdisk(s), {} snapshot(s)",
        pool.era(),
        pool.block_size(),
        pool.vdisks().len(),
        pool.snapshots().len()
    );
    for (id, size) in pool.vdisks() {
        println!("  vdisk {id}: {size} bytes");
    }
    Ok(())
}

/// Run one collection and say what it cost and bought. Space behaviour is
/// the hardest thing to reason about from the outside, and guessing at it
/// is how a pool ends up collecting far more often than it stores.
fn cmd_gc(path: &str) -> std::result::Result<(), String> {
    let mut pool = open_pool(path)?;
    let before = pool.space();
    let started = SystemTime::now();
    let stats = pool.collect_garbage().map_err(|err| err.to_string())?;
    let elapsed = started.elapsed().map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let after = pool.space();
    println!(
        "segments: {} total, free {} -> {}; live blocks {} -> {}; \
         {} payload MiB live",
        before.segments_total,
        before.segments_free,
        after.segments_free,
        before.blocks,
        after.blocks,
        after.payload_bytes / (1 << 20),
    );
    println!(
        "collection: dropped {}, moved {}, compacted {}, freed {} in {elapsed:.1}s",
        stats.blocks_dropped, stats.blocks_moved, stats.segments_compacted, stats.segments_freed,
    );
    Ok(())
}

fn cmd_scrub(path: &str) -> std::result::Result<(), String> {
    let disk = FileDisk::open(Path::new(path)).map_err(|err| err.to_string())?;
    let pool = Pool::open(Brick::open(disk).map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    let report = pool.scrub().map_err(|err| err.to_string())?;
    println!(
        "{} blocks verified, {} corrupt, {} missing references",
        report.blocks_verified,
        report.corrupt.len(),
        report.missing.len()
    );
    if report.corrupt.is_empty() && report.missing.is_empty() {
        Ok(())
    } else {
        Err("the scrub found damage".into())
    }
}

// ---------------------------------------------------------------------------
// The burn-in pair: a deterministic workload that can be killed at any
// instant, and a verifier that proves the durability contract held.
//
// The trick that makes this work on real hardware, where nothing can be
// written down outside the thing under test, is that the vdisk describes
// its own progress. Block 0 is a **watermark**: the number of operations
// this workload has acknowledged. Each round writes a batch of data
// blocks, flushes them, then writes the new watermark and flushes again.
//
// So at every instant the on-disk state satisfies one rule: **every
// operation at or below the stored watermark must be present and exact.**
// A kill between the two flushes leaves the old watermark and some newer
// data — allowed, because nothing above the watermark was ever promised.
// A kill after the second leaves a watermark whose whole history must be
// there. That is the phase-1 contract, checkable with no side channel, no
// log, and no trust in the process that died.
//
// Operations are derived from `(seed, n)` alone, so the verifier replays
// exactly what the workload did without being told.

/// How many data operations per flushed round.
const BURN_BATCH: u64 = 24;

fn op_roll(seed: u64, n: u64) -> u64 {
    SplitMix64::new(seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)).next_u64()
}

fn op_payload(seed: u64, n: u64, len: usize) -> Vec<u8> {
    let mut rng = SplitMix64::new(seed ^ n ^ 0x5AFE_D00D_5AFE_D00D);
    let mut payload = vec![0u8; len];
    rng.fill(&mut payload);
    payload
}

/// What operation `n` does: a whole-block write, or a trim one time in ten.
/// Block 0 is the watermark, so data lives at 1..blocks.
fn op_target(seed: u64, n: u64, blocks: u64) -> (u64, bool) {
    let roll = op_roll(seed, n);
    let index = 1 + roll % (blocks - 1);
    (index, roll.is_multiple_of(10))
}

fn open_pool(path: &str) -> std::result::Result<Pool<FileDisk>, String> {
    let disk = FileDisk::open(Path::new(path)).map_err(|err| err.to_string())?;
    Pool::open(Brick::open(disk).map_err(|err| err.to_string())?).map_err(|err| err.to_string())
}

fn read_watermark(pool: &mut Pool<FileDisk>) -> std::result::Result<u64, String> {
    let bytes = pool
        .read_bytes(VDISK, 0, 8)
        .map_err(|err| err.to_string())?;
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

fn cmd_workload(path: &str, seed: &str) -> std::result::Result<(), String> {
    let seed = parse_bytes(seed)?;
    let mut pool = open_pool(path)?;
    let block_size = pool.block_size() as u64;
    let size = pool.vdisk_size(VDISK).map_err(|err| err.to_string())?;
    let blocks = size / block_size;
    if blocks < 2 {
        return Err("the vdisk needs at least two blocks for a burn-in".into());
    }
    let mut done = read_watermark(&mut pool)?;
    // The free-segment level at which collecting is still worth asking
    // for; lowered when a collection turns out not to help.
    let mut collect_below = u64::MAX;
    println!("workload: seed {seed}, {blocks} blocks, resuming at {done}; kill me any time");

    loop {
        for n in done + 1..=done + BURN_BATCH {
            let (index, trim) = op_target(seed, n, blocks);
            let at = index * block_size;
            if trim {
                with_room(&mut pool, |pool| pool.trim_bytes(VDISK, at, block_size))
                    .map_err(|err| err.to_string())?;
            } else {
                let payload = op_payload(seed, n, block_size as usize);
                with_room(&mut pool, |pool| pool.write_bytes(VDISK, at, &payload))
                    .map_err(|err| err.to_string())?;
            }
        }
        // The data is durable before the watermark that claims it is. A
        // kill between these two flushes is the interesting case, and it
        // is the one the ordering makes safe.
        pool.flush().map_err(|err| err.to_string())?;
        // Collect on the way down, not at the bottom. Waiting for `Full`
        // means every subsequent write triggers a collection and the pool
        // spends itself collecting — which is how a burn-in that looked
        // like twenty passing rounds was really sixteen wedged ones.
        //
        // And when a collection cannot help — a pool genuinely near full —
        // asking again immediately is the same trap by another road. So a
        // collection that gained nothing lowers the bar it would next be
        // asked at: the pool keeps working until things actually get
        // worse, and `Full` remains the honest end of the line.
        let space = pool.space();
        if space.segments_free < (space.segments_total / 4).min(collect_below) {
            let before = space.segments_free;
            pool.collect_garbage().map_err(|err| err.to_string())?;
            let after = pool.space().segments_free;
            collect_below = if after > before {
                u64::MAX
            } else {
                before.saturating_sub(1)
            };
        }
        done += BURN_BATCH;
        with_room(&mut pool, |pool| {
            pool.write_bytes(VDISK, 0, &done.to_le_bytes())
        })
        .map_err(|err| err.to_string())?;
        pool.flush().map_err(|err| err.to_string())?;
    }
}

fn cmd_verify(path: &str, seed: &str, min_watermark: &str) -> std::result::Result<(), String> {
    let seed = parse_bytes(seed)?;
    let min_watermark = parse_bytes(min_watermark)?;
    let mut pool = open_pool(path)?;
    let block_size = pool.block_size() as u64;
    let size = pool.vdisk_size(VDISK).map_err(|err| err.to_string())?;
    let blocks = size / block_size;
    let watermark = read_watermark(&mut pool)?;

    // The watermark is the pool's own account of what it owes, so a check
    // that trusts it can be talked down: a pool that comes back at a far
    // older state would have its debts forgiven by the very number that
    // shrank. The caller therefore remembers the highest watermark it was
    // ever shown, and a pool below it has lost acknowledged history —
    // whatever it says about itself now.
    if watermark < min_watermark {
        return Err(format!(
            "watermark went backwards: {watermark} now, {min_watermark} already acknowledged \
             — the pool lost history it had promised"
        ));
    }

    // Replay the acknowledged history to learn what each block must hold.
    // `None` is trimmed or never written, which reads as zeros.
    let mut expected: Vec<Option<u64>> = vec![None; blocks as usize];
    for n in 1..=watermark {
        let (index, trim) = op_target(seed, n, blocks);
        expected[index as usize] = if trim { None } else { Some(n) };
    }

    // Operations above the watermark were written but never acknowledged,
    // and the contract lets those land or vanish. A SIGKILL in particular
    // leaves the page cache intact, so they usually *have* landed. Each
    // such block therefore has exactly one alternative legal value.
    //
    // The window is one batch wide, and that bound is what keeps this
    // check honest rather than permissive: the workload cannot begin a
    // batch until it has flushed the watermark for the last one, so it is
    // never more than `BURN_BATCH` operations ahead of the watermark on
    // disk. A resumed run rewrites that same window with the same seed, so
    // nothing older can linger either. Any disagreement outside the window
    // is a broken promise.
    //
    // Outer `None` is "no in-flight operation touched this block"; inner
    // `None` is an in-flight trim, whose legal value is zeros.
    let mut in_flight: Vec<Option<Option<u64>>> = vec![None; blocks as usize];
    for n in watermark + 1..=watermark + BURN_BATCH {
        let (index, trim) = op_target(seed, n, blocks);
        in_flight[index as usize] = Some(if trim { None } else { Some(n) });
    }

    let zeros = vec![0u8; block_size as usize];
    let payload_of = |op: Option<u64>| match op {
        Some(n) => op_payload(seed, n, block_size as usize),
        None => zeros.clone(),
    };

    let mut checked = 0u64;
    let mut unacknowledged = 0u64;
    let mut wrong = Vec::new();
    for index in 1..blocks {
        let found = pool
            .read_bytes(VDISK, index * block_size, block_size)
            .map_err(|err| err.to_string())?;
        if found == payload_of(expected[index as usize]) {
            checked += 1;
            continue;
        }
        match in_flight[index as usize] {
            Some(op) if found == payload_of(op) => unacknowledged += 1,
            _ => wrong.push(index),
        }
        checked += 1;
    }

    let report = pool.scrub().map_err(|err| err.to_string())?;
    println!(
        "verify: watermark={watermark}, {checked} blocks checked, {} wrong, \
         {unacknowledged} carrying unacknowledged writes; \
         scrub {} verified, {} corrupt, {} missing",
        wrong.len(),
        report.blocks_verified,
        report.corrupt.len(),
        report.missing.len()
    );
    if wrong.is_empty() && report.corrupt.is_empty() && report.missing.is_empty() {
        Ok(())
    } else {
        if !wrong.is_empty() {
            let shown: Vec<String> = wrong.iter().take(8).map(u64::to_string).collect();
            eprintln!(
                "acknowledged blocks that did not survive: {}{}",
                shown.join(", "),
                if wrong.len() > 8 { ", …" } else { "" }
            );
        }
        Err("the durability contract was broken".into())
    }
}

fn cmd_serve(path: &str, addr: &str) -> std::result::Result<(), String> {
    let disk = FileDisk::open(Path::new(path)).map_err(|err| err.to_string())?;
    let mut pool = Pool::open(Brick::open(disk).map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    let size = pool.vdisk_size(VDISK).map_err(|err| err.to_string())?;
    let listener = TcpListener::bind(addr).map_err(|err| format!("cannot bind {addr}: {err}"))?;
    println!("serving vdisk {VDISK} ({size} bytes) on nbd://{addr} — one client at a time");

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("accept failed: {err}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        println!("client {peer} connected");
        match serve_client(stream, &mut pool, size) {
            Ok(()) => println!("client {peer} disconnected"),
            Err(err) => eprintln!("client {peer} dropped: {err}"),
        }
        // Settle between clients: the WAL retires, and everything the
        // client wrote is anchored before anyone else connects.
        pool.checkpoint().map_err(|err| err.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// NBD, fixed newstyle. Big-endian wire, std only.

const OPTS_MAGIC: u64 = 0x4948_4156_454F_5054; // "IHAVEOPT"
const REPLY_MAGIC: u64 = 0x0003_e889_0455_65a9;
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

const FLAG_FIXED_NEWSTYLE: u16 = 1;
const FLAG_NO_ZEROES: u16 = 2;

const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_INFO: u32 = 6;
const OPT_GO: u32 = 7;

const REP_ACK: u32 = 1;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = 0x8000_0001;

const INFO_EXPORT: u16 = 0;

/// HAS_FLAGS | SEND_FLUSH | SEND_TRIM.
const TRANSMISSION_FLAGS: u16 = 1 | 4 | 32;

const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;
const CMD_TRIM: u16 = 4;

const EIO: u32 = 5;
const EINVAL: u32 = 22;

/// The largest single request honored — qemu's own default ceiling.
const MAX_REQUEST: u32 = 32 << 20;

fn serve_client(
    mut stream: TcpStream,
    pool: &mut Pool<FileDisk>,
    size: u64,
) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();

    // Handshake.
    stream.write_all(b"NBDMAGIC")?;
    stream.write_all(&OPTS_MAGIC.to_be_bytes())?;
    stream.write_all(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes())?;
    stream.flush()?;
    let client_flags = read_u32(&mut stream)?;
    let no_zeroes = client_flags & FLAG_NO_ZEROES as u32 != 0;

    // Option haggling.
    loop {
        let magic = read_u64(&mut stream)?;
        if magic != OPTS_MAGIC {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let option = read_u32(&mut stream)?;
        let len = read_u32(&mut stream)?;
        if len > MAX_REQUEST {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut data = vec![0u8; len as usize];
        stream.read_exact(&mut data)?;

        match option {
            OPT_EXPORT_NAME => {
                // Whatever name was asked for, there is one export.
                stream.write_all(&size.to_be_bytes())?;
                stream.write_all(&TRANSMISSION_FLAGS.to_be_bytes())?;
                if !no_zeroes {
                    stream.write_all(&[0u8; 124])?;
                }
                stream.flush()?;
                break;
            }
            OPT_GO | OPT_INFO => {
                let mut info = Vec::new();
                info.extend_from_slice(&INFO_EXPORT.to_be_bytes());
                info.extend_from_slice(&size.to_be_bytes());
                info.extend_from_slice(&TRANSMISSION_FLAGS.to_be_bytes());
                option_reply(&mut stream, option, REP_INFO, &info)?;
                option_reply(&mut stream, option, REP_ACK, &[])?;
                if option == OPT_GO {
                    break;
                }
            }
            OPT_ABORT => {
                option_reply(&mut stream, option, REP_ACK, &[])?;
                return Ok(());
            }
            _ => option_reply(&mut stream, option, REP_ERR_UNSUP, &[])?,
        }
    }

    // Transmission.
    loop {
        let magic = read_u32(&mut stream)?;
        if magic != REQUEST_MAGIC {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let _flags = read_u16(&mut stream)?;
        let kind = read_u16(&mut stream)?;
        let cookie = read_u64(&mut stream)?;
        let offset = read_u64(&mut stream)?;
        let length = read_u32(&mut stream)?;

        match kind {
            CMD_READ => {
                if length > MAX_REQUEST {
                    simple_reply(&mut stream, EINVAL, cookie, &[])?;
                    continue;
                }
                match pool.read_bytes(VDISK, offset, length as u64) {
                    Ok(data) => simple_reply(&mut stream, 0, cookie, &data)?,
                    Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
                }
            }
            CMD_WRITE => {
                if length > MAX_REQUEST {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
                let mut data = vec![0u8; length as usize];
                stream.read_exact(&mut data)?;
                let outcome = with_room(pool, |pool| pool.write_bytes(VDISK, offset, &data));
                match outcome {
                    Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                    Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
                }
            }
            CMD_FLUSH => match pool.flush() {
                Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
            },
            CMD_TRIM => {
                let outcome = with_room(pool, |pool| pool.trim_bytes(VDISK, offset, length as u64));
                match outcome {
                    Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                    Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
                }
            }
            CMD_DISC => return Ok(()),
            _ => simple_reply(&mut stream, EINVAL, cookie, &[])?,
        }
    }
}

/// The engine reports pressure and refuses to invent policy: a full ring
/// wants a checkpoint, a full brick wants a collection, and deciding when
/// belongs to whoever is running it. This is the tool's answer — make room
/// and go again.
///
/// Each remedy is tried at most once per operation, so a genuinely full
/// pool surfaces its error instead of spinning. Without the `Full` arm a
/// long-running export eventually fails writes while most of its space is
/// garbage nobody asked to reclaim; the burn-in found that on its third
/// round, which is the kind of thing only a real workload finds.
fn with_room(
    pool: &mut Pool<FileDisk>,
    mut op: impl FnMut(&mut Pool<FileDisk>) -> Result<()>,
) -> Result<()> {
    let mut checkpointed = false;
    let mut collected = false;
    loop {
        match op(pool) {
            Err(FsError::WalFull) if !checkpointed => {
                checkpointed = true;
                pool.checkpoint()?;
            }
            Err(FsError::Full) if !collected => {
                collected = true;
                // A collection checkpoints on the way in, so it cures a
                // tight ring as well as a tight brick.
                checkpointed = true;
                pool.collect_garbage()?;
            }
            other => return other,
        }
    }
}

fn engine_errno(err: &FsError) -> u32 {
    eprintln!("request failed: {err}");
    match err {
        FsError::OutOfRange { .. } | FsError::OutOfBounds { .. } => EINVAL,
        _ => EIO,
    }
}

fn option_reply(
    stream: &mut TcpStream,
    option: u32,
    reply: u32,
    data: &[u8],
) -> std::io::Result<()> {
    stream.write_all(&REPLY_MAGIC.to_be_bytes())?;
    stream.write_all(&option.to_be_bytes())?;
    stream.write_all(&reply.to_be_bytes())?;
    stream.write_all(&(data.len() as u32).to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()
}

fn simple_reply(
    stream: &mut TcpStream,
    error: u32,
    cookie: u64,
    data: &[u8],
) -> std::io::Result<()> {
    stream.write_all(&SIMPLE_REPLY_MAGIC.to_be_bytes())?;
    stream.write_all(&error.to_be_bytes())?;
    stream.write_all(&cookie.to_be_bytes())?;
    stream.write_all(data)?;
    stream.flush()
}

fn read_u16(stream: &mut TcpStream) -> std::io::Result<u16> {
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

fn read_u32(stream: &mut TcpStream) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn read_u64(stream: &mut TcpStream) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

// SECTOR_SIZE is re-exported for geometry sanity in future flags; keep the
// import honest even while unused by the minimal tool.
const _: u64 = SECTOR_SIZE;
