//! The bootstrap guest export: the daemon's vdisk over NBD.
//!
//! Same fixed-newstyle protocol as the smoke tool, different backend: every
//! operation goes through the [`GuestHandle`], which means writes
//! replicate, flushes wait for the two-node acknowledgement rule, and a
//! suspended node holds requests instead of failing them. NBD stays what
//! docs/lumenfs.md says it is — bootstrap and debugging, never the VM path;
//! ublk takes that seat next.
//!
//! Attach claims: the first thing a client connection does is take the
//! vdisk's writer lease, and a lease the peer holds refuses the client.
//! That is the single-writer guarantee arriving at the door instead of at
//! the first write.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use lumen_fs::FsError;

use crate::daemon::GuestHandle;

/// The one exported vdisk, same as the smoke tool's.
pub const VDISK: u64 = 1;

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

/// Serve the export until the listener dies. One client at a time — the
/// bootstrap posture; concurrency arrives with ublk.
pub fn serve(listener: TcpListener, guest: GuestHandle) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("nbd accept failed: {err}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        println!("nbd client {peer} connected");
        match serve_client(stream, &guest) {
            Ok(()) => println!("nbd client {peer} disconnected"),
            Err(err) => eprintln!("nbd client {peer} dropped: {err}"),
        }
    }
}

fn serve_client(mut stream: TcpStream, guest: &GuestHandle) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();

    // The attach: hold the pen before promising a disk. A lease the peer
    // holds under the current era refuses the client here, at the door.
    if let Err(err) = guest.claim_writer(VDISK) {
        eprintln!("nbd attach refused: {err}");
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let size = guest
        .vdisk_size(VDISK)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::NotFound))?;

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
                match guest.read(VDISK, offset, length as u64) {
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
                match guest.write(VDISK, offset, &data) {
                    Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                    Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
                }
            }
            CMD_FLUSH => match guest.flush() {
                Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
            },
            CMD_TRIM => match guest.trim(VDISK, offset, length as u64) {
                Ok(()) => simple_reply(&mut stream, 0, cookie, &[])?,
                Err(err) => simple_reply(&mut stream, engine_errno(&err), cookie, &[])?,
            },
            CMD_DISC => return Ok(()),
            _ => simple_reply(&mut stream, EINVAL, cookie, &[])?,
        }
    }
}

fn engine_errno(err: &FsError) -> u32 {
    eprintln!("nbd request failed: {err}");
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
