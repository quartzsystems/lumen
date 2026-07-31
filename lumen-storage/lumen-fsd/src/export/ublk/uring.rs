//! A minimal io_uring, exactly wide enough for ublk: submit
//! `IORING_OP_URING_CMD` requests and collect their completions.
//!
//! Hand-rolled over libc rather than a binding crate for the same reason
//! the wire formats are hand-rolled: the surface actually used is tiny —
//! setup, three mmaps, submit, wait — and every byte of it is load-bearing
//! for guest I/O, so it should be readable in one sitting. No registered
//! buffers, no SQPOLL, no linked ops: the queue loop is synchronous by
//! design at this stage, and refinements arrive when measurements ask.
//!
//! Unsafe is confined to this file and follows one discipline: the ring
//! pointers are computed once at setup from kernel-provided offsets, and
//! every load/store across the kernel boundary uses the acquire/release
//! pairs the io_uring documentation specifies.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};

const SYS_IO_URING_SETUP: libc::c_long = 425;
const SYS_IO_URING_ENTER: libc::c_long = 426;

const IORING_SETUP_SQE128: u32 = 1 << 10;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_OP_URING_CMD: u8 = 46;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqringOffsets,
    cq_off: CqringOffsets,
}

/// One completion: the `user_data` the submission carried, and the result.
#[derive(Debug, Clone, Copy)]
pub struct Completion {
    pub user_data: u64,
    pub res: i32,
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn map(fd: RawFd, len: usize, offset: i64) -> io::Result<Mapping> {
        // SAFETY: a fresh shared mapping of the ring fd at a
        // kernel-defined offset; failure is checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                fd,
                offset,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping {
            ptr: ptr.cast(),
            len,
        })
    }

    fn at(&self, offset: u32) -> *mut u8 {
        // SAFETY: offsets come from the kernel's own io_uring_params and
        // are within the mapped length by construction.
        unsafe { self.ptr.add(offset as usize) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what map() mapped.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

pub struct Uring {
    fd: OwnedFd,
    /// Keeps the rings and SQE array mapped for the struct's lifetime.
    _sq_ring: Mapping,
    _cq_ring: Option<Mapping>,
    sqes: Mapping,
    sqe_len: usize,
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_array: *mut u32,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const u8,
    /// SQEs written since the last enter.
    pending: u32,
}

// SAFETY: the ring is used from one thread at a time (the queue loop owns
// its ring; the control path owns its own); Send lets a thread carry it.
unsafe impl Send for Uring {}

impl Uring {
    /// `sqe128` widens each SQE's command area to 80 bytes. ublk needs it
    /// on both of its rings: the 32-byte control commands would not fit a
    /// plain SQE, and the driver refuses data-plane rings without it even
    /// though the 16-byte io command would fit.
    pub fn new(entries: u32, sqe128: bool) -> io::Result<Uring> {
        let mut params = UringParams {
            flags: if sqe128 { IORING_SETUP_SQE128 } else { 0 },
            ..Default::default()
        };
        // SAFETY: io_uring_setup reads the params struct we own.
        let fd =
            unsafe { libc::syscall(SYS_IO_URING_SETUP, entries, &mut params as *mut UringParams) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the syscall returned a fresh fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };

        let sqe_len = if sqe128 { 128 } else { 64 };
        let sq_len = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_len = params.cq_off.cqes as usize + params.cq_entries as usize * 16;

        let single = params.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_ring = Mapping::map(
            fd.as_raw_fd(),
            if single { sq_len.max(cq_len) } else { sq_len },
            IORING_OFF_SQ_RING,
        )?;
        let cq_ring = if single {
            None
        } else {
            Some(Mapping::map(fd.as_raw_fd(), cq_len, IORING_OFF_CQ_RING)?)
        };
        let sqes = Mapping::map(
            fd.as_raw_fd(),
            params.sq_entries as usize * sqe_len,
            IORING_OFF_SQES,
        )?;

        let cq = cq_ring.as_ref().unwrap_or(&sq_ring);
        let uring = Uring {
            sq_head: sq_ring.at(params.sq_off.head).cast(),
            sq_tail: sq_ring.at(params.sq_off.tail).cast(),
            // SAFETY: reading kernel-initialized ring constants.
            sq_mask: unsafe { *sq_ring.at(params.sq_off.ring_mask).cast::<u32>() },
            sq_array: sq_ring.at(params.sq_off.array).cast(),
            cq_head: cq.at(params.cq_off.head).cast(),
            cq_tail: cq.at(params.cq_off.tail).cast(),
            // SAFETY: as above.
            cq_mask: unsafe { *cq.at(params.cq_off.ring_mask).cast::<u32>() },
            cqes: cq.at(params.cq_off.cqes),
            sqe_len,
            sqes,
            _sq_ring: sq_ring,
            _cq_ring: cq_ring,
            fd,
            pending: 0,
        };
        Ok(uring)
    }

    /// Queue one `IORING_OP_URING_CMD`. `payload` is the command struct,
    /// copied into the SQE's command area (16 bytes in a plain SQE, 80 in
    /// a 128-byte one).
    pub fn push_cmd(
        &mut self,
        fd: RawFd,
        cmd_op: u32,
        user_data: u64,
        payload: &[u8],
    ) -> io::Result<()> {
        assert!(
            payload.len() <= self.sqe_len - 48,
            "payload outgrows the SQE"
        );
        // SAFETY: all ring accesses follow the io_uring contract — the
        // kernel only reads entries at or before the tail we publish, so
        // the slot at the unpublished tail is exclusively ours.
        unsafe {
            let head = (*self.sq_head).load(Ordering::Acquire);
            let tail = (*self.sq_tail).load(Ordering::Relaxed);
            if tail.wrapping_sub(head) > self.sq_mask {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "submission ring full",
                ));
            }
            let index = tail & self.sq_mask;
            let sqe = self.sqes.ptr.add(index as usize * self.sqe_len);
            std::ptr::write_bytes(sqe, 0, self.sqe_len);
            *sqe = IORING_OP_URING_CMD; // opcode
            *(sqe.add(4).cast::<i32>()) = fd;
            *(sqe.add(8).cast::<u32>()) = cmd_op;
            *(sqe.add(32).cast::<u64>()) = user_data;
            std::ptr::copy_nonoverlapping(payload.as_ptr(), sqe.add(48), payload.len());
            *self.sq_array.add(index as usize) = index;
            (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release);
        }
        self.pending += 1;
        Ok(())
    }

    /// Submit everything queued and wait for at least `wait_for`
    /// completions.
    pub fn submit_and_wait(&mut self, wait_for: u32) -> io::Result<()> {
        let to_submit = self.pending;
        self.pending = 0;
        // SAFETY: plain syscall on our ring fd.
        let entered = unsafe {
            libc::syscall(
                SYS_IO_URING_ENTER,
                self.fd.as_raw_fd(),
                to_submit,
                wait_for,
                IORING_ENTER_GETEVENTS,
                std::ptr::null_mut::<libc::c_void>(),
                0usize,
            )
        };
        if entered < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                // Interrupted before the wait finished; the submissions
                // went through or will on the next call — retry the wait.
                self.pending = 0;
                return self.submit_and_wait(wait_for);
            }
            return Err(err);
        }
        Ok(())
    }

    /// Pop one completion, if any.
    pub fn next_completion(&mut self) -> Option<Completion> {
        // SAFETY: acquire on the tail makes the CQE contents visible;
        // release on the head returns the slot to the kernel.
        unsafe {
            let head = (*self.cq_head).load(Ordering::Relaxed);
            let tail = (*self.cq_tail).load(Ordering::Acquire);
            if head == tail {
                return None;
            }
            let cqe = self.cqes.add(((head & self.cq_mask) as usize) * 16);
            let user_data = *(cqe.cast::<u64>());
            let res = *(cqe.add(8).cast::<i32>());
            (*self.cq_head).store(head.wrapping_add(1), Ordering::Release);
            Some(Completion { user_data, res })
        }
    }
}
