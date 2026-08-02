//! A real file or block device as a [`Disk`] — the one shared piece of
//! reality in this crate.
//!
//! The engine stays a pure state machine; this module is the std edge the
//! impure shells (the smoke tool, the daemon) stand on. It lives in the
//! library rather than in each binary because its few lines carry
//! hard-won lessons that must not be re-learned per shell:
//!
//! - **Size comes from seeking to the end, never from metadata.** A block
//!   device reports zero length in its metadata, and a pool that believes
//!   its disk is zero bytes long refuses every write it is ever asked to
//!   make.
//! - **Flush is `sync_all`** — the honest barrier the durability contract
//!   is stated against.
//! - **A block device is formatted in place; anything else must not exist
//!   yet.** A disk cannot be created and the caller is expected to have
//!   meant it; a stray path argument must never eat a file somebody
//!   wanted. That policy belongs to the shells' `format` commands, but the
//!   [`is_block_device`] probe they share lives here.
//!
//! Nothing here touches clocks or randomness; sockets and io_uring stay
//! in the daemon, exactly as the crate manifest promises.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::Path;

use crate::disk::Disk;
use crate::error::{FsError, Result};

pub struct FileDisk {
    file: File,
    size: u64,
}

/// Whether this path is a raw block device — a disk, not a file that
/// stands in for one. The two are formatted differently and only one of
/// them can be created.
#[cfg(unix)]
pub fn is_block_device(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(path)
        .map(|meta| meta.file_type().is_block_device())
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn is_block_device(_path: &Path) -> bool {
    false
}

impl FileDisk {
    pub fn open(path: &Path) -> std::io::Result<FileDisk> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        // Seeking to the end, rather than asking the metadata: a block
        // device reports a length of zero in its metadata.
        let size = file.seek(SeekFrom::End(0))?;
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
        // sync_data, not sync_all: the durability promise is about the
        // bytes, and a brick's length and metadata never change after
        // format — while sync_all drags the filesystem's metadata journal
        // into every flush, on the hottest path the engine has. On a raw
        // block device the two are the same barrier.
        self.file.sync_data().map_err(io_failed)
    }

    fn flush_handle(&self) -> Option<crate::disk::FlushHandle> {
        // A dup'd descriptor reaches the same open file description and,
        // on a block device, the same write cache — its sync_data is the
        // same barrier `flush` runs, minus the need to hold the disk.
        let file = self.file.try_clone().ok()?;
        Some(Box::new(move || file.sync_data().map_err(io_failed)))
    }
}
