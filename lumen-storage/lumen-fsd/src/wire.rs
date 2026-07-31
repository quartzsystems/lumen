//! The peer wire format: [`PeerMessage`] as framed bytes.
//!
//! The engine deliberately owns no encoding — repl.rs hands the daemon
//! in-memory shapes and the daemon owns the socket, so the daemon owns the
//! bytes. Hand-rolled fixed little-endian, the same position the on-disk
//! formats take: small enough that a serialization dependency would cost
//! more than it saves, and every byte accounted for.
//!
//! A frame on the socket is a `u32` little-endian payload length followed
//! by the payload; the payload is one tag byte and the message's fields.
//! Decoding is strict — a short buffer, an unknown tag, or trailing bytes
//! are each an error, never a guess — and every count is checked against
//! the bytes actually present before anything is allocated, so a corrupt
//! or hostile length cannot become an allocation.
//!
//! The session opens with a fixed 25-byte handshake in each direction:
//! magic, pool uuid, node id. The uuid check is what keeps two bricks of
//! different pools from replicating garbage into each other; the node
//! check is what keeps a daemon from talking to itself.

use lumen_fs::{BlockHash, Lease, PeerMessage, ReplOp, SyncOffer};

/// Protocol magic + version. Bump the last byte to break compatibility
/// loudly instead of misparsing quietly — version 3 is phase 5's
/// placement: the map version in the hello, the map itself as a message,
/// and the fetch pair for non-home reads. A phase-4 daemon is refused at
/// the handshake, by name.
pub const HANDSHAKE_MAGIC: [u8; 8] = *b"LFSPEER\x03";

/// The largest frame either side will send or accept. Sync data batches
/// are 64 blocks of at most the pool block size; this is generous headroom
/// over anything the protocol produces, and a hard stop for anything else.
pub const MAX_FRAME: usize = 64 << 20;

/// A session's opening bytes: who this is and which pool it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handshake {
    pub pool_uuid: [u8; 16],
    pub node: u8,
}

pub const HANDSHAKE_LEN: usize = 25;

impl Handshake {
    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[0..8].copy_from_slice(&HANDSHAKE_MAGIC);
        out[8..24].copy_from_slice(&self.pool_uuid);
        out[24] = self.node;
        out
    }

    pub fn decode(buf: &[u8; HANDSHAKE_LEN]) -> Result<Handshake, WireError> {
        if buf[0..8] != HANDSHAKE_MAGIC {
            return Err(WireError::BadMagic);
        }
        let mut pool_uuid = [0u8; 16];
        pool_uuid.copy_from_slice(&buf[8..24]);
        Ok(Handshake {
            pool_uuid,
            node: buf[24],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The buffer ended before the message did.
    Truncated,
    /// The message ended before the buffer did.
    Trailing,
    /// A tag byte no message or op uses.
    BadTag(u8),
    /// A frame length beyond [`MAX_FRAME`].
    TooLarge(usize),
    /// The handshake's opening bytes are not this protocol.
    BadMagic,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated => write!(f, "the frame ended before the message did"),
            WireError::Trailing => write!(f, "the message ended before the frame did"),
            WireError::BadTag(tag) => write!(f, "unknown message tag {tag}"),
            WireError::TooLarge(len) => write!(f, "frame of {len} bytes exceeds the ceiling"),
            WireError::BadMagic => write!(f, "the handshake is not this protocol"),
        }
    }
}

impl std::error::Error for WireError {}

const NO_NODE: u8 = 0xFF;

// ---------------------------------------------------------------------------
// Encoding: infallible, appends to a Vec.

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, data: &[u8]) {
    put_u32(out, data.len() as u32);
    out.extend_from_slice(data);
}

fn put_root(out: &mut Vec<u8>, root: &Option<BlockHash>) {
    match root {
        Some(hash) => {
            out.push(1);
            out.extend_from_slice(hash.as_bytes());
        }
        None => {
            out.push(0);
            out.extend_from_slice(&[0u8; 32]);
        }
    }
}

fn put_lease(out: &mut Vec<u8>, vdisk: u64, lease: &Lease) {
    put_u64(out, vdisk);
    out.push(lease.holder);
    out.push(lease.handing_to.unwrap_or(NO_NODE));
    put_u64(out, lease.era);
}

fn put_op(out: &mut Vec<u8>, op: &ReplOp) {
    match op {
        ReplOp::CreateVdisk {
            id,
            size_bytes,
            tier,
        } => {
            out.push(1);
            put_u64(out, *id);
            put_u64(out, *size_bytes);
            out.push(*tier);
        }
        ReplOp::Write { vdisk, index, hash } => {
            out.push(2);
            put_u64(out, *vdisk);
            put_u64(out, *index);
            out.extend_from_slice(hash.as_bytes());
        }
        ReplOp::Trim { vdisk, index } => {
            out.push(3);
            put_u64(out, *vdisk);
            put_u64(out, *index);
        }
        ReplOp::DeleteVdisk { id } => {
            out.push(4);
            put_u64(out, *id);
        }
        ReplOp::Snapshot { vdisk, snapshot } => {
            out.push(5);
            put_u64(out, *vdisk);
            put_u64(out, *snapshot);
        }
        ReplOp::DeleteSnapshot { vdisk, snapshot } => {
            out.push(6);
            put_u64(out, *vdisk);
            put_u64(out, *snapshot);
        }
        ReplOp::Rollback { vdisk, snapshot } => {
            out.push(7);
            put_u64(out, *vdisk);
            put_u64(out, *snapshot);
        }
        ReplOp::Clone {
            new_id,
            vdisk,
            snapshot,
        } => {
            out.push(8);
            put_u64(out, *new_id);
            put_u64(out, *vdisk);
            put_u64(out, *snapshot);
        }
        ReplOp::SetLease { vdisk, lease } => {
            out.push(9);
            put_lease(out, *vdisk, lease);
        }
    }
}

/// One message as frame payload bytes — the length prefix is the socket
/// layer's business, not the codec's.
pub fn encode(message: &PeerMessage) -> Vec<u8> {
    let mut out = Vec::new();
    match message {
        PeerMessage::Hello {
            era,
            node,
            tiers,
            map_version,
        } => {
            out.push(1);
            put_u64(&mut out, *era);
            out.push(*node);
            put_u64(&mut out, *map_version);
            out.push(tiers.len() as u8);
            out.extend_from_slice(tiers);
        }
        PeerMessage::Payloads(payloads) => {
            out.push(2);
            put_u32(&mut out, payloads.len() as u32);
            for (tier, payload) in payloads {
                out.push(*tier);
                put_bytes(&mut out, payload);
            }
        }
        PeerMessage::Apply { first_rseq, ops } => {
            out.push(3);
            put_u64(&mut out, *first_rseq);
            put_u32(&mut out, ops.len() as u32);
            for op in ops {
                put_op(&mut out, op);
            }
        }
        PeerMessage::Flush { up_to } => {
            out.push(4);
            put_u64(&mut out, *up_to);
        }
        PeerMessage::Durable { up_to } => {
            out.push(5);
            put_u64(&mut out, *up_to);
        }
        PeerMessage::SyncStart => out.push(6),
        PeerMessage::SyncManifest(offer) => {
            out.push(7);
            put_u64(&mut out, offer.era);
            put_u32(&mut out, offer.vdisks.len() as u32);
            for (id, size_bytes, tier, root) in &offer.vdisks {
                put_u64(&mut out, *id);
                put_u64(&mut out, *size_bytes);
                out.push(*tier);
                put_root(&mut out, root);
            }
            put_u32(&mut out, offer.snapshots.len() as u32);
            for (vdisk, snapshot, size_bytes, root) in &offer.snapshots {
                put_u64(&mut out, *vdisk);
                put_u64(&mut out, *snapshot);
                put_u64(&mut out, *size_bytes);
                put_root(&mut out, root);
            }
            put_u32(&mut out, offer.leases.len() as u32);
            for (vdisk, lease) in &offer.leases {
                put_lease(&mut out, *vdisk, lease);
            }
        }
        PeerMessage::SyncNeed(hashes) => {
            out.push(8);
            put_u32(&mut out, hashes.len() as u32);
            for (tier, hash) in hashes {
                out.push(*tier);
                out.extend_from_slice(hash.as_bytes());
            }
        }
        PeerMessage::SyncData(payloads) => {
            out.push(9);
            put_u32(&mut out, payloads.len() as u32);
            for (tier, payload) in payloads {
                out.push(*tier);
                put_bytes(&mut out, payload);
            }
        }
        PeerMessage::SyncReady => out.push(10),
        PeerMessage::SyncAdopt { final_rseq } => {
            out.push(11);
            put_u64(&mut out, *final_rseq);
        }
        PeerMessage::SyncDone { era } => {
            out.push(12);
            put_u64(&mut out, *era);
        }
        // The fetch pair — phase 5's non-home reads. A two-member daemon
        // never emits them (everyone homes everything), so the handshake
        // magic waits for the mesh to bump it.
        PeerMessage::Read(hashes) => {
            out.push(13);
            put_u32(&mut out, hashes.len() as u32);
            for (tier, hash) in hashes {
                out.push(*tier);
                out.extend_from_slice(hash.as_bytes());
            }
        }
        PeerMessage::ReadData(payloads) => {
            out.push(14);
            put_u32(&mut out, payloads.len() as u32);
            for (tier, payload) in payloads {
                out.push(*tier);
                put_bytes(&mut out, payload);
            }
        }
        PeerMessage::MapIs { version, pairs } => {
            out.push(15);
            put_u64(&mut out, *version);
            put_u32(&mut out, pairs.len() as u32);
            for homes in pairs {
                out.push(homes[0]);
                out.push(homes[1]);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Decoding: strict, allocation-guarded.

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::Truncated);
        }
        let slice = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn hash(&mut self) -> Result<BlockHash, WireError> {
        Ok(BlockHash::from_bytes(self.take(32)?.try_into().unwrap()))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, WireError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn root(&mut self) -> Result<Option<BlockHash>, WireError> {
        let present = self.u8()?;
        let hash = self.hash()?;
        Ok((present != 0).then_some(hash))
    }

    fn lease(&mut self) -> Result<(u64, Lease), WireError> {
        let vdisk = self.u64()?;
        let holder = self.u8()?;
        let handing = self.u8()?;
        let era = self.u64()?;
        Ok((
            vdisk,
            Lease {
                holder,
                handing_to: (handing != NO_NODE).then_some(handing),
                era,
            },
        ))
    }

    /// A declared element count, refused unless the bytes for it could
    /// possibly be present — the guard that keeps a corrupt count from
    /// becoming an allocation.
    fn count(&mut self, min_element: usize) -> Result<usize, WireError> {
        let count = self.u32()? as usize;
        if count.saturating_mul(min_element) > self.remaining() {
            return Err(WireError::Truncated);
        }
        Ok(count)
    }

    fn op(&mut self) -> Result<ReplOp, WireError> {
        match self.u8()? {
            1 => Ok(ReplOp::CreateVdisk {
                id: self.u64()?,
                size_bytes: self.u64()?,
                tier: self.u8()?,
            }),
            2 => Ok(ReplOp::Write {
                vdisk: self.u64()?,
                index: self.u64()?,
                hash: self.hash()?,
            }),
            3 => Ok(ReplOp::Trim {
                vdisk: self.u64()?,
                index: self.u64()?,
            }),
            4 => Ok(ReplOp::DeleteVdisk { id: self.u64()? }),
            5 => Ok(ReplOp::Snapshot {
                vdisk: self.u64()?,
                snapshot: self.u64()?,
            }),
            6 => Ok(ReplOp::DeleteSnapshot {
                vdisk: self.u64()?,
                snapshot: self.u64()?,
            }),
            7 => Ok(ReplOp::Rollback {
                vdisk: self.u64()?,
                snapshot: self.u64()?,
            }),
            8 => Ok(ReplOp::Clone {
                new_id: self.u64()?,
                vdisk: self.u64()?,
                snapshot: self.u64()?,
            }),
            9 => {
                let (vdisk, lease) = self.lease()?;
                Ok(ReplOp::SetLease { vdisk, lease })
            }
            tag => Err(WireError::BadTag(tag)),
        }
    }
}

/// One frame payload back into a message. Strict: trailing bytes are an
/// error, because bytes nobody meant are bytes nobody checked.
pub fn decode(buf: &[u8]) -> Result<PeerMessage, WireError> {
    if buf.len() > MAX_FRAME {
        return Err(WireError::TooLarge(buf.len()));
    }
    let mut r = Reader::new(buf);
    let message = match r.u8()? {
        1 => {
            let era = r.u64()?;
            let node = r.u8()?;
            let map_version = r.u64()?;
            let tier_count = r.u8()? as usize;
            let tiers = r.take(tier_count)?.to_vec();
            PeerMessage::Hello {
                era,
                node,
                tiers,
                map_version,
            }
        }
        2 => {
            let count = r.count(5)?;
            let mut payloads = Vec::with_capacity(count);
            for _ in 0..count {
                let tier = r.u8()?;
                payloads.push((tier, r.bytes()?));
            }
            PeerMessage::Payloads(payloads)
        }
        3 => {
            let first_rseq = r.u64()?;
            let count = r.count(9)?;
            let mut ops = Vec::with_capacity(count);
            for _ in 0..count {
                ops.push(r.op()?);
            }
            PeerMessage::Apply { first_rseq, ops }
        }
        4 => PeerMessage::Flush { up_to: r.u64()? },
        5 => PeerMessage::Durable { up_to: r.u64()? },
        6 => PeerMessage::SyncStart,
        7 => {
            let era = r.u64()?;
            let vdisk_count = r.count(50)?;
            let mut vdisks = Vec::with_capacity(vdisk_count);
            for _ in 0..vdisk_count {
                vdisks.push((r.u64()?, r.u64()?, r.u8()?, r.root()?));
            }
            let snapshot_count = r.count(57)?;
            let mut snapshots = Vec::with_capacity(snapshot_count);
            for _ in 0..snapshot_count {
                snapshots.push((r.u64()?, r.u64()?, r.u64()?, r.root()?));
            }
            let lease_count = r.count(18)?;
            let mut leases = Vec::with_capacity(lease_count);
            for _ in 0..lease_count {
                leases.push(r.lease()?);
            }
            PeerMessage::SyncManifest(SyncOffer {
                era,
                vdisks,
                snapshots,
                leases,
            })
        }
        8 => {
            let count = r.count(33)?;
            let mut hashes = Vec::with_capacity(count);
            for _ in 0..count {
                let tier = r.u8()?;
                hashes.push((tier, r.hash()?));
            }
            PeerMessage::SyncNeed(hashes)
        }
        9 => {
            let count = r.count(5)?;
            let mut payloads = Vec::with_capacity(count);
            for _ in 0..count {
                let tier = r.u8()?;
                payloads.push((tier, r.bytes()?));
            }
            PeerMessage::SyncData(payloads)
        }
        10 => PeerMessage::SyncReady,
        11 => PeerMessage::SyncAdopt {
            final_rseq: r.u64()?,
        },
        12 => PeerMessage::SyncDone { era: r.u64()? },
        13 => {
            let count = r.count(33)?;
            let mut hashes = Vec::with_capacity(count);
            for _ in 0..count {
                let tier = r.u8()?;
                hashes.push((tier, r.hash()?));
            }
            PeerMessage::Read(hashes)
        }
        14 => {
            let count = r.count(5)?;
            let mut payloads = Vec::with_capacity(count);
            for _ in 0..count {
                let tier = r.u8()?;
                payloads.push((tier, r.bytes()?));
            }
            PeerMessage::ReadData(payloads)
        }
        15 => {
            let version = r.u64()?;
            let count = r.count(2)?;
            let mut pairs = Vec::with_capacity(count);
            for _ in 0..count {
                pairs.push([r.u8()?, r.u8()?]);
            }
            PeerMessage::MapIs { version, pairs }
        }
        tag => return Err(WireError::BadTag(tag)),
    };
    if r.remaining() != 0 {
        return Err(WireError::Trailing);
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> BlockHash {
        BlockHash::from_bytes([byte; 32])
    }

    fn round_trip(message: PeerMessage) {
        let bytes = encode(&message);
        assert_eq!(
            decode(&bytes).unwrap(),
            message,
            "round trip changed the message"
        );
    }

    #[test]
    fn every_message_round_trips_exactly() {
        round_trip(PeerMessage::Hello {
            era: u64::MAX,
            node: 1,
            tiers: vec![0, 1, 2],
            map_version: 9,
        });
        round_trip(PeerMessage::Hello {
            era: 1,
            node: 0,
            tiers: vec![0],
            map_version: 0,
        });
        round_trip(PeerMessage::Payloads(vec![]));
        round_trip(PeerMessage::Payloads(vec![
            (0, vec![]),
            (1, vec![0xAB; 16384]),
        ]));
        round_trip(PeerMessage::Apply {
            first_rseq: 1,
            ops: vec![
                ReplOp::CreateVdisk {
                    id: 1,
                    size_bytes: 1 << 40,
                    tier: 2,
                },
                ReplOp::Write {
                    vdisk: 1,
                    index: 7,
                    hash: hash(0x11),
                },
                ReplOp::Trim { vdisk: 1, index: 9 },
                ReplOp::DeleteVdisk { id: 3 },
                ReplOp::Snapshot {
                    vdisk: 1,
                    snapshot: 4,
                },
                ReplOp::DeleteSnapshot {
                    vdisk: 1,
                    snapshot: 4,
                },
                ReplOp::Rollback {
                    vdisk: 1,
                    snapshot: 4,
                },
                ReplOp::Clone {
                    new_id: 9,
                    vdisk: 1,
                    snapshot: 4,
                },
                ReplOp::SetLease {
                    vdisk: 1,
                    lease: Lease {
                        holder: 0,
                        handing_to: Some(1),
                        era: 3,
                    },
                },
                ReplOp::SetLease {
                    vdisk: 2,
                    lease: Lease {
                        holder: 1,
                        handing_to: None,
                        era: 9,
                    },
                },
            ],
        });
        round_trip(PeerMessage::Flush { up_to: 42 });
        round_trip(PeerMessage::Durable { up_to: 42 });
        round_trip(PeerMessage::SyncStart);
        round_trip(PeerMessage::SyncManifest(SyncOffer {
            era: 5,
            vdisks: vec![(1, 1 << 30, 0, Some(hash(0x22))), (2, 4096, 1, None)],
            snapshots: vec![(1, 1000, 1 << 30, Some(hash(0x33)))],
            leases: vec![(
                1,
                Lease {
                    holder: 1,
                    handing_to: None,
                    era: 5,
                },
            )],
        }));
        round_trip(PeerMessage::SyncManifest(SyncOffer {
            era: 1,
            vdisks: vec![],
            snapshots: vec![],
            leases: vec![],
        }));
        round_trip(PeerMessage::SyncNeed(vec![
            (0, hash(0x44)),
            (2, hash(0x55)),
        ]));
        round_trip(PeerMessage::SyncData(vec![(1, vec![1, 2, 3])]));
        round_trip(PeerMessage::SyncReady);
        round_trip(PeerMessage::SyncAdopt { final_rseq: 77 });
        round_trip(PeerMessage::SyncDone { era: 6 });
        round_trip(PeerMessage::Read(vec![(0, hash(0x77)), (1, hash(0x78))]));
        round_trip(PeerMessage::Read(vec![]));
        round_trip(PeerMessage::ReadData(vec![(0, vec![9, 9, 9]), (2, vec![])]));
        round_trip(PeerMessage::MapIs {
            version: 7,
            pairs: (0..=255u8).map(|s| [s % 3, (s % 3 + 1) % 3]).collect(),
        });
    }

    #[test]
    fn damage_is_an_error_never_a_guess() {
        let good = encode(&PeerMessage::SyncNeed(vec![(0, hash(0x66))]));
        // Truncated anywhere: refused.
        for cut in 0..good.len() {
            assert!(
                decode(&good[..cut]).is_err(),
                "a truncation at {cut} decoded anyway"
            );
        }
        // Trailing garbage: refused.
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode(&long).unwrap_err(), WireError::Trailing);
        // Unknown tags: refused.
        assert_eq!(decode(&[16]).unwrap_err(), WireError::BadTag(16));
        assert_eq!(decode(&[0]).unwrap_err(), WireError::BadTag(0));
    }

    #[test]
    fn a_hostile_count_cannot_become_an_allocation() {
        // A Payloads frame claiming four billion entries in ten bytes.
        let mut evil = vec![2u8];
        evil.extend_from_slice(&u32::MAX.to_le_bytes());
        evil.extend_from_slice(&[0u8; 10]);
        assert_eq!(decode(&evil).unwrap_err(), WireError::Truncated);
    }

    #[test]
    fn the_handshake_round_trips_and_rejects_imposters() {
        let shake = Handshake {
            pool_uuid: [0xCD; 16],
            node: 1,
        };
        let bytes = shake.encode();
        assert_eq!(Handshake::decode(&bytes).unwrap(), shake);
        let mut wrong = bytes;
        wrong[0] = b'X';
        assert_eq!(Handshake::decode(&wrong).unwrap_err(), WireError::BadMagic);
    }
}
