//! The on-disk shapes, version 2: superblock, segment header, block record.
//!
//! Version 2 (phase 4) adds two facts the drive wizard reads off the
//! platter rather than out of a config file: the brick's **tier** (0 = the
//! fastest class, downward from there) and whether this brick **hosts the
//! WAL and anchors** for its node, plus a brick **roster** in the anchor so
//! the set of bricks a node's pool spans is itself durable state. There is
//! no upgrade path from version 1 — a v1 brick is refused by name
//! (`UnsupportedVersion`), and the remedy is a reformat through the pool
//! create workflow. Decided rather than defaulted: the only v1 bricks in
//! existence are lab hardware the workflow recreates anyway.
//!
//! Layout of a brick:
//!
//! ```text
//!   sector 0        superblock, slot A ─┐ two slots; the valid one with
//!   sector 1        superblock, slot B ─┘ the highest generation wins
//!   sector 2        anchor, slot A ─┐ the durable root of everything above
//!   sector 3        anchor, slot B ─┘ the extent store; alternating slots
//!   WAL area        the write-ahead ring (size chosen at format)
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
//! - **The scan keeps what verifies and resyncs past what does not.** An
//!   invalid record does not end the segment: the scan advances to the
//!   next sector boundary and tries again. What it skips is a torn,
//!   never-acknowledged tail or a rotted record for scrub to report; what
//!   it finds beyond a failure is either acknowledged data — which one bad
//!   neighbor must not take down — or an unacknowledged survivor the
//!   durability contract permits in either fate. (Payload bytes that
//!   happen to parse as a record are just a valid block: in a
//!   content-addressed store, any pair of hash and matching payload is
//!   legitimate content, referenced by nothing until a map says so.)
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

pub const FORMAT_VERSION: u32 = 2;

/// Two anchor copies right behind them, same reasoning.
pub const ANCHOR_SLOTS: u64 = 2;

const SUPERBLOCK_MAGIC: &[u8; 8] = b"LUMENFS\0";
const ANCHOR_MAGIC: &[u8; 8] = b"LFSANC\0\0";
const SEGMENT_MAGIC: &[u8; 8] = b"LFSSEG\0\0";
const RECORD_MAGIC: &[u8; 8] = b"LFSBLK\0\0";

/// Where the anchor slots begin: right after the superblock slots.
pub const ANCHOR_AREA_START: u64 = SECTOR_SIZE * SUPERBLOCK_SLOTS;

/// Where the WAL area begins: right after the anchor slots. It ends where
/// the superblock says the segment area starts.
pub const WAL_AREA_START: u64 = ANCHOR_AREA_START + SECTOR_SIZE * ANCHOR_SLOTS;

/// The most bricks one node's anchor can roster. Well beyond any appliance
/// chassis, and it keeps the anchor a fixed shape inside its one sector.
pub const ROSTER_CAP: usize = 16;

/// Superblock flag bit: this brick hosts its node's WAL and anchors.
const FLAG_WAL_HOLDER: u8 = 1;

/// Encoded sizes. The structs occupy the front of their sector; the rest is
/// zeros.
const SUPERBLOCK_LEN: usize = 144;
const ROSTER_ENTRY_LEN: usize = 17;
const ANCHOR_ROSTER_AT: usize = 80;
const ANCHOR_CHECK_AT: usize = ANCHOR_ROSTER_AT + 1 + ROSTER_CAP * ROSTER_ENTRY_LEN;
const ANCHOR_LEN: usize = ANCHOR_CHECK_AT + 32;
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
///   48..56  wal_start              u64
///   56..64  wal_size               u64   0 permitted when not the WAL holder
///   64..80  pool_uuid              16 bytes
///   80..96  brick_uuid             16 bytes
///   96      tier                   u8    0 = the fastest class, downward
///   97      flags                  u8    bit0: this brick hosts WAL + anchors
///   98..112 reserved, zero
///   112..144 checksum              BLAKE3 of bytes 0..112
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    pub segment_size: u64,
    pub segment_area_start: u64,
    pub segment_count: u64,
    pub generation: u64,
    pub wal_start: u64,
    pub wal_size: u64,
    pub pool_uuid: [u8; 16],
    pub brick_uuid: [u8; 16],
    /// The device class this brick was assigned at format: 0 is the fastest
    /// present in the pool, higher numbers slower. The assignment is the
    /// wizard's; the platter remembers it so nothing else has to.
    pub tier: u8,
    /// Whether this brick hosts its node's WAL ring and anchor slots.
    /// Exactly one brick per node says yes, and it is always a tier-0 brick.
    pub wal_holder: bool,
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
        put_u64(&mut buf, 48, self.wal_start);
        put_u64(&mut buf, 56, self.wal_size);
        buf[64..80].copy_from_slice(&self.pool_uuid);
        buf[80..96].copy_from_slice(&self.brick_uuid);
        buf[96] = self.tier;
        buf[97] = if self.wal_holder { FLAG_WAL_HOLDER } else { 0 };
        let check = full_check(&buf[0..112]);
        buf[112..144].copy_from_slice(&check);
        buf
    }

    /// Decode one slot. `Ok(None)` is "this slot holds no valid superblock"
    /// — an ordinary answer during open; a *version* mismatch is an error
    /// because a valid superblock of another version must stop the mount by
    /// name, not read as blank. The version is judged before the checksum:
    /// another version's checksum lives at another offset, so checking ours
    /// first would make every foreign version read as no superblock at all
    /// — exactly the silent answer the error exists to prevent.
    pub fn decode(buf: &[u8]) -> Result<Option<Superblock>> {
        if buf.len() < SUPERBLOCK_LEN || &buf[0..8] != SUPERBLOCK_MAGIC {
            return Ok(None);
        }
        let version = get_u32(buf, 8);
        if version != FORMAT_VERSION {
            return Err(FsError::UnsupportedVersion(version));
        }
        if full_check(&buf[0..112]) != buf[112..144] {
            return Ok(None);
        }
        let mut pool_uuid = [0u8; 16];
        pool_uuid.copy_from_slice(&buf[64..80]);
        let mut brick_uuid = [0u8; 16];
        brick_uuid.copy_from_slice(&buf[80..96]);
        Ok(Some(Superblock {
            block_size: get_u32(buf, 12),
            segment_size: get_u64(buf, 16),
            segment_area_start: get_u64(buf, 24),
            segment_count: get_u64(buf, 32),
            generation: get_u64(buf, 40),
            wal_start: get_u64(buf, 48),
            wal_size: get_u64(buf, 56),
            pool_uuid,
            brick_uuid,
            tier: buf[96],
            wal_holder: buf[97] & FLAG_WAL_HOLDER != 0,
        }))
    }

    /// Byte offset of a segment's first sector.
    pub fn segment_offset(&self, index: u64) -> u64 {
        self.segment_area_start + index * self.segment_size
    }
}

// ---------------------------------------------------------------------------
// Anchor

/// The durable root of everything above the extent store: which manifest
/// block describes the vdisks, and where WAL replay begins. Written by a
/// checkpoint (alternating slots, rising generation) and at format; read at
/// open. The superblock is who the brick *is*; the anchor is where its
/// state *starts*.
///
/// Layout (little-endian):
/// ```text
///   0..8    magic "LFSANC\0\0"
///   8..16   generation          u64   highest valid generation wins
///   16..24  wal_replay_offset   u64   absolute; where replay begins
///   24..32  wal_replay_seq      u64   the seq expected there
///   32..40  wal_epoch           u64   the epoch floor for replay
///   40..48  era                 u64   the data generation (replication)
///   48..80  manifest_hash       the vdisk manifest block; all-zeros = none
///   80      roster_len          u8    bricks this node's pool spans
///   81..353 roster entries      roster_len × (brick_uuid 16 + tier 1);
///                               the unused tail of the area stays zero
///   353..385 checksum           BLAKE3 of bytes 0..353
/// ```
///
/// The era is the replication layer's generation counter — bumped when a
/// survivor continues alone under a fence verdict, adopted by a peer that
/// resyncs. It lives in the anchor because "I went on without my peer" is
/// a fact a crashed survivor must still know at its next open.
///
/// The roster is the durable answer to "which bricks make up this node's
/// pool". It lives in the anchor — rewritten at every checkpoint on the
/// WAL-holding brick — so the set is derivable by reading, and a config
/// file that names a different set is caught at open instead of trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub generation: u64,
    pub wal_replay_offset: u64,
    pub wal_replay_seq: u64,
    pub wal_epoch: u64,
    pub era: u64,
    pub manifest_hash: [u8; 32],
    /// Every brick of this node's pool, the WAL holder included. At most
    /// [`ROSTER_CAP`]; order is not meaningful.
    pub roster: Vec<RosterEntry>,
}

/// One rostered brick: who it is and the tier it was assigned at format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RosterEntry {
    pub brick_uuid: [u8; 16],
    pub tier: u8,
}

impl Anchor {
    /// Panics if the roster exceeds [`ROSTER_CAP`] — the cap is enforced
    /// with a named error where bricks are assigned, so an oversized roster
    /// here is a bug above, not an input.
    pub fn encode(&self) -> [u8; ANCHOR_LEN] {
        assert!(
            self.roster.len() <= ROSTER_CAP,
            "anchor roster exceeds ROSTER_CAP"
        );
        let mut buf = [0u8; ANCHOR_LEN];
        buf[0..8].copy_from_slice(ANCHOR_MAGIC);
        put_u64(&mut buf, 8, self.generation);
        put_u64(&mut buf, 16, self.wal_replay_offset);
        put_u64(&mut buf, 24, self.wal_replay_seq);
        put_u64(&mut buf, 32, self.wal_epoch);
        put_u64(&mut buf, 40, self.era);
        buf[48..80].copy_from_slice(&self.manifest_hash);
        buf[ANCHOR_ROSTER_AT] = self.roster.len() as u8;
        for (i, entry) in self.roster.iter().enumerate() {
            let at = ANCHOR_ROSTER_AT + 1 + i * ROSTER_ENTRY_LEN;
            buf[at..at + 16].copy_from_slice(&entry.brick_uuid);
            buf[at + 16] = entry.tier;
        }
        let check = full_check(&buf[0..ANCHOR_CHECK_AT]);
        buf[ANCHOR_CHECK_AT..ANCHOR_LEN].copy_from_slice(&check);
        buf
    }

    /// `None` for a slot that holds no valid anchor — a torn checkpoint
    /// write reads this way, and its twin carries the previous state.
    pub fn decode(buf: &[u8]) -> Option<Anchor> {
        if buf.len() < ANCHOR_LEN || &buf[0..8] != ANCHOR_MAGIC {
            return None;
        }
        if full_check(&buf[0..ANCHOR_CHECK_AT]) != buf[ANCHOR_CHECK_AT..ANCHOR_LEN] {
            return None;
        }
        let roster_len = buf[ANCHOR_ROSTER_AT] as usize;
        if roster_len > ROSTER_CAP {
            return None;
        }
        let mut roster = Vec::with_capacity(roster_len);
        for i in 0..roster_len {
            let at = ANCHOR_ROSTER_AT + 1 + i * ROSTER_ENTRY_LEN;
            let mut brick_uuid = [0u8; 16];
            brick_uuid.copy_from_slice(&buf[at..at + 16]);
            roster.push(RosterEntry {
                brick_uuid,
                tier: buf[at + 16],
            });
        }
        let mut manifest_hash = [0u8; 32];
        manifest_hash.copy_from_slice(&buf[48..80]);
        Some(Anchor {
            generation: get_u64(buf, 8),
            wal_replay_offset: get_u64(buf, 16),
            wal_replay_seq: get_u64(buf, 24),
            wal_epoch: get_u64(buf, 32),
            era: get_u64(buf, 40),
            manifest_hash,
            roster,
        })
    }

    /// The slot a generation lands in: alternating, so a torn write can
    /// only ever hurt the generation being written, never the survivor.
    pub fn slot_offset(generation: u64) -> u64 {
        ANCHOR_AREA_START + (generation % ANCHOR_SLOTS) * SECTOR_SIZE
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
            segment_area_start: WAL_AREA_START + (64 << 10),
            segment_count: 7,
            generation: 3,
            wal_start: WAL_AREA_START,
            wal_size: 64 << 10,
            pool_uuid: [1; 16],
            brick_uuid: [2; 16],
            tier: 0,
            wal_holder: true,
        }
    }

    #[test]
    fn a_superblock_round_trips_exactly() {
        let sb = sample_superblock();
        let decoded = Superblock::decode(&sb.encode()).unwrap().unwrap();
        assert_eq!(sb, decoded);
        let quiet = Superblock {
            tier: 3,
            wal_holder: false,
            ..sample_superblock()
        };
        assert_eq!(Superblock::decode(&quiet.encode()).unwrap(), Some(quiet));
    }

    #[test]
    fn a_single_flipped_superblock_bit_reads_as_no_superblock() {
        let mut buf = sample_superblock().encode();
        // Every field but the version: the magic, geometry, uuids, the new
        // tier and flags bytes, a reserved byte, and the checksum itself.
        for bit_of in [0usize, 13, 17, 50, 60, 90, 96, 97, 105, 120, 140] {
            buf[bit_of] ^= 0x01;
            assert_eq!(Superblock::decode(&buf).unwrap(), None, "byte {bit_of}");
            buf[bit_of] ^= 0x01;
        }
    }

    #[test]
    fn a_valid_superblock_from_the_future_is_an_error_not_a_blank() {
        let mut buf = sample_superblock().encode();
        put_u32(&mut buf, 8, FORMAT_VERSION + 1);
        let check = full_check(&buf[0..112]);
        buf[112..144].copy_from_slice(&check);
        assert_eq!(
            Superblock::decode(&buf).unwrap_err(),
            FsError::UnsupportedVersion(FORMAT_VERSION + 1)
        );
    }

    /// Phase 4's compatibility decision, pinned: version 1 has no upgrade
    /// path — a real v1 superblock (its checksum at its own offsets, valid
    /// under its own rules) is refused by name, never read as blank.
    #[test]
    fn a_version_one_superblock_is_refused_by_name() {
        // Hand-encode the v1 layout: same fields through byte 96, checksum
        // over 0..96 at 96..128, nothing after 128.
        let mut buf = [0u8; 144];
        buf[0..8].copy_from_slice(b"LUMENFS\0");
        put_u32(&mut buf, 8, 1);
        put_u32(&mut buf, 12, 16384);
        put_u64(&mut buf, 16, 1 << 20);
        put_u64(&mut buf, 24, WAL_AREA_START + (64 << 10));
        put_u64(&mut buf, 32, 7);
        put_u64(&mut buf, 40, 3);
        put_u64(&mut buf, 48, WAL_AREA_START);
        put_u64(&mut buf, 56, 64 << 10);
        buf[64..80].copy_from_slice(&[1; 16]);
        buf[80..96].copy_from_slice(&[2; 16]);
        let check = full_check(&buf[0..96]);
        buf[96..128].copy_from_slice(&check);
        assert_eq!(
            Superblock::decode(&buf).unwrap_err(),
            FsError::UnsupportedVersion(1)
        );
    }

    fn sample_anchor() -> Anchor {
        Anchor {
            generation: 12,
            wal_replay_offset: WAL_AREA_START + 4096,
            wal_replay_seq: 88,
            wal_epoch: 4,
            era: 3,
            manifest_hash: [9; 32],
            roster: vec![
                RosterEntry {
                    brick_uuid: [2; 16],
                    tier: 0,
                },
                RosterEntry {
                    brick_uuid: [5; 16],
                    tier: 1,
                },
            ],
        }
    }

    #[test]
    fn an_anchor_round_trips_and_a_flipped_bit_reads_as_no_anchor() {
        let anchor = sample_anchor();
        let mut buf = anchor.encode();
        assert_eq!(Anchor::decode(&buf), Some(anchor));
        // Fields old and new: header, roster length, a roster entry's uuid
        // and tier, a byte of the unused roster tail, and the checksum.
        for bit_of in [0usize, 10, 20, 36, 44, 60, 80, 85, 97, 200, 360] {
            buf[bit_of] ^= 0x01;
            assert_eq!(Anchor::decode(&buf), None, "byte {bit_of}");
            buf[bit_of] ^= 0x01;
        }
    }

    #[test]
    fn a_roster_carries_up_to_the_cap_and_refuses_beyond_it() {
        let full = Anchor {
            roster: (0..ROSTER_CAP)
                .map(|i| RosterEntry {
                    brick_uuid: [i as u8; 16],
                    tier: (i % 3) as u8,
                })
                .collect(),
            ..sample_anchor()
        };
        let buf = full.encode();
        assert_eq!(Anchor::decode(&buf), Some(full));
        // A length beyond the cap is not an anchor, even re-checksummed:
        // decode refuses before reading entries that have no reserved bytes.
        let mut over = buf;
        over[ANCHOR_ROSTER_AT] = ROSTER_CAP as u8 + 1;
        let check = full_check(&over[0..ANCHOR_CHECK_AT]);
        over[ANCHOR_CHECK_AT..ANCHOR_LEN].copy_from_slice(&check);
        assert_eq!(Anchor::decode(&over), None);
    }

    #[test]
    fn an_empty_roster_is_a_valid_anchor() {
        let bare = Anchor {
            roster: Vec::new(),
            ..sample_anchor()
        };
        assert_eq!(Anchor::decode(&bare.encode()), Some(bare));
    }

    #[test]
    fn anchor_generations_alternate_between_the_two_slots() {
        assert_eq!(Anchor::slot_offset(2), ANCHOR_AREA_START);
        assert_eq!(Anchor::slot_offset(3), ANCHOR_AREA_START + SECTOR_SIZE);
        assert_ne!(Anchor::slot_offset(7), Anchor::slot_offset(8));
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
