//! The ublk driver's UAPI, transcribed: `include/uapi/linux/ublk_cmd.h`
//! from the 6.12 kernel EL10 ships — the structs the kernel reads out of
//! io_uring command payloads and the mmap'd descriptor array, byte for
//! byte.
//!
//! This module is deliberately platform-independent: the layouts are what
//! the driver defines, and the layout tests at the bottom run on every dev
//! box — a wrong offset here would corrupt guest I/O on lumen1 while
//! passing every simulation, which is exactly the class of bug that must
//! not wait for hardware to be found. The Linux-only plumbing that *uses*
//! these lives next door.
//!
//! Everything uses the ioctl-style command encoding (`UBLK_F_CMD_IOCTL_ENCODE`
//! is set at ADD_DEV), the form modern kernels steer toward; the legacy
//! plain-number encoding is not implemented at all — one encoding, stated,
//! rather than two half-tested ones.

/// The control device every command starts at.
pub const CTRL_PATH: &str = "/dev/ublk-control";

/// Per-device char device (`/dev/ublkc<id>`) — the data plane's handle.
pub fn char_path(dev_id: u32) -> String {
    format!("/dev/ublkc{dev_id}")
}

/// The block device guests actually open (`/dev/ublkb<id>`).
pub fn block_path(dev_id: u32) -> String {
    format!("/dev/ublkb{dev_id}")
}

// ---------------------------------------------------------------------------
// ioctl-style command opcodes: _IOWR('u', nr, struct) exactly as the
// kernel's macros expand them.

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (('u' as u32) << 8) | nr
}

const CTRL_CMD_SIZE: u32 = std::mem::size_of::<CtrlCmd>() as u32;
const IO_CMD_SIZE: u32 = std::mem::size_of::<IoCmd>() as u32;

// Control-plane commands, against /dev/ublk-control.
pub const CMD_GET_DEV_INFO: u32 = ioc(IOC_READ, 0x02, CTRL_CMD_SIZE);
pub const CMD_ADD_DEV: u32 = ioc(IOC_READ | IOC_WRITE, 0x04, CTRL_CMD_SIZE);
pub const CMD_DEL_DEV: u32 = ioc(IOC_READ | IOC_WRITE, 0x05, CTRL_CMD_SIZE);
pub const CMD_START_DEV: u32 = ioc(IOC_READ | IOC_WRITE, 0x06, CTRL_CMD_SIZE);
pub const CMD_STOP_DEV: u32 = ioc(IOC_READ | IOC_WRITE, 0x07, CTRL_CMD_SIZE);
pub const CMD_SET_PARAMS: u32 = ioc(IOC_READ | IOC_WRITE, 0x08, CTRL_CMD_SIZE);

// Data-plane commands, against /dev/ublkc<id>.
pub const IO_FETCH_REQ: u32 = ioc(IOC_READ | IOC_WRITE, 0x20, IO_CMD_SIZE);
pub const IO_COMMIT_AND_FETCH_REQ: u32 = ioc(IOC_READ | IOC_WRITE, 0x21, IO_CMD_SIZE);

// Device flags (ublksrv_ctrl_dev_info.flags).
pub const F_CMD_IOCTL_ENCODE: u64 = 1 << 6;

// What the driver answers in a FETCH/COMMIT completion's `res`.
pub const IO_RES_OK: i32 = 0;

// Request opcodes, low byte of `IoDesc.op_flags`.
pub const IO_OP_READ: u8 = 0;
pub const IO_OP_WRITE: u8 = 1;
pub const IO_OP_FLUSH: u8 = 2;
pub const IO_OP_DISCARD: u8 = 3;
pub const IO_OP_WRITE_ZEROES: u8 = 5;

// ublk_params.types bits.
pub const PARAM_TYPE_BASIC: u32 = 1 << 0;
pub const PARAM_TYPE_DISCARD: u32 = 1 << 1;

// ublk_param_basic.attrs bits. VOLATILE_CACHE is the load-bearing one:
// without it the block layer treats the device as write-through and
// **elides every flush**, so fsync succeeds without the durability
// contract ever engaging. An anonymous `1 << n` here once cost exactly
// that — found as a failover losing the acknowledged tail on lumen1,
// with the daemon's stream counters showing durable=0 after a "successful"
// fsync. The names stay, so the next reader checks them against the
// header instead of trusting an integer.
pub const ATTR_READ_ONLY: u32 = 1 << 0;
pub const ATTR_ROTATIONAL: u32 = 1 << 1;
pub const ATTR_VOLATILE_CACHE: u32 = 1 << 2;
pub const ATTR_FUA: u32 = 1 << 3;

/// Where the per-queue descriptor array sits in the char device's mmap
/// space: queue `q` at `q * MAX_QUEUE_DEPTH * size_of::<IoDesc>()`.
pub const MAX_QUEUE_DEPTH: u64 = 4096;

// ---------------------------------------------------------------------------
// The structs. `#[repr(C)]` with explicit layout tests below; field names
// keep the kernel's meaning under this crate's casing.

/// `struct ublksrv_ctrl_cmd` — the 32-byte payload of every control
/// command, carried inside the io_uring SQE's command area (which is why
/// the control ring needs 128-byte SQEs).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CtrlCmd {
    pub dev_id: u32,
    pub queue_id: u16,
    /// Length of the out-of-line buffer at `addr` (dev_info, params).
    pub len: u16,
    pub addr: u64,
    /// Command-specific inline data — START_DEV carries the daemon pid.
    pub data: u64,
    /// Unprivileged-device path plumbing; unused here, present for layout.
    pub dev_path_len: u16,
    pub pad: u16,
    pub reserved: u32,
}

/// `struct ublksrv_ctrl_dev_info` — what ADD_DEV sends and GET_DEV_INFO
/// returns.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DevInfo {
    pub nr_hw_queues: u16,
    pub queue_depth: u16,
    pub state: u16,
    pub pad0: u16,
    pub max_io_buf_bytes: u32,
    pub dev_id: u32,
    pub ublksrv_pid: i32,
    pub pad1: u32,
    pub flags: u64,
    /// Server-private bits the driver ignores.
    pub ublksrv_flags: u64,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub reserved1: u64,
    pub reserved2: u64,
}

/// `struct ublksrv_io_desc` — one request, written by the driver into the
/// mmap'd descriptor array; the server reads it after a fetch completes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IoDesc {
    /// Low byte: the operation; the rest: request flags.
    pub op_flags: u32,
    pub nr_sectors: u32,
    pub start_sector: u64,
    /// Where the driver put (WRITE) or wants (READ) the data when the
    /// server owns the buffers — unused in our mode, where the buffer
    /// address travels in the fetch command instead.
    pub addr: u64,
}

impl IoDesc {
    pub fn op(&self) -> u8 {
        (self.op_flags & 0xff) as u8
    }
}

/// `struct ublksrv_io_cmd` — the 16-byte payload of FETCH_REQ and
/// COMMIT_AND_FETCH_REQ. Fits in a plain 64-byte SQE's command area.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IoCmd {
    pub q_id: u16,
    pub tag: u16,
    /// COMMIT: bytes transferred, or a negative errno.
    pub result: i32,
    /// The server-owned buffer for this tag; the driver copies request
    /// data in before completing a WRITE fetch and copies out of it after
    /// a READ commit.
    pub addr: u64,
}

/// `struct ublk_param_basic`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ParamBasic {
    pub attrs: u32,
    pub logical_bs_shift: u8,
    pub physical_bs_shift: u8,
    pub io_opt_shift: u8,
    pub io_min_shift: u8,
    pub max_sectors: u32,
    pub chunk_sectors: u32,
    pub dev_sectors: u64,
    pub virt_boundary_mask: u64,
}

/// `struct ublk_param_discard`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ParamDiscard {
    pub discard_alignment: u32,
    pub discard_granularity: u32,
    pub max_discard_sectors: u32,
    pub max_write_zeroes_sectors: u32,
    pub max_discard_segments: u16,
    pub reserved0: u16,
}

/// The head of `struct ublk_params`, with the two parameter blocks this
/// server sets. The kernel reads `len` first and touches only the bytes
/// it covers, so later parameter blocks (devt, zoned, dma, segment) can
/// stay unrepresented until something here needs them.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Params {
    /// Length as sent — [`PARAMS_LEN`], the meaningful prefix, not
    /// `size_of::<Params>()`: repr(C) pads this struct's tail to its
    /// 8-byte alignment, and those pad bytes sit where the kernel's devt
    /// block begins.
    pub len: u32,
    pub types: u32,
    pub basic: ParamBasic,
    pub discard: ParamDiscard,
}

/// The prefix of `ublk_params` that carries basic + discard: two u32s,
/// the 32-byte basic block, the 20-byte discard block.
pub const PARAMS_LEN: u32 = 8 + 32 + 20;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The kernel's sizes, from ublk_cmd.h — a mismatch here is guest
    /// data corruption on real hardware, caught on a laptop instead.
    #[test]
    fn every_struct_matches_the_kernels_layout() {
        assert_eq!(size_of::<CtrlCmd>(), 32);
        assert_eq!(offset_of!(CtrlCmd, addr), 8);
        assert_eq!(offset_of!(CtrlCmd, data), 16);
        assert_eq!(offset_of!(CtrlCmd, dev_path_len), 24);

        assert_eq!(size_of::<DevInfo>(), 64);
        assert_eq!(offset_of!(DevInfo, max_io_buf_bytes), 8);
        assert_eq!(offset_of!(DevInfo, dev_id), 12);
        assert_eq!(offset_of!(DevInfo, ublksrv_pid), 16);
        assert_eq!(offset_of!(DevInfo, flags), 24);
        assert_eq!(offset_of!(DevInfo, ublksrv_flags), 32);
        assert_eq!(offset_of!(DevInfo, owner_uid), 40);

        assert_eq!(size_of::<IoDesc>(), 24);
        assert_eq!(offset_of!(IoDesc, start_sector), 8);
        assert_eq!(offset_of!(IoDesc, addr), 16);

        assert_eq!(size_of::<IoCmd>(), 16);
        assert_eq!(offset_of!(IoCmd, result), 4);
        assert_eq!(offset_of!(IoCmd, addr), 8);

        assert_eq!(size_of::<ParamBasic>(), 32);
        assert_eq!(offset_of!(ParamBasic, dev_sectors), 16);
        assert_eq!(size_of::<ParamDiscard>(), 20);
        assert_eq!(offset_of!(Params, basic), 8);
        assert_eq!(offset_of!(Params, discard), 40);
        // The struct pads to 64; the wire length is the 60-byte prefix.
        assert_eq!(PARAMS_LEN, 60);
        assert!(size_of::<Params>() as u32 >= PARAMS_LEN);
    }

    /// The ioctl numbers, expanded by hand from _IOWR('u', nr, type) —
    /// wrong ones are ENOTTY at runtime on a machine this crate cannot
    /// reach from a test, so they are pinned here as arithmetic.
    #[test]
    fn the_ioctl_encodings_expand_exactly() {
        // dir<<30 | size<<16 | 'u'<<8 | nr, 'u' = 0x75.
        assert_eq!(CMD_ADD_DEV, (3 << 30) | (32 << 16) | 0x7504);
        assert_eq!(CMD_DEL_DEV, (3 << 30) | (32 << 16) | 0x7505);
        assert_eq!(CMD_START_DEV, (3 << 30) | (32 << 16) | 0x7506);
        assert_eq!(CMD_STOP_DEV, (3 << 30) | (32 << 16) | 0x7507);
        assert_eq!(CMD_SET_PARAMS, (3 << 30) | (32 << 16) | 0x7508);
        assert_eq!(CMD_GET_DEV_INFO, (2 << 30) | (32 << 16) | 0x7502);
        assert_eq!(IO_FETCH_REQ, (3 << 30) | (16 << 16) | 0x7520);
        assert_eq!(IO_COMMIT_AND_FETCH_REQ, (3 << 30) | (16 << 16) | 0x7521);
    }
}
