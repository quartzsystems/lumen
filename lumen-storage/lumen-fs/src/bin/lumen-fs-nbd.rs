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
//!   lumen-fs-nbd format <file> <disk-bytes> <vdisk-bytes>
//!   lumen-fs-nbd serve  <file> <addr>          # e.g. 127.0.0.1:10809
//!   lumen-fs-nbd scrub  <file>
//! ```

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use lumen_fs::{hash_block, Brick, BrickParams, Disk, FsError, Pool, Result, SECTOR_SIZE};

// ---------------------------------------------------------------------------
// A real file as a Disk. Flush is fsync — the honest barrier.

struct FileDisk {
    file: File,
    size: u64,
}

impl FileDisk {
    fn open(path: &str) -> std::io::Result<FileDisk> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(FileDisk { file, size })
    }

    fn check(&self, offset: u64, len: usize) -> Result<()> {
        match offset.checked_add(len as u64) {
            Some(end) if end <= self.size => Ok(()),
            _ => Err(FsError::OutOfBounds {
                offset,
                len: len as u64,
                disk_size: self.size,
            }),
        }
    }
}

fn io_failed(err: std::io::Error) -> FsError {
    // The engine's error type has no io variant on purpose (the simulation
    // never fails); at the edge of reality, an io error is the device
    // contradicting itself.
    eprintln!("io error against the backing file: {err}");
    FsError::Corrupt("the backing file failed an i/o")
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.write_all_at(data, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut offset: u64, mut buf: &mut [u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut offset: u64, mut data: &[u8]) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !data.is_empty() {
        let n = file.seek_write(data, offset)?;
        data = &data[n..];
        offset += n as u64;
    }
    Ok(())
}

impl Disk for FileDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(offset, buf.len())?;
        read_at(&self.file, offset, buf).map_err(io_failed)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.check(offset, data.len())?;
        write_at(&self.file, offset, data).map_err(io_failed)
    }

    fn flush(&mut self) -> Result<()> {
        self.file.sync_all().map_err(io_failed)
    }
}

// ---------------------------------------------------------------------------
// Commands

const VDISK: u64 = 1;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let outcome = match args.get(1).map(String::as_str) {
        Some("format") if args.len() == 5 => cmd_format(&args[2], &args[3], &args[4]),
        Some("serve") if args.len() == 4 => cmd_serve(&args[2], &args[3]),
        Some("scrub") if args.len() == 3 => cmd_scrub(&args[2]),
        _ => {
            eprintln!("usage: lumen-fs-nbd format <file> <disk-bytes> <vdisk-bytes>");
            eprintln!("       lumen-fs-nbd serve  <file> <addr>");
            eprintln!("       lumen-fs-nbd scrub  <file>");
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

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("cannot create {path}: {err}"))?;
    file.set_len(disk_bytes)
        .map_err(|err| format!("cannot size {path}: {err}"))?;
    drop(file);
    let disk = FileDisk::open(path).map_err(|err| err.to_string())?;

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
    };
    let brick = Brick::format(disk, params).map_err(|err| err.to_string())?;
    let mut pool = Pool::create(brick).map_err(|err| err.to_string())?;
    pool.create_vdisk(VDISK, vdisk_bytes)
        .map_err(|err| err.to_string())?;
    pool.checkpoint().map_err(|err| err.to_string())?;
    println!("formatted {path}: {disk_bytes} bytes, vdisk {VDISK} of {vdisk_bytes} bytes");
    Ok(())
}

fn cmd_scrub(path: &str) -> std::result::Result<(), String> {
    let disk = FileDisk::open(path).map_err(|err| err.to_string())?;
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

fn cmd_serve(path: &str, addr: &str) -> std::result::Result<(), String> {
    let disk = FileDisk::open(path).map_err(|err| err.to_string())?;
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
                let outcome = with_wal_room(pool, |pool| pool.write_bytes(VDISK, offset, &data));
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
                let outcome =
                    with_wal_room(pool, |pool| pool.trim_bytes(VDISK, offset, length as u64));
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

/// A full ring is the engine asking for a checkpoint; the tool is the
/// policy layer, and its policy is: make room and go again.
fn with_wal_room(
    pool: &mut Pool<FileDisk>,
    mut op: impl FnMut(&mut Pool<FileDisk>) -> Result<()>,
) -> Result<()> {
    match op(pool) {
        Err(FsError::WalFull) => {
            pool.checkpoint()?;
            op(pool)
        }
        other => other,
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
