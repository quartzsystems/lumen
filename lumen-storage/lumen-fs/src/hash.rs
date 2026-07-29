//! The content address: a BLAKE3-256 of the block's payload.
//!
//! One primitive carries three duties, by design (docs/lumenfs.md): the hash
//! is the block's *address* (where it lives is derived from it), its
//! *checksum* (every read verifies it), and the *dedupe key* (an existing
//! hash is a refcount, not a write). Compare-by-hash at 256 bits is the
//! industry-settled position; there is deliberately no equality fallback
//! that reads both payloads.

use std::fmt;

/// A block's content address. 32 bytes, `Copy`, ordered, hashable — made to
/// be a map key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub [u8; 32]);

impl BlockHash {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        BlockHash(bytes)
    }
}

/// Hash a payload into its content address.
pub fn hash_block(payload: &[u8]) -> BlockHash {
    BlockHash(*blake3::hash(payload).as_bytes())
}

/// The short integrity check used on fixed-size headers: the first 8 bytes
/// of the BLAKE3 of the covered range. Headers are not content-addressed —
/// this only has to catch a torn or stale write, and 64 bits does that.
pub fn header_check(covered: &[u8]) -> [u8; 8] {
    let full = blake3::hash(covered);
    let mut out = [0u8; 8];
    out.copy_from_slice(&full.as_bytes()[..8]);
    out
}

/// The full-width integrity check used on the superblock, where 32 spare
/// bytes cost nothing and the stakes are the whole brick.
pub fn full_check(covered: &[u8]) -> [u8; 32] {
    *blake3::hash(covered).as_bytes()
}

impl fmt::Debug for BlockHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Eight hex chars identify a block in a log line; the full address
        // is for machines.
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_payload_always_hashes_to_the_same_address() {
        let a = hash_block(b"the same bytes");
        let b = hash_block(b"the same bytes");
        assert_eq!(a, b);
    }

    #[test]
    fn a_single_flipped_bit_changes_the_address() {
        let a = hash_block(&[0b0000_0000]);
        let b = hash_block(&[0b0000_0001]);
        assert_ne!(a, b);
    }
}
