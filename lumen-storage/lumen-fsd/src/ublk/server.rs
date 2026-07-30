//! The ublk export: a vdisk as `/dev/ublkb<id>`, the real guest path.
//!
//! This is what docs/lumenfs.md chose over vhost-user-blk: in-tree driver
//! on EL10's 6.12 kernel, a stable identical device path on every member,
//! and not one line of domain_xml.rs changed. The server side is small on
//! purpose — single queue, synchronous servicing, kernel-copied buffers —
//! because every operation lands on the [`GuestHandle`], where replication,
//! the flush contract, and suspension already live. A suspended node makes
//! the *block device* stall, exactly as a DRBD secondary-less primary
//! would, and a fence verdict releases it.
//!
//! Life cycle: ADD_DEV → SET_PARAMS → the queue thread opens
//! `/dev/ublkc<id>`, mmaps the descriptor array, and primes one FETCH per
//! tag → START_DEV (which completes only once the queue is armed) →
//! requests flow until the device is stopped or deleted, at which point
//! fetches complete with an error and the loop ends.
//!
//! The flush attribute is load-bearing: the device advertises a volatile
//! cache so the kernel *sends* flushes, and a flush completes only when
//! the engine's two-node (or verdict-backed) acknowledgement rule says so.
//! An export that quietly swallowed flushes would pass every casual test
//! and lose acknowledged writes at the first power cut.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lumen_fs::FsError;

use crate::daemon::{Attach, ExportControl, GuestHandle};

use super::uapi::*;
use super::uring::Uring;

/// One queue, thirty-two in flight: the bootstrap posture. Depth and
/// queue count grow when a measurement asks, not before.
const QUEUE_DEPTH: u16 = 32;
/// Largest single request accepted from the kernel.
const MAX_IO_BYTES: u32 = 512 << 10;
const SECTOR: u64 = 512;

fn os_err(what: &str, err: io::Error) -> String {
    format!("{what}: {err}")
}

/// Negative errno from a control completion into a readable error.
fn ctrl_err(what: &str, res: i32) -> String {
    format!("{what}: {}", io::Error::from_raw_os_error(-res))
}

/// The control device: one uring_cmd per verb, synchronous.
struct Ctrl {
    file: std::fs::File,
    ring: Uring,
}

/// The base of every control command. `queue_id` is the no-queue sentinel:
/// ADD_DEV with an explicit device id *requires* it (the driver reads a
/// zero queue_id there as a malformed request and answers EINVAL — found
/// against the real kernel, not the header).
fn ctrl_cmd(dev_id: u32) -> CtrlCmd {
    CtrlCmd {
        dev_id,
        queue_id: 0xFFFF,
        ..Default::default()
    }
}

impl Ctrl {
    fn open() -> Result<Ctrl, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(CTRL_PATH)
            .map_err(|err| os_err(CTRL_PATH, err))?;
        // Control commands are 32 bytes: the ring must carry 128-byte SQEs.
        let ring = Uring::new(4, true).map_err(|err| os_err("io_uring_setup", err))?;
        Ok(Ctrl { file, ring })
    }

    fn cmd(&mut self, cmd_op: u32, cmd: &CtrlCmd) -> Result<(), String> {
        let bytes: [u8; 32] =
            // SAFETY: CtrlCmd is repr(C), 32 bytes, plain data — pinned by
            // the layout tests in uapi.rs.
            unsafe { std::mem::transmute(*cmd) };
        self.ring
            .push_cmd(self.file.as_raw_fd(), cmd_op, 0, &bytes)
            .map_err(|err| os_err("queue control command", err))?;
        self.ring
            .submit_and_wait(1)
            .map_err(|err| os_err("submit control command", err))?;
        let completion = self
            .ring
            .next_completion()
            .ok_or("a control wait returned without a completion")?;
        if completion.res < 0 {
            return Err(ctrl_err("ublk control command", completion.res));
        }
        Ok(())
    }

    fn add_dev(&mut self, dev_id: u32) -> Result<(), String> {
        let info = DevInfo {
            nr_hw_queues: 1,
            queue_depth: QUEUE_DEPTH,
            max_io_buf_bytes: MAX_IO_BYTES,
            dev_id,
            flags: F_CMD_IOCTL_ENCODE,
            ..Default::default()
        };
        self.cmd(
            CMD_ADD_DEV,
            &CtrlCmd {
                len: std::mem::size_of::<DevInfo>() as u16,
                addr: &info as *const DevInfo as u64,
                ..ctrl_cmd(dev_id)
            },
        )
    }

    fn set_params(&mut self, dev_id: u32, size_bytes: u64, block_size: u32) -> Result<(), String> {
        let params = Params {
            len: PARAMS_LEN,
            types: PARAM_TYPE_BASIC | PARAM_TYPE_DISCARD,
            basic: ParamBasic {
                // Volatile cache: the kernel must send flushes, because a
                // flush is the only thing the engine promises on. This was
                // once `1 << 1` — ROTATIONAL — and every flush was elided.
                attrs: ATTR_VOLATILE_CACHE,
                logical_bs_shift: 9,
                physical_bs_shift: 12,
                io_opt_shift: block_size.trailing_zeros() as u8,
                io_min_shift: 12,
                max_sectors: MAX_IO_BYTES / SECTOR as u32,
                dev_sectors: size_bytes / SECTOR,
                ..Default::default()
            },
            discard: ParamDiscard {
                // Trims are honest only at whole-block granularity.
                discard_granularity: block_size,
                max_discard_sectors: MAX_IO_BYTES / SECTOR as u32,
                max_discard_segments: 1,
                ..Default::default()
            },
        };
        self.cmd(
            CMD_SET_PARAMS,
            &CtrlCmd {
                len: PARAMS_LEN as u16,
                addr: &params as *const Params as u64,
                ..ctrl_cmd(dev_id)
            },
        )
    }

    fn start_dev(&mut self, dev_id: u32) -> Result<(), String> {
        self.cmd(
            CMD_START_DEV,
            &CtrlCmd {
                data: std::process::id() as u64,
                ..ctrl_cmd(dev_id)
            },
        )
    }

    fn stop_dev(&mut self, dev_id: u32) -> Result<(), String> {
        self.cmd(CMD_STOP_DEV, &ctrl_cmd(dev_id))
    }

    fn del_dev(&mut self, dev_id: u32) -> Result<(), String> {
        self.cmd(CMD_DEL_DEV, &ctrl_cmd(dev_id))
    }
}

/// A live export: the device, the thread servicing its queue, and the
/// handle whose cancellation releases that thread if it is parked.
pub struct Export {
    vdisk: u64,
    dev_id: u32,
    guest: GuestHandle,
    queue: JoinHandle<Result<(), String>>,
}

impl ExportControl for Export {
    /// The guest device path, and nothing else: callers key this by vdisk
    /// already, and a machine-readable answer beats a prettier one.
    fn describe(&self) -> String {
        block_path(self.dev_id)
    }

    /// Release, stop, join, delete — a sequence in which every step
    /// unblocks the next, and every ordering here was learned by
    /// deadlocking against a real kernel.
    ///
    /// 1. **Cancel first.** STOP_DEV freezes the block queue and waits for
    ///    in-flight requests to complete. A request parked inside the
    ///    engine on a suspension no verdict will ever resolve never
    ///    completes, so STOP_DEV waits forever in
    ///    `blk_mq_freeze_queue_wait` while the guest's bio waits in
    ///    `submit_bio_wait`. Releasing parked I/O first turns those into
    ///    errors, which is the honest answer for a device going away.
    /// 2. **Then STOP_DEV**, which aborts the queue's parked fetches —
    ///    the only thing that ends the servicing loop, so it must precede
    ///    the join.
    /// 3. **Then join**, which is what drops the descriptor mapping.
    /// 4. **DEL_DEV last**: the kernel's delete waits until every
    ///    reference to the char device is gone, mappings included, so
    ///    deleting earlier hangs in `ublk_ctrl_del_dev` — with the
    ///    control surface stuck behind it, since this runs on the thread
    ///    answering the operator.
    fn stop(self: Box<Self>) -> Result<(), String> {
        let mut ctrl = Ctrl::open()?;
        self.guest.cancel();
        let stopped = ctrl.stop_dev(self.dev_id);
        let joined = self
            .queue
            .join()
            .map_err(|_| "the queue thread panicked".to_string())?;
        let deleted = ctrl.del_dev(self.dev_id);
        stopped.and(joined).and(deleted)
    }
}

/// Serve `vdisk` as `/dev/ublkb<dev_id>`, returning once the device is
/// live. The attach decides the pen: an ordinary attach takes the writer
/// lease, and an attach inside a migration window aimed here opens
/// penless — the destination holds the disk open while the source is
/// still writing it, and writes refuse until the handover's accept.
pub fn start(
    guest: GuestHandle,
    vdisk: u64,
    dev_id: u32,
) -> Result<Box<dyn ExportControl>, String> {
    let attach = guest
        .attach(vdisk)
        .map_err(|err| format!("ublk attach refused: {err}"))?;
    let size_bytes = guest.vdisk_size(vdisk).map_err(|err| err.to_string())?;
    let block_size = guest.block_size();

    let mut ctrl = Ctrl::open()?;
    ctrl.add_dev(dev_id)?;
    // From here, failures must tear the half-made device down — an
    // orphaned ublk device outlives the process that meant to serve it.
    let outcome = (|| {
        ctrl.set_params(dev_id, size_bytes, block_size)?;
        let queue = {
            let guest = guest.clone();
            std::thread::spawn(move || queue_loop(guest, vdisk, dev_id))
        };
        ctrl.start_dev(dev_id)?;
        Ok(queue)
    })();
    match outcome {
        Ok(queue) => {
            println!(
                "ublk: vdisk {vdisk} ({size_bytes} bytes, {}) live at {}",
                match attach {
                    Attach::Writer => "writer",
                    Attach::Penless => "penless, awaiting handover",
                },
                block_path(dev_id)
            );
            Ok(Box::new(Export {
                vdisk,
                dev_id,
                guest,
                queue,
            }))
        }
        Err(err) => {
            let _ = ctrl.stop_dev(dev_id);
            let _ = ctrl.del_dev(dev_id);
            Err(err)
        }
    }
}

/// Stop and delete a device — the cleanup verb for a daemon that died
/// without one.
pub fn delete_device(dev_id: u32) -> Result<(), String> {
    let mut ctrl = Ctrl::open()?;
    let stop = ctrl.stop_dev(dev_id);
    let del = ctrl.del_dev(dev_id);
    stop.and(del)
}

/// The driver-written request descriptors, mapped read-only — and
/// unmapped when the queue ends, which is load-bearing rather than tidy.
/// A mapping outlives the fd it came from and keeps its own reference to
/// the char device, so a leaked one makes the kernel's DEL_DEV wait
/// forever for a device nobody is using. Found exactly that way: an
/// `unexport` that never returned, with a kernel thread parked in
/// `ublk_ctrl_del_dev` and the queue thread long since exited.
struct Descriptors {
    ptr: *mut u8,
    len: usize,
}

impl Descriptors {
    fn map(fd: RawFd, len: usize) -> Result<Descriptors, String> {
        // SAFETY: a fresh read-only shared mapping of the char device's
        // descriptor region at its defined offset; failure is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(os_err("mmap descriptors", io::Error::last_os_error()));
        }
        Ok(Descriptors {
            ptr: ptr.cast(),
            len,
        })
    }

    fn at(&self, tag: usize) -> IoDesc {
        // SAFETY: callers pass tag < queue depth, inside the mapping; the
        // driver guarantees a descriptor is stable from the fetch
        // completion that announced it until its commit.
        unsafe { *self.ptr.cast::<IoDesc>().add(tag) }
    }
}

impl Drop for Descriptors {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what map() mapped.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// Wait for udev to surface the char device ADD_DEV created.
fn open_char_dev(dev_id: u32) -> Result<std::fs::File, String> {
    let path = char_path(dev_id);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => return Ok(file),
            Err(err) => {
                if Instant::now() > deadline {
                    return Err(os_err(&path, err));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn queue_loop(guest: GuestHandle, vdisk: u64, dev_id: u32) -> Result<(), String> {
    let depth = QUEUE_DEPTH as usize;
    let char_dev = open_char_dev(dev_id)?;

    // The descriptor array: written by the driver, read here after every
    // fetch completion. Queue 0 sits at offset 0.
    let desc_len = depth * std::mem::size_of::<IoDesc>();
    let desc_len = desc_len.div_ceil(4096) * 4096;
    let descs = Descriptors::map(char_dev.as_raw_fd(), desc_len)?;
    let desc_at = |tag: usize| -> IoDesc { descs.at(tag) };

    // One kernel-copy buffer per tag, address registered in every fetch.
    let mut buffers: Vec<Vec<u8>> = (0..depth)
        .map(|_| vec![0u8; MAX_IO_BYTES as usize])
        .collect();

    // SQE128 even though the 16-byte io command would fit a plain SQE:
    // the driver refuses rings without it.
    let mut ring = Uring::new(depth as u32, true).map_err(|err| os_err("io_uring_setup", err))?;

    let io_cmd = |tag: usize, result: i32, addr: u64| -> [u8; 16] {
        let cmd = IoCmd {
            q_id: 0,
            tag: tag as u16,
            result,
            addr,
        };
        // SAFETY: IoCmd is repr(C), 16 bytes, plain data — layout-tested.
        unsafe { std::mem::transmute(cmd) }
    };

    for tag in 0..depth {
        ring.push_cmd(
            char_dev.as_raw_fd(),
            IO_FETCH_REQ,
            tag as u64,
            &io_cmd(tag, -1, buffers[tag].as_ptr() as u64),
        )
        .map_err(|err| os_err("prime fetch", err))?;
    }

    loop {
        ring.submit_and_wait(1)
            .map_err(|err| os_err("io_uring_enter", err))?;
        while let Some(completion) = ring.next_completion() {
            let tag = completion.user_data as usize;
            if completion.res != IO_RES_OK {
                // The device is stopping (or the driver aborted us):
                // every parked fetch completes with an error and the
                // export is over.
                return Ok(());
            }
            let desc = desc_at(tag);
            let result = service(&guest, vdisk, &desc, &mut buffers[tag]);
            ring.push_cmd(
                char_dev.as_raw_fd(),
                IO_COMMIT_AND_FETCH_REQ,
                tag as u64,
                &io_cmd(tag, result, buffers[tag].as_ptr() as u64),
            )
            .map_err(|err| os_err("queue commit", err))?;
        }
    }
}

/// One request against the replicated guest path. Returns the ublk
/// result: bytes transferred, or a negative errno.
fn service(guest: &GuestHandle, vdisk: u64, desc: &IoDesc, buffer: &mut [u8]) -> i32 {
    let offset = desc.start_sector * SECTOR;
    let len = desc.nr_sectors as u64 * SECTOR;
    if len as usize > buffer.len() {
        return -libc::EINVAL;
    }
    match desc.op() {
        IO_OP_READ => match guest.read(vdisk, offset, len) {
            Ok(data) => {
                buffer[..data.len()].copy_from_slice(&data);
                data.len() as i32
            }
            Err(err) => errno(&err),
        },
        IO_OP_WRITE => match guest.write(vdisk, offset, &buffer[..len as usize]) {
            Ok(()) => len as i32,
            Err(err) => errno(&err),
        },
        IO_OP_FLUSH => match guest.flush() {
            Ok(()) => 0,
            Err(err) => errno(&err),
        },
        IO_OP_DISCARD => match guest.trim(vdisk, offset, len) {
            Ok(()) => 0,
            Err(err) => errno(&err),
        },
        // A write of zeros must *guarantee* zeros, which a trim's
        // advisory whole-block granularity cannot. Dedupe makes the
        // literal answer cheap: every zero block is one stored block.
        IO_OP_WRITE_ZEROES => {
            buffer[..len as usize].fill(0);
            match guest.write(vdisk, offset, &buffer[..len as usize]) {
                Ok(()) => 0,
                Err(err) => errno(&err),
            }
        }
        _ => -libc::EOPNOTSUPP,
    }
}

fn errno(err: &FsError) -> i32 {
    eprintln!("ublk request failed: {err}");
    match err {
        FsError::OutOfRange { .. } | FsError::OutOfBounds { .. } => -libc::EINVAL,
        _ => -libc::EIO,
    }
}
