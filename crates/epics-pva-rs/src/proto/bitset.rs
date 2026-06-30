//! BitSet for monitor-delta encoding.
//!
//! Implements the exact wire format used by pvxs `bitmask.cpp::to_wire/from_wire`,
//! which differs from a "fixed-size byte vector" in two important ways:
//!
//! 1. **LSB-first within each byte / word.** Bit `i` lives in byte `i/8` at
//!    position `i%8`. (pvxs internally uses `u64` words, but the wire format
//!    is just a sequence of bytes so we use `Vec<u8>` here.)
//! 2. **Trailing-zero trimming on encode.** Writes only enough bytes to cover
//!    the highest set bit. A BitSet with no bits set encodes as a single
//!    `Size{0}` byte (`0x00`).
//!
//! **Byte order.** pvxs `bitmask.cpp::to_wire` emits each full 64-bit word
//! via the generic `to_wire(Buffer&, uint64_t)` overload, which byte-swaps
//! the word when the connection byte order differs from the host's. The
//! trailing partial bytes (a final word that is not 8 bytes wide) are
//! instead emitted byte-by-byte with explicit `uint8_t(last >> (8*i))`
//! shifts, which are byte-order independent. So on a big-endian
//! connection the bytes within each *full* word are reversed, while any
//! trailing partial bytes pass through unreversed. This module reproduces
//! that layout exactly so the wire form is byte-identical to pvxs and
//! pvAccessJava (whose `BitSet.serialize` likewise writes full words via
//! `ByteBuffer.putLong`, honouring the buffer's byte order, and the last
//! partial word byte-by-byte LSB-first).
//!
//! Field-bit numbering on a `PvStructure` follows pvData spec §5.4: the
//! root structure occupies bit 0, then nested fields are numbered depth-first
//! in declaration order. This module is purely the bit container; the field-
//! numbering logic lives next to `pvdata::FieldDesc`.

use std::io::Cursor;

use super::buffer::{ByteOrder, DecodeError, ReadExt};
use super::size::{decode_size_nonnull, encode_size_into};

/// Compact bit container with LSB-first packing matching pvxs `bitmask.cpp`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitSet {
    bytes: Vec<u8>,
}

impl BitSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a BitSet large enough to hold `nbits` and pre-fill nothing.
    pub fn with_capacity(nbits: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(nbits.div_ceil(8)),
        }
    }

    /// All bits in `0..nbits` set. Used for "first monitor event" delta where
    /// the entire structure is new.
    pub fn all_set(nbits: usize) -> Self {
        let nbytes = nbits.div_ceil(8);
        let mut bytes = vec![0xFFu8; nbytes];
        // Mask the unused high bits in the final byte.
        let extra = nbytes * 8 - nbits;
        if extra > 0 {
            let last = bytes.len() - 1;
            bytes[last] &= 0xFFu8 >> extra;
        }
        Self { bytes }
    }

    /// Set bit `i`. Grows storage as needed.
    pub fn set(&mut self, i: usize) {
        let need = i / 8 + 1;
        if self.bytes.len() < need {
            self.bytes.resize(need, 0);
        }
        self.bytes[i / 8] |= 1 << (i % 8);
    }

    /// Clear bit `i`. Does NOT shrink storage; trailing zeros are trimmed
    /// only at encode time.
    pub fn clear(&mut self, i: usize) {
        let byte_idx = i / 8;
        if byte_idx < self.bytes.len() {
            self.bytes[byte_idx] &= !(1 << (i % 8));
        }
    }

    /// Test bit `i`.
    pub fn get(&self, i: usize) -> bool {
        let byte_idx = i / 8;
        if byte_idx >= self.bytes.len() {
            return false;
        }
        self.bytes[byte_idx] & (1 << (i % 8)) != 0
    }

    /// True iff no bits are set.
    pub fn is_empty(&self) -> bool {
        self.bytes.iter().all(|&b| b == 0)
    }

    /// Iterator over set bit positions in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.bytes.iter().enumerate().flat_map(|(byte_idx, &byte)| {
            (0..8).filter_map(move |bit_idx| {
                if byte & (1 << bit_idx) != 0 {
                    Some(byte_idx * 8 + bit_idx)
                } else {
                    None
                }
            })
        })
    }

    /// Find the next set bit at or after `start`. Returns `size()` (the
    /// total bit-storage size in bits) when no bit ≥ `start` is set.
    /// Mirrors pvxs `BitMask::findSet`.
    pub fn find_set(&self, start: usize) -> usize {
        let total_bits = self.size();
        let mut i = start;
        while i < total_bits {
            if self.get(i) {
                return i;
            }
            i += 1;
        }
        total_bits
    }

    /// Total bit-storage size, in bits — i.e. `bytes.len() * 8`. Note this
    /// is *not* the highest set bit; use `find_set(0)` for that.
    pub fn size(&self) -> usize {
        self.bytes.len() * 8
    }

    /// Backing-byte count (storage size / 8 in pvxs terms `wsize` / 8).
    pub fn byte_size(&self) -> usize {
        self.bytes.len()
    }

    /// Number of bits set.
    pub fn count(&self) -> usize {
        self.bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Encode in pvxs-compatible wire format.
    pub fn encode(&self, order: ByteOrder) -> Vec<u8> {
        let mut out = Vec::new();
        self.write_into(order, &mut out);
        out
    }

    /// Append the encoded form to `buf`. Matches pvxs `bitmask.cpp::to_wire`:
    /// emit u64 words plus any trailing partial bytes, with the *bit*
    /// numbering staying LSB-first (bit 0 = LSB of word 0). For little-
    /// endian wire byte order, this means byte 0 of the wire = LSB byte
    /// of word 0 = our `self.bytes[0]`. For big-endian, the bytes within
    /// each 8-byte word are reversed: wire byte 0 = MSB byte of word 0
    /// = our `self.bytes[7]`. Trailing partial bytes (a final word with
    /// fewer than 8 bytes) are emitted in storage order regardless of
    /// `order`, because pvxs writes them via byte-by-byte shifts rather
    /// than the byte-order-aware `to_wire(uint64_t)` overload.
    pub fn write_into(&self, order: ByteOrder, buf: &mut Vec<u8>) {
        // Trim trailing zero bytes (LSB-first numbering: trim from the
        // end of `self.bytes`, which is the high-order bytes).
        let mut nbytes = self.bytes.len();
        while nbytes > 0 && self.bytes[nbytes - 1] == 0 {
            nbytes -= 1;
        }
        encode_size_into(nbytes as u32, order, buf);

        match order {
            ByteOrder::Little => {
                // Wire bytes == storage bytes for LE.
                buf.extend_from_slice(&self.bytes[..nbytes]);
            }
            ByteOrder::Big => {
                // Emit u64 words byte-reversed; trailing partial bytes
                // (less than 8) are emitted in their original order
                // because they sit beyond the last full word.
                let nwords = nbytes / 8;
                let extra = nbytes % 8;
                for w in 0..nwords {
                    let start = w * 8;
                    let mut word = [0u8; 8];
                    word.copy_from_slice(&self.bytes[start..start + 8]);
                    word.reverse();
                    buf.extend_from_slice(&word);
                }
                if extra > 0 {
                    let start = nwords * 8;
                    buf.extend_from_slice(&self.bytes[start..start + extra]);
                }
            }
        }
    }

    /// Decode the next BitSet from `cur` (pvxs byte-count format).
    pub fn decode(cur: &mut Cursor<&[u8]>, order: ByteOrder) -> Result<Self, DecodeError> {
        let nbytes = decode_size_nonnull(cur, order, "bitset size")? as usize;
        let wire = cur.get_bytes(nbytes)?;
        let bytes = match order {
            ByteOrder::Little => wire,
            ByteOrder::Big => {
                // Reverse byte order within each 8-byte word; trailing
                // partial bytes pass through.
                let nwords = nbytes / 8;
                let extra = nbytes % 8;
                let mut out = Vec::with_capacity(nbytes);
                for w in 0..nwords {
                    let start = w * 8;
                    let mut word = [0u8; 8];
                    word.copy_from_slice(&wire[start..start + 8]);
                    word.reverse();
                    out.extend_from_slice(&word);
                }
                if extra > 0 {
                    let start = nwords * 8;
                    out.extend_from_slice(&wire[start..start + extra]);
                }
                out
            }
        };
        Ok(Self { bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bitset_encodes_as_zero_size() {
        let bs = BitSet::new();
        assert!(bs.is_empty());
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x00]);
    }

    #[test]
    fn single_bit_set() {
        let mut bs = BitSet::new();
        bs.set(0);
        assert!(bs.get(0));
        assert!(!bs.get(1));
        // size=1, byte=0x01
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x01, 0x01]);
    }

    #[test]
    fn high_bit_grows_storage() {
        let mut bs = BitSet::new();
        bs.set(20); // byte 2, bit 4 → bytes = [0x00, 0x00, 0x10]
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x03, 0x00, 0x00, 0x10]);
        assert!(bs.get(20));
        assert!(!bs.get(19));
        assert!(!bs.get(21));
    }

    #[test]
    fn clear_then_trim() {
        let mut bs = BitSet::new();
        bs.set(20);
        bs.set(0);
        bs.clear(20);
        // Trailing zeros must be trimmed even though storage still has 3 bytes.
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x01, 0x01]);
    }

    #[test]
    fn all_set_full_byte() {
        let bs = BitSet::all_set(8);
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x01, 0xFF]);
        for i in 0..8 {
            assert!(bs.get(i));
        }
        assert!(!bs.get(8));
    }

    #[test]
    fn all_set_partial_byte_masked() {
        let bs = BitSet::all_set(5);
        // 5 bits → only bottom 5 bits should be set
        assert_eq!(bs.encode(ByteOrder::Little), vec![0x01, 0b0001_1111]);
    }

    #[test]
    fn round_trip_random() {
        let mut bs = BitSet::new();
        for &i in &[0usize, 7, 8, 9, 63, 64, 100, 200] {
            bs.set(i);
        }
        let encoded = bs.encode(ByteOrder::Little);
        let mut cur = Cursor::new(encoded.as_slice());
        let decoded = BitSet::decode(&mut cur, ByteOrder::Little).unwrap();
        assert_eq!(bs, decoded);
        let set: Vec<usize> = decoded.iter().collect();
        assert_eq!(set, vec![0, 7, 8, 9, 63, 64, 100, 200]);
        assert_eq!(decoded.count(), 8);
    }

    #[test]
    fn be_and_le_identical_when_no_full_word() {
        // pvxs `bitmask.cpp::to_wire` byte-swaps only *full* 8-byte words.
        // When the trimmed bitset is shorter than one full word (highest
        // set bit < 64, so ≤ 7 bytes) there is no full word, only trailing
        // partial bytes — which pvxs writes byte-order independently. So
        // the BE and LE encodings must be byte-identical in that regime.
        for bits in [
            vec![],
            vec![0usize],
            vec![0usize, 7, 8],
            vec![1usize, 55],
            vec![0usize, 1, 2, 47, 48, 55],
        ] {
            let mut bs = BitSet::new();
            for &i in &bits {
                bs.set(i);
            }
            assert!(
                bs.byte_size() <= 7,
                "test fixture must stay below one full word for bits {bits:?}"
            );
            let le = bs.encode(ByteOrder::Little);
            let be = bs.encode(ByteOrder::Big);
            assert_eq!(le, be, "BE/LE encodings differ for sub-word bits {bits:?}");
        }
    }

    #[test]
    fn round_trip_big_endian() {
        // Round-trips that exercise full words plus a trailing partial
        // word — the BE word-swap path in both encode and decode.
        for bits in [
            vec![0usize, 7, 8, 9, 63, 64, 100, 200],
            vec![1usize, 63],
            vec![1usize, 63, 64, 126],
            vec![63usize, 64, 67],
        ] {
            let mut bs = BitSet::new();
            for &i in &bits {
                bs.set(i);
            }
            let encoded = bs.encode(ByteOrder::Big);
            let mut cur = Cursor::new(encoded.as_slice());
            let decoded = BitSet::decode(&mut cur, ByteOrder::Big).unwrap();
            assert_eq!(bs, decoded, "BE round-trip failed for bits {bits:?}");
            assert_eq!(cur.position() as usize, encoded.len());
            let set: Vec<usize> = decoded.iter().collect();
            assert_eq!(set, bits, "BE decoded set mismatch for {bits:?}");
        }
    }

    #[test]
    fn iter_sorted_ascending() {
        let mut bs = BitSet::new();
        for &i in &[100usize, 0, 50, 7, 8] {
            bs.set(i);
        }
        let collected: Vec<_> = bs.iter().collect();
        assert_eq!(collected, vec![0, 7, 8, 50, 100]);
    }
}
