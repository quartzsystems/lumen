//! The daemon binary: format a brick, or serve one.
//!
//! ```text
//!   lumen-fsd format <file> <disk-bytes> <vdisk-bytes> [pool-uuid-hex]
//!   lumen-fsd serve  <file> <node-id> --listen <addr> [--nbd <addr>] [--control <addr>]
//!   lumen-fsd serve  <file> <node-id> --dial   <addr> [--nbd <addr>] [--control <addr>]
//! ```
//!
//! Two nodes, one pool: format the first brick without a uuid (one is
//! minted and printed), format the second *with* that uuid — the peer
//! handshake refuses anything else. One side `--listen`s, the other
//! `--dial`s.
//!
//! The control socket takes one line per request and answers one line:
//! `status`, `fence-peer`, `checkpoint`, `gc`, `scrub`. `fence-peer` is
//! how the cluster's verdict arrives until lumen-pool wires the real
//! machinery to it; it is the break-glass, and it carries the same weight
//! as `pcs stonith confirm` — saying it about a peer that is not dead is
//! how two writers happen.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_fs::hash_block;
use lumen_fsd::{format_brick, nbd, Config, Daemon};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("format") if args.len() == 5 || args.len() == 6 => cmd_format(
            &args[2],
            &args[3],
            &args[4],
            args.get(5).map(String::as_str),
        ),
        Some("serve") if args.len() >= 5 => cmd_serve(&args[2..]),
        _ => {
            eprintln!("usage: lumen-fsd format <file> <disk-bytes> <vdisk-bytes> [pool-uuid-hex]");
            eprintln!("       lumen-fsd serve  <file> <node-id> --listen <addr> [--nbd <addr>] [--control <addr>]");
            eprintln!("       lumen-fsd serve  <file> <node-id> --dial   <addr> [--nbd <addr>] [--control <addr>]");
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("lumen-fsd: {err}");
            ExitCode::FAILURE
        }
    }
}

fn parse_bytes(text: &str) -> Result<u64, String> {
    text.parse::<u64>()
        .map_err(|_| format!("not a byte count: {text}"))
}

fn parse_uuid(text: &str) -> Result<[u8; 16], String> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return Err(format!("not a 32-hex-digit uuid: {text}"));
    }
    let mut out = [0u8; 16];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let hex = std::str::from_utf8(chunk).unwrap();
        out[i] = u8::from_str_radix(hex, 16).unwrap();
    }
    Ok(out)
}

fn uuid_hex(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fresh identity from the impure shell: wall clock and pid, hashed —
/// the engine itself owns no randomness.
fn fresh_uuid(salt: &str) -> [u8; 16] {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = hash_block(format!("lumen-fsd {salt} {clock} {}", std::process::id()).as_bytes());
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&seed.as_bytes()[0..16]);
    uuid
}

fn cmd_format(
    path: &str,
    disk_bytes: &str,
    vdisk_bytes: &str,
    pool_uuid: Option<&str>,
) -> Result<(), String> {
    let disk_bytes = parse_bytes(disk_bytes)?;
    let vdisk_bytes = parse_bytes(vdisk_bytes)?;
    let pool_uuid = match pool_uuid {
        Some(text) => parse_uuid(text)?,
        None => fresh_uuid("pool"),
    };
    let brick_uuid = fresh_uuid("brick");
    format_brick(
        std::path::Path::new(path),
        disk_bytes,
        vdisk_bytes,
        pool_uuid,
        brick_uuid,
    )?;
    println!(
        "formatted {path}: vdisk {} of {vdisk_bytes} bytes, pool uuid {}",
        nbd::VDISK,
        uuid_hex(&pool_uuid)
    );
    println!("format the peer's brick with that same uuid; the handshake enforces it");
    Ok(())
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    let brick = PathBuf::from(&args[0]);
    let node: u8 = args[1]
        .parse()
        .map_err(|_| format!("not a node id: {}", args[1]))?;
    let mut listen = None;
    let mut dial = None;
    let mut nbd_addr = None;
    let mut control_addr = None;
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        let addr: SocketAddr = value
            .parse()
            .map_err(|_| format!("not an address: {value}"))?;
        match flag.as_str() {
            "--listen" => listen = Some(addr),
            "--dial" => dial = Some(addr),
            "--nbd" => nbd_addr = Some(addr),
            "--control" => control_addr = Some(addr),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }

    let daemon = Daemon::start(Config {
        node,
        brick,
        listen,
        dial,
    })?;
    if let Some(addr) = daemon.peer_addr() {
        println!("peer link listening on {addr}");
    }

    let mut threads = Vec::new();
    if let Some(addr) = nbd_addr {
        let listener =
            TcpListener::bind(addr).map_err(|err| format!("cannot bind nbd {addr}: {err}"))?;
        println!("nbd export on {addr}");
        let guest = daemon.guest();
        threads.push(std::thread::spawn(move || nbd::serve(listener, guest)));
    }
    if let Some(addr) = control_addr {
        let listener =
            TcpListener::bind(addr).map_err(|err| format!("cannot bind control {addr}: {err}"))?;
        println!("control on {addr}");
        serve_control(listener, &daemon);
    } else {
        // No control surface: serve until killed.
        for thread in threads {
            let _ = thread.join();
        }
        loop {
            std::thread::park();
        }
    }
    Ok(())
}

/// One line in, one line out. Trust model: this binds where the operator
/// says and nowhere by default; real authentication arrives with
/// lumen-pool's peer channel, and this surface is the interim break-glass.
fn serve_control(listener: TcpListener, daemon: &Daemon) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(clone) => clone,
            Err(_) => continue,
        });
        let mut stream = stream;
        let mut line = String::new();
        while {
            line.clear();
            matches!(reader.read_line(&mut line), Ok(n) if n > 0)
        } {
            let reply = control_command(daemon, line.trim());
            if stream.write_all(reply.as_bytes()).is_err()
                || stream.write_all(b"\n").is_err()
                || stream.flush().is_err()
            {
                break;
            }
        }
    }
}

fn control_command(daemon: &Daemon, command: &str) -> String {
    match command {
        "status" => {
            let s = daemon.status();
            let leases: Vec<String> = s
                .leases
                .iter()
                .map(|(vdisk, lease)| {
                    format!(
                        "{vdisk}:{}@{}{}",
                        lease.holder,
                        lease.era,
                        match lease.handing_to {
                            Some(to) => format!("->{to}"),
                            None => String::new(),
                        }
                    )
                })
                .collect();
            format!(
                "node {} state {:?} era {} writes {} vdisks {:?} leases [{}] free {}/{}",
                s.node,
                s.state,
                s.era,
                if s.accepts_writes { "open" } else { "held" },
                s.vdisks,
                leases.join(","),
                s.space.segments_free,
                s.space.segments_total,
            )
        }
        "fence-peer" => match daemon.fence_peer() {
            Ok(()) => "ok: continuing alone under the verdict".into(),
            Err(err) => format!("error: {err}"),
        },
        "checkpoint" => match daemon.checkpoint() {
            Ok(()) => "ok".into(),
            Err(err) => format!("error: {err}"),
        },
        "gc" => match daemon.collect_garbage() {
            Ok(stats) => format!(
                "ok: dropped {} moved {} freed {}",
                stats.blocks_dropped, stats.blocks_moved, stats.segments_freed
            ),
            Err(err) => format!("error: {err}"),
        },
        "scrub" => match daemon.scrub() {
            Ok(report) => format!(
                "ok: {} verified, {} corrupt, {} missing",
                report.blocks_verified,
                report.corrupt.len(),
                report.missing.len()
            ),
            Err(err) => format!("error: {err}"),
        },
        _ => "error: unknown command (status, fence-peer, checkpoint, gc, scrub)".into(),
    }
}
