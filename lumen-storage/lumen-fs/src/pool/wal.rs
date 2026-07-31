//! The write-ahead ring: where a vdisk write becomes durable before any
//! tree knows about it.
//!
//! The ack path (docs/lumenfs.md) is: data block into the extent store, WAL
//! entry into this ring, one flush — acknowledged. The COW map trees are
//! folded forward later, by a checkpoint; between checkpoints the WAL *is*
//! the truth about recent writes, and recovery replays it from the position
//! the anchor names.
//!
//! Frames are packed back to back, each self-validating: a torn frame is
//! just bytes that fail their check, and — like the segment scan — replay
//! stops at the first frame that fails or whose seq breaks the run. Frames
//! never split across the ring's end; when one doesn't fit, the writer
//! jumps to the start, and the reader discovers the jump the same way it
//! discovers the tail: the bytes at the old position don't decode with the
//! expected seq, so it tries the start.
//!
//! Space before the anchor's replay position is dead and reusable; the ring
//! is full when an append would lap into live history, and `WalFull` is the
//! pool's cue to checkpoint. One debris case is accepted, mirroring the
//! segment story: a frame that was written, never flushed, and survived a
//! crash intact can be replayed — its entry was real, merely never
//! promised, and replay validates every reference before applying it.
//!
//! What makes that acceptance safe is the **recovery epoch**. Every
//! recovery starts writing under a higher epoch than anything it saw, every
//! frame carries its epoch, and replay accepts a run only while epochs
//! never decrease. Without it there is a real trap: a recovery adopts the
//! failed tail's seq at the failed tail's position, so a *later* crash
//! could chain out of this history's frames and into the previous
//! history's debris — same seqs, aligned positions — and replay stale
//! entries *after* acknowledged ones. The epoch fence makes that chain
//! impossible: once a frame of the current epoch is accepted, every older
//! frame is refused. Debris can therefore only ever resurrect *whole and
//! first*, when the history after it never got a single frame to disk —
//! and such a run is entirely unacknowledged by construction.

use crate::disk::Disk;
use crate::error::{FsError, Result};
use crate::hash::full_check;
use crate::store::brick::Brick;

const FRAME_MAGIC: &[u8; 8] = b"LFSWAL\0\0";

/// magic 8 + seq 8 + epoch 8 + length 4 + pad 4.
const FRAME_HEADER_LEN: usize = 32;
/// BLAKE3 over header + payload, appended after the payload.
const FRAME_CHECK_LEN: usize = 32;

/// The largest payload a frame may carry — generous for map entries, and a
/// hard bound so a corrupt length can never send the scanner on a tour.
pub const MAX_WAL_PAYLOAD: usize = 4096;

fn frame_len(payload_len: usize) -> u64 {
    (FRAME_HEADER_LEN + payload_len + FRAME_CHECK_LEN) as u64
}

fn encode_frame(seq: u64, epoch: u64, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; frame_len(payload.len()) as usize];
    buf[0..8].copy_from_slice(FRAME_MAGIC);
    buf[8..16].copy_from_slice(&seq.to_le_bytes());
    buf[16..24].copy_from_slice(&epoch.to_le_bytes());
    buf[24..28].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    buf[FRAME_HEADER_LEN..FRAME_HEADER_LEN + payload.len()].copy_from_slice(payload);
    let check = full_check(&buf[..FRAME_HEADER_LEN + payload.len()]);
    buf[FRAME_HEADER_LEN + payload.len()..].copy_from_slice(&check);
    buf
}

/// The ring's in-memory position. All offsets are absolute disk offsets
/// inside the WAL area.
#[derive(Debug)]
pub struct Wal {
    start: u64,
    size: u64,
    /// Where the next frame goes.
    cursor: u64,
    /// The next frame's seq.
    next_seq: u64,
    /// The epoch new frames are written under — strictly above everything
    /// any earlier history wrote, which is the whole fence.
    epoch: u64,
    /// Where live history begins — the anchor's replay position. Everything
    /// from here to the cursor must survive; everything else is dead.
    replay_start: u64,
}

/// One recovered frame, with the position the ring would stand at if replay
/// accepts it — the pool truncates the run wherever validation says stop,
/// and adopts that frame's positions.
#[derive(Debug, Clone)]
pub struct RecoveredFrame {
    pub payload: Vec<u8>,
    pub seq: u64,
    pub epoch: u64,
    pub cursor_after: u64,
}

impl Wal {
    /// Distance from `a` forward to `b` around the ring.
    fn distance(&self, a: u64, b: u64) -> u64 {
        let (a, b) = (a - self.start, b - self.start);
        (b + self.size - a) % self.size
    }

    /// A ring with no live history, positioned by the anchor at format time
    /// or after every checkpoint.
    pub fn empty(start: u64, size: u64, cursor: u64, next_seq: u64, epoch: u64) -> Wal {
        Wal {
            start,
            size,
            cursor,
            next_seq,
            epoch,
            replay_start: cursor,
        }
    }

    /// Scan live history from the anchor's position, returning every frame
    /// that decodes in sequence under a never-decreasing epoch at or above
    /// `floor_epoch`. The caller applies its own validation on top and
    /// truncates; [`Wal::adopt`] then fixes the ring's position.
    pub fn recover<D: Disk>(
        brick: &Brick<D>,
        replay_offset: u64,
        replay_seq: u64,
        floor_epoch: u64,
    ) -> Result<Vec<RecoveredFrame>> {
        let (start, size) = brick.wal_bounds();
        let end = start + size;
        let mut frames = Vec::new();
        let mut pos = replay_offset;
        let mut seq = replay_seq;
        let mut epoch_floor = floor_epoch;
        let mut travelled = 0u64;

        loop {
            match Self::decode_at(brick, pos, end, seq, epoch_floor)? {
                Some((payload, len, epoch)) => {
                    travelled += len;
                    if travelled >= size {
                        // A full lap can only mean the ring never had a
                        // boundary the scan recognized; stop rather than
                        // loop.
                        break;
                    }
                    pos += len;
                    frames.push(RecoveredFrame {
                        payload,
                        seq,
                        epoch,
                        cursor_after: pos,
                    });
                    seq += 1;
                    epoch_floor = epoch;
                }
                None => {
                    // Not a frame here. If we are not at the ring's start,
                    // this may be the writer's wrap point — the one other
                    // place the run can continue is the start.
                    if pos != start {
                        if let Some((payload, len, epoch)) =
                            Self::decode_at(brick, start, end, seq, epoch_floor)?
                        {
                            travelled += len + (end - pos);
                            if travelled >= size {
                                break;
                            }
                            pos = start + len;
                            frames.push(RecoveredFrame {
                                payload,
                                seq,
                                epoch,
                                cursor_after: pos,
                            });
                            seq += 1;
                            epoch_floor = epoch;
                            continue;
                        }
                    }
                    break;
                }
            }
        }
        Ok(frames)
    }

    /// Decode one frame at `pos` if a valid one with `seq` and an epoch at
    /// or above `min_epoch` lies there.
    fn decode_at<D: Disk>(
        brick: &Brick<D>,
        pos: u64,
        end: u64,
        seq: u64,
        min_epoch: u64,
    ) -> Result<Option<(Vec<u8>, u64, u64)>> {
        if pos + frame_len(0) > end {
            return Ok(None);
        }
        let mut header = [0u8; FRAME_HEADER_LEN];
        brick.aux_read(pos, &mut header)?;
        if &header[0..8] != FRAME_MAGIC {
            return Ok(None);
        }
        if u64::from_le_bytes(header[8..16].try_into().unwrap()) != seq {
            return Ok(None);
        }
        let epoch = u64::from_le_bytes(header[16..24].try_into().unwrap());
        if epoch < min_epoch {
            // An older history's debris. Refusing it here is what keeps a
            // stale entry from ever landing after a current one.
            return Ok(None);
        }
        let length = u32::from_le_bytes(header[24..28].try_into().unwrap()) as usize;
        if length > MAX_WAL_PAYLOAD || pos + frame_len(length) > end {
            return Ok(None);
        }
        let mut rest = vec![0u8; length + FRAME_CHECK_LEN];
        brick.aux_read(pos + FRAME_HEADER_LEN as u64, &mut rest)?;
        let mut covered = Vec::with_capacity(FRAME_HEADER_LEN + length);
        covered.extend_from_slice(&header);
        covered.extend_from_slice(&rest[..length]);
        if full_check(&covered) != rest[length..] {
            return Ok(None);
        }
        Ok(Some((rest[..length].to_vec(), frame_len(length), epoch)))
    }

    /// Adopt the position after a validated replay: the ring continues from
    /// exactly where the accepted run ended, and everything from the
    /// anchor's position to there is live.
    pub fn adopt(&mut self, cursor: u64, next_seq: u64, replay_start: u64) {
        self.cursor = cursor;
        self.next_seq = next_seq;
        self.replay_start = replay_start;
    }

    /// Append one entry. Not durable until the brick flushes. `WalFull`
    /// means live history would be lapped — the pool's cue to checkpoint.
    pub fn append<D: Disk>(&mut self, brick: &mut Brick<D>, payload: &[u8]) -> Result<u64> {
        assert!(payload.len() <= MAX_WAL_PAYLOAD, "wal payload over the cap");
        let end = self.start + self.size;
        let len = frame_len(payload.len());
        let mut at = self.cursor;
        let mut spent = self.distance(self.replay_start, self.cursor);
        if at + len > end {
            // Doesn't fit before the ring's end: jump to the start, the
            // wasted tail counting against live space like anything else.
            spent += end - at;
            at = self.start;
        }
        if spent + len >= self.size {
            return Err(FsError::WalFull);
        }
        brick.aux_write(at, &encode_frame(self.next_seq, self.epoch, payload))?;
        let seq = self.next_seq;
        self.cursor = at + len;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Set the epoch new frames are written under. Recovery calls this with
    /// one more than anything it accepted — the fence itself.
    pub fn set_epoch(&mut self, epoch: u64) {
        self.epoch = epoch;
    }

    /// The epoch new frames carry — what a checkpoint's anchor records as
    /// the floor for the next recovery.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// A checkpoint has made everything up to the cursor redundant: live
    /// history restarts here. The caller writes the anchor carrying these
    /// positions — this only moves the in-memory fence.
    pub fn retire_to_cursor(&mut self) {
        self.replay_start = self.cursor;
    }

    /// What the anchor should record as the replay position and seq.
    pub fn position(&self) -> (u64, u64) {
        (self.cursor, self.next_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::sim::SimDisk;
    use crate::store::brick::{Brick, BrickParams};

    const KIB: u64 = 1024;

    fn brick(seed: u64) -> Brick<SimDisk> {
        Brick::format(
            SimDisk::new(1024 * KIB, seed),
            BrickParams {
                pool_uuid: [1; 16],
                brick_uuid: [2; 16],
                block_size: 4 * KIB as u32,
                segment_size: 64 * KIB,
                wal_size: 16 * KIB,
                tier: 0,
                wal_holder: true,
            },
        )
        .unwrap()
    }

    fn fresh_wal(brick: &Brick<SimDisk>) -> Wal {
        let (start, size) = brick.wal_bounds();
        Wal::empty(start, size, start, 1, 1)
    }

    #[test]
    fn appended_frames_recover_in_order() {
        let mut brick = brick(1);
        let mut wal = fresh_wal(&brick);
        for i in 0..5u8 {
            wal.append(&mut brick, &[i; 10]).unwrap();
        }
        brick.flush().unwrap();
        let (start, _) = brick.wal_bounds();
        let frames = Wal::recover(&brick, start, 1, 1).unwrap();
        assert_eq!(frames.len(), 5);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(frame.payload, vec![i as u8; 10]);
            assert_eq!(frame.seq, 1 + i as u64);
            assert_eq!(frame.epoch, 1);
        }
    }

    #[test]
    fn a_frame_that_cannot_fit_wraps_and_recovery_follows_it() {
        let mut brick = brick(2);
        let mut wal = fresh_wal(&brick);
        // Fill most of the 16 KiB ring, retire, then append across the end.
        let big = vec![7u8; 3000];
        for _ in 0..4 {
            wal.append(&mut brick, &big).unwrap();
        }
        wal.retire_to_cursor();
        let (cursor, next_seq) = wal.position();
        wal.append(&mut brick, &big).unwrap(); // fits before the end
        wal.append(&mut brick, &big).unwrap(); // must wrap to the start
        brick.flush().unwrap();
        let frames = Wal::recover(&brick, cursor, next_seq, 1).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].payload, big);
    }

    #[test]
    fn an_append_that_would_lap_live_history_is_refused() {
        let mut brick = brick(3);
        let mut wal = fresh_wal(&brick);
        let chunk = vec![1u8; 1000];
        let mut appended = 0;
        loop {
            match wal.append(&mut brick, &chunk) {
                Ok(_) => appended += 1,
                Err(FsError::WalFull) => break,
                Err(other) => panic!("{other}"),
            }
        }
        // 16 KiB ring, 1056-byte frames: it must refuse before the ring
        // physically overflows, having accepted a healthy number.
        assert!((10..16).contains(&appended), "appended {appended}");
        // Retiring live history makes room again.
        wal.retire_to_cursor();
        wal.append(&mut brick, &chunk).unwrap();
    }

    #[test]
    fn recovery_stops_at_a_torn_frame_and_keeps_what_came_before() {
        let mut brick = brick(4);
        let mut wal = fresh_wal(&brick);
        wal.append(&mut brick, &[1; 100]).unwrap();
        wal.append(&mut brick, &[2; 100]).unwrap();
        brick.flush().unwrap();
        let (_, second_end) = {
            let (start, _) = brick.wal_bounds();
            (start, start + 2 * frame_len(100))
        };
        // Corrupt the second frame's check bytes.
        brick.aux_write(second_end - 4, &[0xFF; 4]).unwrap();
        brick.flush().unwrap();
        let (start, _) = brick.wal_bounds();
        let frames = Wal::recover(&brick, start, 1, 1).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].payload, vec![1; 100]);
    }

    #[test]
    fn an_older_epochs_debris_cannot_chain_after_a_newer_frame() {
        // The trap the epoch exists for: an earlier history's unacked tail
        // sits at exactly the positions and seqs a later history will use.
        // Write the "old" history at epoch 5, overwrite only its first
        // frame at epoch 7 — as if the later history wrote one frame and
        // crashed — and replay: the epoch-7 frame is accepted, and the
        // aligned epoch-5 frame behind it must be refused, not chained.
        {
            let mut brick = brick(5);
            let (start, size) = brick.wal_bounds();
            let mut old = Wal::empty(start, size, start, 1, 5);
            old.append(&mut brick, &[0xAA; 100]).unwrap();
            old.append(&mut brick, &[0xBB; 100]).unwrap();
            old.append(&mut brick, &[0xCC; 100]).unwrap();
            brick.flush().unwrap();
            let mut new = Wal::empty(start, size, start, 1, 7);
            new.append(&mut brick, &[0xDD; 100]).unwrap();
            brick.flush().unwrap();

            let frames = Wal::recover(&brick, start, 1, 5).unwrap();
            assert_eq!(frames.len(), 1, "the debris chained: {frames:?}");
            assert_eq!(frames[0].payload, vec![0xDD; 100]);
            assert_eq!(frames[0].epoch, 7);
        }

        // Whole-and-first is still allowed: with no newer frame in front,
        // the old run replays intact from its own start.
        {
            let mut brick = brick(6);
            let (start, size) = brick.wal_bounds();
            let mut old = Wal::empty(start, size, start, 1, 5);
            old.append(&mut brick, &[0xAA; 100]).unwrap();
            old.append(&mut brick, &[0xBB; 100]).unwrap();
            brick.flush().unwrap();
            let frames = Wal::recover(&brick, start, 1, 5).unwrap();
            assert_eq!(frames.len(), 2);
        }
    }
}
