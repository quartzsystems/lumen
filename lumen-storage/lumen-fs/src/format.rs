//! The on-disk shapes, version 1: superblock, segment header, block record.
//!
//! Layout of a brick:
//!
//! ```text
//!   sector 0        superblock, slot A ─┐ two slots, alternate generations;
//!   sector 1        superblock, slot B ─┘ the valid one with the highest
//!                                          generation wins
//!   segment area    fixed-size segments, back to back to the end of disk
//! ```
//!
//! And of a segment:
//!
//! ```text
//!   sector 0        segment header: seq (incarnation), brick uuid
//!   then            block records, each sector-aligned:
//!                     64-byte record header + payload, zero-padded
//! ```
//!
//! Three rules make recovery a plain forward scan with no journal:
//!
//! - **Everything self-validates.** The superblock and segment header carry
//!   a hash of their own bytes; a record's payload is checked against the
//!   content address in its header, and the header against its own 8-byte
//!   check. A torn write is not a special case — it is just bytes that fail
//!   their check.
//! - **The seq pins records to their segment incarnation.** Every record
//!   repeats the seq of the segment header it was written under. A reused
//!   segment gets a higher seq, so leftover records from the previous
//!   incarnation fail the seq comparison and end the scan — stale data
//!   cannot masquerade as current.
//! - **The scan stops at the first invalid record.** Everything before it
//!   was flushed or happened to survive; everything after it was never
//!   acknowledged (the flush barrier orders writes), so stopping loses
//!   nothing a caller was promised.
//!
//! Fixed little-endian layouts, hand-encoded. No serde: the bytes on disk
//! are a contract with future versions of this code, not a serialization
//! detail.

use crate::error::{FsError, Result};
use crate::hash::{full_check, header_check, BlockHash};

/// The atomic-looking unit the layout aligns to. The simulator deliberately
/// tears *inside* sectors too, so nothing below actually assumes sector
/// atomicity — alignment is for real-device performance and simple math.
pub const SECTOR_SIZE: u64 = 4096;

/// Two superblock copies, so one torn superblock write cannot orphan a brick.
pub const SUPERBLOCK_SLOTS: u64 = 2;

pub const FORMAT_VERSION: u32 = 1;

const SUPERBLOCK_MAGIC: &[u8; 8] = b"LUMENFS\0";
const SEGMENT_MAGIC: &[u8; 8] = b"LFSSEG\0\0";
const RECORD_MAGIC: &[u8; 8] = b"LFSBLK\0\0";

/// Where the segment area begins: right after the superblock slots.
pub const SEGMENT_AREA_START: u64 = SECTOR_SIZE * SUPERBLOCK_SLOTS;

/// Encoded sizes. The structs occupy the front of their sector; the rest is
/// zeros.
const SUPERBLOCK_LEN: usize = 112;
const SEGMENT_HEADER_LEN: usize = 72;
pub const RECORD_HEADER_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Little-endian field helpers over a fixed buffer.

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(buf[at..at + 4].try_into().unwrap())
}

fn get_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Superblock

/// The brick's identity and geometry. Written at format time and thereafter
/// only when the geometry story changes (never, in this version).
///
/// Layout (little-endian):
/// ```text
///   0..8    magic "LUMENFS\0"
///   8..12   version                u32
///   12..16  block_size             u32   max payload bytes per block
///   16..24  segment_size           u64   bytes per segment, sector multiple
///   24..32  segment_area_start     u64
///   32..40  segment_count          u64
///   40..48  generation             u64   highest valid generation wins
///   48..64  pool_uuid              16 bytes
///   64..80  brick_uuid             16 bytes
///   80..112 checksum               BLAKE3 of bytes 0..80
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    pub segment_size: u64,
    pub segment_area_start: u64,
    pub segment_count: u64,
    pub generation: u64,
    pub pool_uuid: [u8; 16],
    pub brick_uuid: [u8; 16],
}

impl Superblock {
    pub fn encode(&self) -> [u8; SUPERBLOCK_LEN] {
        let mut buf = [0u8; SUPERBLOCK_LEN];
        buf[0..8].copy_from_slice(SUPERBLOCK_MAGIC);
        put_u32(&mut buf, 8, FORMAT_VERSION);
        put_u32(&mut buf, 12, self.block_size);
        put_u64(&mut buf, 16, self.segment_size);
        put_u64(&mut buf, 24, self.segment_area_start);
        put_u64(&mut buf, 32, self.segment_count);
        put_u64(&mut buf, 40, self.generation);
        buf[48..64].copy_from_slice(&self.pool_uuid);
        buf[64..80].copy_from_slice(&self.brick_uuid);
        let check = full_check(&buf[0..80]);
        buf[80..112].copy_from_slice(&check);
        buf
    }

    /// Decode one slot. `Ok(None)` is "this slot holds no valid superblock"
    /// — an ordinary answer during open; the *version* mismatch is an error
    /// because a valid-but-newer superblock must stop the mount, not read as
    /// blank.
    pub fn decode(buf: &[u8]) -> Result<Option<Superblock>> {
        if buf.len() < SUPERBLOCK_LEN || &buf[0..8] != SUPERBLOCK_MAGIC {
            return Ok(None);
        }
        if full_check(&buf[0..80]) != buf[80..112] {
            return Ok(None);
        }
        let version = get_u32(buf, 8);
        if version != FORMAT_VERSION {
            return Err(FsError::UnsupportedVersion(version));
        }
        let mut pool_uuid = [0u8; 16];
        pool_uuid.copy_from_slice(&buf[48..64]);
        let mut brick_uuid = [0u8; 16];
        brick_uuid.copy_from_slice(&buf[64..80]);
        Ok(Some(Superblock {
            block_size: get_u32(buf, 12),
            segment_size: get_u64(buf, 16),
            segment_area_start: get_u64(buf, 24),
            segment_count: get_u64(buf, 32),
            generation: get_u64(buf, 40),
            pool_uuid,
            brick_uuid,
        }))
    }

    /// Byte offset of a segment's first sector.
    pub fn segment_offset(&self, index: u64) -> u64 {
        self.segment_area_start + index * self.segment_size
    }
}

// ---------------------------------------------------------------------------
// Segment header

/// One segment incarnation. Written when the segment is opened for writing;
/// every record inside repeats `seq`.
///
/// Layout (little-endian):
/// ```text
///   0..8    magic "LFSSEG\0\0"
///   8..16   seq            u64   incarnation, monotonic per brick, ≥ 1
///   16..24  segment_index  u64   must match the position it was read from
///   24..40  brick_uuid     16 bytes — a foreign or pre-reformat header fails
///   40..72  checksum       BLAKE3 of bytes 0..40
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeader {
    pub seq: u64,
    pub segment_index: u64,
    pub brick_uuid: [u8; 16],
}

impl SegmentHeader {
    pub fn encode(&self) -> [u8; SEGMENT_HEADER_LEN] {
        let mut buf = [0u8; SEGMENT_HEADER_LEN];
        buf[0..8].copy_from_slice(SEGMENT_MAGIC);
        put_u64(&mut buf, 8, self.seq);
        put_u64(&mut buf, 16, self.segment_index);
        buf[24..40].copy_from_slice(&self.brick_uuid);
        let check = full_check(&buf[0..40]);
        buf[40..72].copy_from_slice(&check);
        buf
    }

    /// `None` for anything that is not a valid header for *this* brick at
    /// *this* position — torn, stale, foreign, or blank all read the same
    /// way: the segment is unused.
    pub fn decode(buf: &[u8], expect_index: u64, brick_uuid: &[u8; 16]) -> Option<SegmentHeader> {
        if buf.len() < SEGMENT_HEADER_LEN || &buf[0..8] != SEGMENT_MAGIC {
            return None;
        }
        if full_check(&buf[0..40]) != buf[40..72] {
            return None;
        }
        let seq = get_u64(buf, 8);
        let segment_index = get_u64(buf, 16);
        if seq == 0 || segment_index != expect_index || &buf[24..40] != brick_uuid {
            return None;
        }
        Some(SegmentHeader {
            seq,
            segment_index,
            brick_uuid: *brick_uuid,
        })
    }
}

// ---------------------------------------------------------------------------
// Block record

/// One stored block: a 64-byte header followed by the payload, zero-padded
/// to the next sector boundary.
///
/// Layout (little-endian):
/// ```text
///   0..8    magic "LFSBLK\0\0"
///   8..16   seq           u64   must equal the segment header's seq
///   16..20  length        u32   payload bytes, 1..=block_size
///   20      flags         u8    reserved, 0
///   21      codec         u8    reserved for compression (docs/lumenfs.md);
///                               0 = raw
///   22..24  pad
///   24..56  hash          the content address; also the payload checksum
///   56..64  header_check  first 8 bytes of BLAKE3 of bytes 0..56
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordHeader {
    pub seq: u64,
    pub length: u32,
    pub hash: BlockHash,
}

impl RecordHeader {
    pub fn encode(&self) -> [u8; RECORD_HEADER_LEN] {
        let mut buf = [0u8; RECORD_HEADER_LEN];
        buf[0..8].copy_from_slice(RECORD_MAGIC);
        put_u64(&mut buf, 8, self.seq);
        put_u32(&mut buf, 16, self.length);
        // flags, codec, pad already zero
        buf[24..56].copy_from_slice(self.hash.as_bytes());
        let check = header_check(&buf[0..56]);
        buf[56..64].copy_from_slice(&check);
        buf
    }

    /// `None` for anything that is not a valid record header written under
    /// `expect_seq` — which is exactly the scan's stop condition.
    pub fn decode(buf: &[u8], expect_seq: u64, block_size: u32) -> Option<RecordHeader> {
        if buf.len() < RECORD_HEADER_LEN || &buf[0..8] != RECORD_MAGIC {
            return None;
        }
        if header_check(&buf[0..56]) != buf[56..64] {
            return None;
        }
        let seq = get_u64(buf, 8);
        let length = get_u32(buf, 16);
        if seq != expect_seq || length == 0 || length > block_size {
            return None;
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&buf[24..56]);
        Some(RecordHeader {
            seq,
            length,
            hash: BlockHash::from_bytes(hash),
        })
    }
}

/// Bytes a record occupies on disk: header plus payload, rounded up to the
/// sector boundary the next record starts on.
pub fn record_span(payload_len: u32) -> u64 {
    let raw = RECORD_HEADER_LEN as u64 + payload_len as u64;
    raw.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_block;

    fn sample_superblock() -> Superblock {
        Superblock {
            block_size: 16384,
            segment_size: 1 << 20,
            segment_area_start: SEGMENT_AREA_START,
            segment_count: 7,
            generation: 3,
            pool_uuid: [1; 16],
            brick_uuid: [2; 16],
        }
    }

    #[test]
    fn a_superblock_round_trips_exactly() {
        let sb = sample_superblock();
        let decoded = Superblock::decode(&sb.encode()).unwrap().unwrap();
        assert_eq!(sb, decoded);
    }

    #[test]
    fn a_single_flipped_superblock_bit_reads_as_no_superblock() {
        let mut buf = sample_superblock().encode();
        for bit_of in [0usize, 9, 17, 50, 79, 100] {
            buf[bit_of] ^= 0x01;
            assert_eq!(Superblock::decode(&buf).unwrap(), None, "byte {bit_of}");
            buf[bit_of] ^= 0x01;
        }
    }

    #[test]
    fn a_valid_superblock_from_the_future_is_an_error_not_a_blank() {
        let mut buf = sample_superblock().encode();
        put_u32(&mut buf, 8, FORMAT_VERSION + 1);
        let check = full_check(&buf[0..80]);
        buf[80..112].copy_from_slice(&check);
        assert_eq!(
            Superblock::decode(&buf).unwrap_err(),
            FsError::UnsupportedVersion(FORMAT_VERSION + 1)
        );
    }

    #[test]
    fn a_segment_header_round_trips_and_pins_index_and_uuid() {
        let header = SegmentHeader {
            seq: 9,
            segment_index: 4,
            brick_uuid: [7; 16],
        };
        let buf = header.encode();
        assert_eq!(SegmentHeader::decode(&buf, 4, &[7; 16]), Some(header));
        // The same bytes read at another position, or by another brick, are
        // not a header at all.
        assert_eq!(SegmentHeader::decode(&buf, 5, &[7; 16]), None);
        assert_eq!(SegmentHeader::decode(&buf, 4, &[8; 16]), None);
    }

    #[test]
    fn a_record_header_round_trips_and_pins_its_seq() {
        let header = RecordHeader {
            seq: 12,
            length: 100,
            hash: hash_block(b"payload"),
        };
        let buf = header.encode();
        assert_eq!(RecordHeader::decode(&buf, 12, 16384), Some(header));
        // A stale incarnation's record ends the scan.
        assert_eq!(RecordHeader::decode(&buf, 13, 16384), None);
        // A length beyond the pool's block size is not a record.
        assert_eq!(RecordHeader::decode(&buf, 12, 64), None);
    }

    #[test]
    fn a_record_span_is_sector_aligned_and_minimal() {
        assert_eq!(record_span(1), SECTOR_SIZE);
        assert_eq!(
            record_span((SECTOR_SIZE - RECORD_HEADER_LEN as u64) as u32),
            SECTOR_SIZE
        );
        assert_eq!(
            record_span((SECTOR_SIZE - RECORD_HEADER_LEN as u64 + 1) as u32),
            2 * SECTOR_SIZE
        );
        assert_eq!(record_span(16384), 5 * SECTOR_SIZE);
    }
}
