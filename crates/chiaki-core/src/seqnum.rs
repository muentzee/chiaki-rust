// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/include/chiaki/seqnum.h (chiaki-ng).
//
// RFC 1982 — Sequenznummern-Vergleiche mit Serial-Arithmetic-Wraparound.
//
// Das C-Makro CHIAKI_DEFINE_SEQNUM(bits, greater_sint) wird hier als Makro
// über Funktionsnamen portiert (exakt gleiche Semantik):
//
//   lt(a, b):  a == b → false; d = b - a (breiter, signed);
//              (a < b && d <  2^(bits-1)) || (a > b && -d > 2^(bits-1))
//   gt(a, b):  a == b → false;
//              (a < b && d >  2^(bits-1)) || (a > b && -d < 2^(bits-1))
//
// add/sub sind Erweiterungen (im C-Header nicht vorhanden): wrapping-Arithmetik
// auf der Sequenznummer, wie sie für reorder-queues gebraucht wird.

pub type SeqNum16 = u16;
pub type SeqNum32 = u32;

macro_rules! define_seqnum {
    ($lt:ident, $gt:ident, $add:ident, $sub:ident, $t:ty, $greater:ty, $half:expr) => {
        /// Port von `chiaki_seq_num_N_lt`.
        pub fn $lt(a: $t, b: $t) -> bool {
            if a == b {
                return false;
            }
            // |d| <= u32::MAX passt in den breiteren signed Typ — kein Overflow.
            let d = (b as $greater).wrapping_sub(a as $greater);
            ((a < b) && (d < $half)) || ((a > b) && (-d > $half))
        }

        /// Port von `chiaki_seq_num_N_gt`.
        pub fn $gt(a: $t, b: $t) -> bool {
            if a == b {
                return false;
            }
            let d = (b as $greater).wrapping_sub(a as $greater);
            ((a < b) && (d > $half)) || ((a > b) && (-d < $half))
        }

        /// Sequenznummer um `n` vorwärts (wrapping). Erweiterung gegenüber C.
        pub fn $add(a: $t, n: $t) -> $t {
            a.wrapping_add(n)
        }

        /// Rückgabe der Differenz `a - n` (wrapping). Erweiterung gegenüber C.
        pub fn $sub(a: $t, n: $t) -> $t {
            a.wrapping_sub(n)
        }
    };
}

define_seqnum!(
    seq_num_16_lt,
    seq_num_16_gt,
    seq_num_16_add,
    seq_num_16_sub,
    u16,
    i32,
    (1i32) << 15
);
define_seqnum!(
    seq_num_32_lt,
    seq_num_32_gt,
    seq_num_32_add,
    seq_num_32_sub,
    u32,
    i64,
    (1i64) << 31
);

#[cfg(test)]
mod tests {
    use super::*;

    // Golden-Tests aus chiaki-ng test/seqnum.c (1:1 übernommen).

    #[test]
    fn test_seq_num_16() {
        let mut a: u16 = 0;
        loop {
            let b = a.wrapping_add(1);
            assert!(seq_num_16_gt(b, a));
            assert!(!seq_num_16_gt(a, b));
            assert!(seq_num_16_lt(a, b));
            assert!(!seq_num_16_lt(b, a));
            a = b;
            if a == 0 {
                break;
            }
        }

        a = 0;
        loop {
            let b = a.wrapping_add(0xfff);
            assert!(seq_num_16_gt(b, a));
            assert!(!seq_num_16_gt(a, b));
            assert!(seq_num_16_lt(a, b));
            assert!(!seq_num_16_lt(b, a));
            a = a.wrapping_add(1);
            if a == 0 {
                break;
            }
        }

        assert!(seq_num_16_gt(1, 0xfff5));
        assert!(!seq_num_16_gt(0xfff5, 1));
    }

    #[test]
    fn test_seq_num_32() {
        assert!(seq_num_32_gt(1, 0));
        assert!(!seq_num_32_gt(0, 1));
        assert!(!seq_num_32_lt(1, 0));
        assert!(seq_num_32_lt(0, 1));
        assert!(seq_num_32_gt(1, 0xfffffff5));
        assert!(!seq_num_32_gt(0xfffffff5, 1));
    }

    // Ergänzende Tests für die add/sub-Erweiterung.

    #[test]
    fn test_seq_num_add_sub_wrap() {
        assert_eq!(seq_num_16_add(0xfffe, 3), 1);
        assert_eq!(seq_num_16_sub(1, 3), 0xfffe);
        assert_eq!(seq_num_32_add(0xffffffff, 2), 1);
        assert_eq!(seq_num_32_sub(1, 2), 0xffffffff);
        // add/sub sind zum Vergleichen konsistent: a + n > a für n < 2^(bits-1)
        let a: u32 = 0x7fffffff;
        let b = seq_num_32_add(a, 5);
        assert!(seq_num_32_gt(b, a));
        assert!(seq_num_32_lt(a, b));
    }

    #[test]
    fn test_equal_seq_nums_compare_false() {
        assert!(!seq_num_16_lt(1234, 1234));
        assert!(!seq_num_16_gt(1234, 1234));
        assert!(!seq_num_32_lt(0xdeadbeef, 0xdeadbeef));
        assert!(!seq_num_32_gt(0xdeadbeef, 0xdeadbeef));
    }
}
