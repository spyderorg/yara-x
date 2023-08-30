use std::cmp::Ordering;
use std::iter;

use bitvec::bitarr;
use regex_syntax::hir::literal::Seq;

/// Compute the quality of a masked atom.
///
/// Both iterators (`bytes` and `masks`) should have the same number of
/// elements, if not, the shortest one will determine the length of the atom.
///
/// Each byte in the atom contributes a certain amount of points to the   
/// quality. Bytes [a-zA-Z] contribute 18 points each, the extremely common
/// byte 0x00 contributes only 6 points, and other common bytes like 0x20
/// and 0xFF contribute 12 points. The rest of the bytes contribute 20 points
/// each. Masked bytes adds 2 points for each non-masked bit, and subtracts 1
/// point for each masked bit. So, the ?? mask subtracts 8 points, and masks X?
/// and ?X contributes 4 points.
///
/// An additional boost consisting in 2x the number of unique bytes in the atom
/// is added to the quality. This are some examples of the quality of atoms:
///
///   01 0? 03      quality = 20 +  4 + 20      + 4 = 48
///   01 02         quality = 20 + 20           + 4 = 44
///   01 ?? ?3 04   quality = 20 -  8 +  4 + 20 + 4 = 36
///   61 62         quality = 18 + 18           + 4 = 40
///   61 61         quality = 18 + 18           + 2 = 38
///   01 ?? 03      quality = 20 -  8 + 20      + 4 = 36
///   00 01         quality =  6 + 20           + 4 = 30
///   01            quality = 20                + 1 = 21
///
pub fn masked_atom_quality<'a, B, M>(bytes: B, masks: M) -> i32
where
    B: IntoIterator<Item = &'a u8>,
    M: IntoIterator<Item = &'a u8>,
{
    let mut q: i32 = 0;

    // Create a bit array with 256 bits, where all bits are initially 0.
    // Bit N is set to 1 (true) if the atom contains the non-masked byte N.
    let mut bytes_present = bitarr![0; 256];

    let bytes = bytes.into_iter();
    let mut atom_len = 0;

    for (byte, mask) in bytes.zip(masks) {
        // If there's any masked bit, the quality is incremented by
        // N * 2 - M, where N is the number of non-masked bits and M is
        // the number of masked bits. For ?? the increment is -8, while
        // ?X and X? results in a +4 increment.
        if mask.count_zeros() > 0 {
            q += 2 * mask.count_ones() as i32 - mask.count_zeros() as i32;
        }
        // For non-masked bytes the increment depends on the byte value.
        // Common values like 0x00, 0xff, 0xcc (opcode using of function
        // padding in PE files), 0x20 (whitespace) the increment is a bit
        // lower than for other bytes.
        else {
            bytes_present.set(*byte as usize, true);

            match *byte {
                // Common values contribute less to the quality than the
                // rest of values.
                0x20 | 0x90 | 0xcc | 0xff => {
                    q += 12;
                }
                // Zeroes are specially bad and contribute less.
                0x00 => {
                    q += 6;
                }
                // Bytes in the ASCII ranges a-z and A-Z have a slightly
                // lower quality than the rest. We want to favor atoms that
                // don't contain too many letters, as they generate less
                // additional atoms when the `nocase` modifier is used in
                // the pattern.
                b'a'..=b'z' | b'A'..=b'Z' => {
                    q += 18;
                }
                // General case.
                _ => {
                    q += 20;
                }
            }
        }

        atom_len += 1;
    }

    // The number of unique bytes is the number of ones in bytes_present.
    let unique_bytes = bytes_present.count_ones();

    // If all the bytes in the atom are equal and very common, let's
    // penalize it heavily.
    if unique_bytes == 1 {
        // As the number of unique bytes is 1, the first one in
        // bytes_present corresponds to that unique byte.
        match bytes_present.first_one().unwrap() {
            0x00 | 0x20 | 0x90 | 0xcc | 0xff => {
                q -= 10 * atom_len;
            }
            _ => {
                q += 2;
            }
        }
    }
    // In general, atoms with more unique bytes have better quality,
    // let's boost the quality proportionally to the number of unique bytes.
    else {
        q += 2 * unique_bytes as i32;
    }

    q
}

/// Compute the quality of an atom.
#[inline]
pub fn atom_quality<'a, B>(bytes: B) -> i32
where
    B: IntoIterator<Item = &'a u8>,
{
    masked_atom_quality(bytes, iter::repeat(&0xff))
}

#[derive(PartialEq)]
pub(crate) struct SeqQuality {
    seq_len: u32,
    min_atom_len: u32,
    min_atom_quality: i32,
}

impl SeqQuality {
    pub fn min() -> Self {
        Self { seq_len: u32::MAX, min_atom_len: 0, min_atom_quality: i32::MIN }
    }
}

impl PartialOrd for SeqQuality {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        // This sequence is better than the other if its worst atom is better
        // the other's worst atom.
        if self.min_atom_quality > other.min_atom_quality {
            return Some(Ordering::Greater);
        }
        // If the shortest atom in both sequences have the same length, the
        // best sequence is the one that has the higher min_atom_quality. If
        // both have the same min_atom_quality, then the shorter sequence is
        // the best.
        if self.min_atom_len == other.min_atom_len {
            return match (self.min_atom_quality, other.min_atom_quality) {
                (q1, q2) if q1 == q2 => {
                    if self.seq_len < other.seq_len {
                        Some(Ordering::Greater)
                    } else {
                        Some(Ordering::Less)
                    }
                }
                (q1, q2) if q1 > q2 => Some(Ordering::Greater),
                _ => Some(Ordering::Less),
            };
        }
        // If the minimum atom length for this sequence is exactly one byte
        // more than the other, this sequence still can be better than the
        // other if it has exactly 255 atoms less. This covers the case where a
        // single atom of length N is preferred over 256 atoms of length N+1.
        if self.min_atom_len + 1 == other.min_atom_len {
            return if (self.seq_len as usize * 256) <= (other.seq_len as usize)
            {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Less)
            };
        }

        if self.min_atom_len == other.min_atom_len + 1 {
            return if (self.seq_len as usize) < (other.seq_len as usize * 256)
            {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Less)
            };
        }

        // In general, this sequence is better than the other only if
        // its minimum atom length is greater.
        if self.min_atom_quality > other.min_atom_quality
            || self.min_atom_len > other.min_atom_len
        {
            Some(Ordering::Greater)
        } else {
            Some(Ordering::Less)
        }
    }
}

pub(crate) fn seq_quality(seq: &Seq) -> Option<SeqQuality> {
    seq.len().map(|len| SeqQuality {
        seq_len: len as u32,
        min_atom_len: seq.min_literal_len().unwrap_or(0) as u32,
        min_atom_quality: seq
            .literals()
            .unwrap_or(&[])
            .iter()
            .map(|l| atom_quality(l.as_bytes()))
            .min()
            .unwrap_or(i32::MIN),
    })
}

#[cfg(test)]
mod test {
    use super::atom_quality;
    use super::seq_quality;
    use crate::compiler::atoms::quality::masked_atom_quality;
    use regex_syntax::hir::literal::Literal;
    use regex_syntax::hir::literal::Seq;

    #[rustfmt::skip]
    #[allow(non_snake_case)]
    #[test]
    fn test_atom_quality() {
        let q_01 = atom_quality(&[0x01]);
        let q_0001 = atom_quality(&[0x00, 0x01]);
        let q_000001 = atom_quality(&[0x00, 0x00, 0x01]);
        let q_0102 = atom_quality(&[0x01, 0x02]);
        let q_000102 = atom_quality(&[0x00, 0x01, 0x02]);
        let q_010203 = atom_quality(&[0x01, 0x02, 0x03]);
        let q_00000000 = atom_quality(&[0x00, 0x00, 0x00, 0x00]);
        let q_00000001 = atom_quality(&[0x00, 0x00, 0x00, 0x01]);
        let q_00000102 = atom_quality(&[0x00, 0x00, 0x01, 0x02]);
        let q_00010203 = atom_quality(&[0x00, 0x01, 0x02, 0x03]);
        let q_01020304 = atom_quality(&[0x01, 0x02, 0x03, 0x04]);
        let q_01010101 = atom_quality(&[0x01, 0x01, 0x01, 0x01]);
        let q_01020102 = atom_quality(&[0x01, 0x02, 0x01, 0x02]);
        let q_01020000 = atom_quality(&[0x01, 0x02, 0x00, 0x00]);
        let q_ffffffff = atom_quality(&[0xff, 0xff, 0xff, 0xff]);
        let q_cccccccc = atom_quality(&[0xcc, 0xcc, 0xcc, 0xcc]);
        let q_90909090 = atom_quality(&[0x90, 0x90, 0x90, 0x90]);
        let q_20202020 = atom_quality(&[0x20, 0x20, 0x20, 0x20]);
        let q_aa = atom_quality(b"aa");
        let q_ab = atom_quality(b"ab");
        let q_abcd = atom_quality(b"abcd");
        let q_ABCD = atom_quality(b"ABCD");
        let q_abc_dot = atom_quality(b"abc.");

        let q_01x203 = masked_atom_quality(
            [0x01, 0x02, 0x03].iter(),
            [0xff, 0x0f, 0xff].iter()
        );

        let q_010x03 = masked_atom_quality(
            [0x01, 0x02, 0x03].iter(),
            [0xff, 0xf0, 0xff].iter()
        );

        let q_01xx03 = masked_atom_quality(
            [0x01, 0x02, 0x03].iter(),
            [0xff, 0x00, 0xff].iter()
        );

        let q_010x0x = masked_atom_quality(
            [0x01, 0x02, 0x03].iter(),
            [0xff, 0xf0, 0xf0].iter()
        );

        let q_0102xx04 = masked_atom_quality(
            [0x01, 0x02, 0x03, 0x04].iter(),
            [0xff, 0xff, 0x00, 0xff].iter(),
        );
        
        assert!(q_00000001 > q_00000000);
        assert!(q_00000001 > q_000001);
        assert!(q_000001 > q_0001);
        assert!(q_00000102 > q_00000001);
        assert!(q_00010203 > q_00000102);
        assert!(q_01020304 > q_00010203);
        assert!(q_000102 > q_000001);
        assert!(q_00010203 > q_010203);
        assert!(q_010203 > q_0102);
        assert!(q_010203 > q_00000000);
        assert!(q_0102 > q_01);
        assert!(q_01x203 > q_0102);
        assert!(q_01x203 > q_0001);
        assert!(q_01x203 < q_010203);
        assert_eq!(q_01x203, q_010x03);
        assert_eq!(q_cccccccc, q_ffffffff);
        assert_eq!(q_cccccccc, q_90909090);
        assert_eq!(q_cccccccc, q_20202020);
        assert!(q_01xx03 <= q_0102);
        assert!(q_01xx03 < q_010x03);
        assert!(q_01xx03 < q_010203);
        assert!(q_010x0x > q_01);
        assert!(q_010x0x < q_010203);
        assert_eq!(q_01020000, q_0102xx04);
        assert!(q_01020102 > q_01020000);
        assert!(q_01020102 > q_01010101);
        assert!(q_01020304 > q_01020102);
        assert!(q_01020102 > q_010203);
        assert!(q_01020304 > q_abcd);
        assert!(q_010203 < q_abcd);
        assert_eq!(q_abcd, q_ABCD);
        assert!(q_abc_dot > q_abcd);
        assert!(q_ab > q_01);
        assert!(q_aa > q_01);
        assert!(q_ab > q_aa);

        
        assert!(q_ab > q_000001);
    }

    #[test]
    fn test_seq_quality() {
        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("abcd")]))
                > seq_quality(&Seq::new(vec![Literal::inexact("abc")]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("abc"),]))
                > seq_quality(&Seq::new(vec![
                    Literal::inexact("abc"),
                    Literal::inexact("ab")
                ]))
        );

        assert!(
            seq_quality(&Seq::new(vec![
                Literal::inexact("ab"),
                Literal::inexact("cd")
            ])) > seq_quality(&Seq::new(vec![
                Literal::inexact("abc"),
                Literal::inexact("a")
            ]))
        );

        assert!(
            seq_quality(&Seq::new(vec![
                Literal::inexact("abc"),
                Literal::inexact("cde")
            ])) > seq_quality(&Seq::new(vec![
                Literal::inexact("abc"),
                Literal::inexact("cde"),
                Literal::inexact("fgh")
            ]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("abcd"),]))
                > seq_quality(&Seq::new(vec![Literal::inexact(
                    "\x00\x00\x00\x00"
                ),]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("abc"),]))
                > seq_quality(&Seq::new(vec![Literal::inexact(
                    "\x00\x00\x00\x00"
                ),]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("abc"),]))
                > seq_quality(&Seq::new(vec![Literal::inexact(
                    "\x00\x00\x00\x01"
                ),]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("\x01\0x02\0x03"),]))
                > seq_quality(&Seq::new(vec![Literal::inexact(
                    "\x00\x00\x00\x01"
                ),]))
        );

        assert!(
            seq_quality(&Seq::new(vec![Literal::inexact("ab"),]))
                > seq_quality(&Seq::new(vec![Literal::inexact(
                    "\x00\x00\x00\x00"
                ),]))
        );
    }
}
