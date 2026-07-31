//! The daemon binary: format a node's bricks, or serve them.
//!
//! ```text
//!   lumen-fsd format <path> --tier <n> [--wal] [--roster <uuid>:<tier>]...
//!                    [--bytes <n>] [--vdisk-bytes <n>] [--pool-uuid <hex>]
//!   lumen-fsd serve  <brick>... --node <id> --listen <addr> [--nbd <addr>] [--control <addr>]
//!   lumen-fsd serve  <brick>... --node <id> --dial   <addr> [--nbd <addr>] [--control <addr>]
//! ```
//!
//! A node's bricks are formatted one invocation each, the WAL holder
//! (`--wal`, always tier 0) **last**: each non-holder prints its identity,
//! the holder's `--roster` names them all, and the anchor the holder
//! writes is what lets `serve` refuse a wrong or partial set by name.
//! Two nodes, one pool: the first format without `--pool-uuid` mints one
//! and prints it, every other brick — both nodes — formats *with* it; the
//! peer handshake refuses anything else. One side `--listen`s, the other
//! `--dial`s.
//!
//! The control socket takes one line per request and answers one line:
//! `status`, `fence-peer`, `checkpoint`, `gc`, `scrub`. `fence-peer` is
//! how the cluster's verdict arrives until lumen-pool wires the real
//! machinery to it; it is the break-glass, and it carries the same weight
//! as `pcs stonith confirm` — saying it about a peer that is not dead is
//! how two writers happen.

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_fs::hash_block;
use lumen_fsd::export::nbd;
use lumen_fsd::{control, format_brick, Config, Daemon};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("format") if args.len() >= 3 => cmd_format(&args[2..]),
        Some("serve") if args.len() >= 4 => cmd_serve(&args[2..]),
        Some("ublk-del") if args.len() == 3 => cmd_ublk_del(&args[2]),
        _ => {
            eprintln!(
                "usage: lumen-fsd format <path> --tier <n> [--wal] [--roster <uuid>:<tier>]... \
                 [--bytes <n>] [--vdisk-bytes <n>] [--pool-uuid <hex>] [--brick-uuid <hex>]"
            );
            eprintln!("       lumen-fsd serve  <brick>... --node <id> --listen <addr> [--nbd <addr>] [--ublk <dev-id>] [--control <addr>]");
            eprintln!("       lumen-fsd serve  <brick>... --node <id> --dial   <addr> [--nbd <addr>] [--ublk <dev-id>] [--control <addr>]");
            eprintln!("       lumen-fsd ublk-del <dev-id>   # clean up after an unclean death");
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

fn cmd_format(args: &[String]) -> Result<(), String> {
    let path = &args[0];
    if path.starts_with("--") {
        return Err("the first argument is the path to format".into());
    }
    let mut tier: Option<u8> = None;
    let mut wal_holder = false;
    let mut roster: Vec<lumen_fs::RosterEntry> = Vec::new();
    let mut create_bytes: Option<u64> = None;
    let mut vdisk_bytes: Option<u64> = None;
    let mut pool_uuid: Option<[u8; 16]> = None;
    let mut brick_uuid: Option<[u8; 16]> = None;
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        if flag == "--wal" {
            wal_holder = true;
            continue;
        }
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--tier" => {
                tier = Some(
                    value
                        .parse()
                        .map_err(|_| format!("not a tier number: {value}"))?,
                )
            }
            "--roster" => {
                let (uuid, entry_tier) = value
                    .split_once(':')
                    .ok_or_else(|| format!("--roster wants <uuid>:<tier>, got {value}"))?;
                roster.push(lumen_fs::RosterEntry {
                    brick_uuid: parse_uuid(uuid)?,
                    tier: entry_tier
                        .parse()
                        .map_err(|_| format!("not a tier number: {entry_tier}"))?,
                });
            }
            "--bytes" => create_bytes = Some(parse_bytes(value)?),
            "--vdisk-bytes" => vdisk_bytes = Some(parse_bytes(value)?),
            "--pool-uuid" => pool_uuid = Some(parse_uuid(value)?),
            // The workflow mints identities up front so the holder's roster
            // is known without parsing this command's stdout.
            "--brick-uuid" => brick_uuid = Some(parse_uuid(value)?),
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    let tier = tier.ok_or("--tier is required: the platter remembers it forever")?;
    if wal_holder && tier != 0 {
        return Err("the WAL holder is always a tier-0 brick".into());
    }
    let pool_uuid = pool_uuid.unwrap_or_else(|| fresh_uuid("pool"));
    let brick_uuid = brick_uuid.unwrap_or_else(|| fresh_uuid("brick"));
    format_brick(
        std::path::Path::new(path),
        create_bytes,
        tier,
        wal_holder,
        roster,
        vdisk_bytes,
        pool_uuid,
        brick_uuid,
    )?;
    println!(
        "formatted {path}: tier {tier}{}, brick uuid {}, pool uuid {}",
        if wal_holder { " (WAL holder)" } else { "" },
        uuid_hex(&brick_uuid),
        uuid_hex(&pool_uuid)
    );
    if let Some(bytes) = vdisk_bytes {
        println!("bootstrap vdisk {} of {bytes} bytes", nbd::VDISK);
    }
    if !wal_holder {
        println!(
            "name this brick in the holder's --roster as {}:{tier}",
            uuid_hex(&brick_uuid)
        );
    }
    println!("every brick of the pool formats with the same pool uuid; the handshake enforces it");
    Ok(())
}

fn cmd_ublk_del(dev_id: &str) -> Result<(), String> {
    let dev_id: u32 = dev_id
        .parse()
        .map_err(|_| format!("not a device id: {dev_id}"))?;
    #[cfg(target_os = "linux")]
    {
        lumen_fsd::export::ublk::delete_device(dev_id)?;
        println!("ublk device {dev_id} stopped and deleted");
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dev_id;
        Err("the ublk export needs a Linux kernel".into())
    }
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    // Leading positional arguments are brick paths — however many disks
    // this node gave the pool; the first flag ends the list.
    let bricks: Vec<PathBuf> = args
        .iter()
        .take_while(|arg| !arg.starts_with("--"))
        .map(PathBuf::from)
        .collect();
    if bricks.is_empty() {
        return Err("serve wants at least one brick path before the flags".into());
    }
    let mut node: Option<u8> = None;
    let mut listen = None;
    let mut dial = None;
    let mut nbd_addr = None;
    let mut control_addr = None;
    let mut ublk_dev: Option<u32> = None;
    let mut rest = args[bricks.len()..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--node" => {
                node = Some(
                    value
                        .parse()
                        .map_err(|_| format!("not a node id: {value}"))?,
                )
            }
            "--ublk" => {
                ublk_dev = Some(
                    value
                        .parse()
                        .map_err(|_| format!("not a device id: {value}"))?,
                )
            }
            _ => {
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
        }
    }
    let node = node.ok_or("--node is required")?;

    let daemon = Daemon::start(Config {
        node,
        bricks,
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
        threads.push(std::thread::spawn(move || {
            nbd::serve(listener, guest, nbd::VDISK)
        }));
    }
    if let Some(dev_id) = ublk_dev {
        // The boot flag is a convenience for the default vdisk; the
        // control surface is where exports are managed for real.
        let device = daemon.export(nbd::VDISK, dev_id)?;
        println!("ublk export: {device}");
    }
    if let Some(addr) = control_addr {
        let listener =
            TcpListener::bind(addr).map_err(|err| format!("cannot bind control {addr}: {err}"))?;
        println!("control on {addr}");
        control::serve(listener, &daemon);
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
