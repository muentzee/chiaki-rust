// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/packetstats.c + lib/include/chiaki/packetstats.h (chiaki-ng).

use std::sync::Mutex;

use super::error::ChiakiResult;
use super::seqnum::seq_num_16_gt;

/// Port von `ChiakiPacketStats`.
///
/// Zwei Zählwege wie im C:
/// - "generations": bekannte Soll-Anzahl pro Generation (FEC-Units)
/// - "sequential": fortlaufende Sequenznummern (Fehlende = Differenz)
pub struct PacketStats {
    inner: Mutex<PacketStatsInner>,
}

struct PacketStatsInner {
    // For generations of packets, i.e. where we know the number of expected packets per generation
    gen_received: u64,
    gen_lost: u64,

    // For sequential packets, i.e. where packets are identified by a sequence number
    seq_min: u16, // sequence number that was max at the last reset
    seq_max: u16, // currently maximal sequence number
    seq_received: u64, // total received packets since the last reset
}

impl Default for PacketStats {
    fn default() -> Self {
        PacketStats::new()
    }
}

impl PacketStats {
    /// Port von `chiaki_packet_stats_init()`.
    ///
    /// Kann — anders als das C-Original (Mutex-Init kann fehlschlagen) — nicht
    /// fehlschlagen; deshalb kein `Result`.
    pub fn new() -> Self {
        PacketStats {
            inner: Mutex::new(PacketStatsInner {
                gen_received: 0,
                gen_lost: 0,
                seq_min: 0,
                seq_max: 0,
                seq_received: 0,
            }),
        }
    }

    /// Port von `chiaki_packet_stats_reset()`.
    pub fn reset(&self) {
        let mut stats = self.lock();
        reset_stats(&mut stats);
    }

    /// Port von `chiaki_packet_stats_push_generation()`.
    pub fn push_generation(&self, received: u64, lost: u64) {
        let mut stats = self.lock();
        stats.gen_received += received;
        stats.gen_lost += lost;
    }

    /// Port von `chiaki_packet_stats_push_seq()`.
    ///
    /// Abweichung zum C: das Original zählt hier ohne Mutex ( Datenrace,
    /// "funktioniert" nur, weil es aus einem einzigen Thread gerufen wird).
    /// In Rust greift der Zugriff über denselben Mutex — semantic identisch,
    /// aber threadsicher.
    pub fn push_seq(&self, seq_num: u16) {
        let mut stats = self.lock();
        stats.seq_received += 1;
        if seq_num_16_gt(seq_num, stats.seq_max) {
            stats.seq_max = seq_num;
        }
    }

    /// Port von `chiaki_packet_stats_get()`: summiert beide Zählwege und
    /// liefert `(received, lost)`; setzt bei `reset == true` zurück.
    pub fn get(&self, reset: bool) -> (u64, u64) {
        let mut stats = self.lock();

        // gen
        let mut received = stats.gen_received;
        let mut lost = stats.gen_lost;

        // seq — bewusster u16-Wraparound (C: "overflow on purpose if max < min")
        let seq_diff = (stats.seq_max as i32 - stats.seq_min as i32) as u64;
        let seq_lost = if stats.seq_received > seq_diff {
            seq_diff
        } else {
            seq_diff - stats.seq_received
        };
        received += stats.seq_received;
        lost += seq_lost;

        if reset {
            reset_stats(&mut stats);
        }
        (received, lost)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PacketStatsInner> {
        // Poisoned Mutex: Zähler bleiben konsistent, weiterzählen ist sicher.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn seq_max_for_test(&self) -> u16 {
        self.lock().seq_max
    }

    #[cfg(test)]
    fn seq_min_for_test(&self) -> u16 {
        self.lock().seq_min
    }
}

/// C: `reset_stats()` — `seq_min` friert das aktuelle `seq_max` ein.
fn reset_stats(stats: &mut PacketStatsInner) {
    stats.gen_received = 0;
    stats.gen_lost = 0;
    stats.seq_min = stats.seq_max;
    stats.seq_received = 0;
}

// ChiakiResult bleibt API-kompatibel erhalten, falls Aufrufer die C-Form
// spiegeln wollen; init kann wie oben beschrieben nicht mehr fehlschlagen.
/// Port von `chiaki_packet_stats_init()` in C-Signatur-Form.
pub fn packet_stats_init() -> ChiakiResult<PacketStats> {
    Ok(PacketStats::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let stats = PacketStats::new();
        assert_eq!(stats.get(false), (0, 0));
        assert_eq!(stats.get(true), (0, 0));
    }

    #[test]
    fn generation_counting() {
        let stats = PacketStats::new();
        stats.push_generation(10, 2);
        stats.push_generation(5, 1);
        assert_eq!(stats.get(false), (15, 3));
        // get(false) lässt Zähler stehen
        assert_eq!(stats.get(true), (15, 3));
        // get(true) hat zurückgesetzt
        assert_eq!(stats.get(false), (0, 0));
    }

    #[test]
    fn sequential_counting_no_loss() {
        let stats = PacketStats::new();
        // Erste Sequenznummer nur als Seed, dann reset (wie im echten Betrieb):
        stats.push_seq(100);
        stats.reset(); // seq_min = 100, seq_received = 0
        for i in 1..=10 {
            stats.push_seq(100 + i);
        }
        // seq_max = 110, diff = 10, received = 10 -> lost = 0
        let (received, lost) = stats.get(false);
        assert_eq!(received, 10);
        assert_eq!(lost, 0);
    }

    #[test]
    fn duplicates_hit_c_ternary_quirk() {
        let stats = PacketStats::new();
        stats.push_seq(100);
        stats.reset(); // seq_min = 100
        stats.push_seq(101);
        stats.push_seq(101); // Duplikat
        // C: seq_received(2) > seq_diff(1) -> seq_lost = seq_diff = 1
        assert_eq!(stats.get(false), (2, 1));
    }

    #[test]
    fn sequential_counting_with_loss() {
        let stats = PacketStats::new();
        stats.push_seq(1000);
        stats.reset(); // seq_min = 1000, seq_received = 0
        // Empfange 1002, 1003, 1005 (1001 und 1004 fehlen)
        stats.push_seq(1002);
        stats.push_seq(1003);
        stats.push_seq(1005);
        // seq_max = 1005, diff = 5; received = 3 -> lost = 2
        assert_eq!(stats.get(false), (3, 2));
    }

    #[test]
    fn sequential_wrap_produces_c_overflow_quirk() {
        // seq_max läuft über die u16-Grenze; C: "overflow on purpose if max < min".
        // diff = (u16)(0 - 0xfffd) in i32 -> negativ -> als u64 riesig.
        let stats = PacketStats::new();
        // Max über Serial-Arithmetic an 0xfffd heranführen:
        stats.push_seq(0x7fff); // gt(0x7fff, 0) -> max = 0x7fff
        stats.push_seq(0xfffd); // gt(0xfffd, 0x7fff) -> max = 0xfffd
        stats.reset(); // seq_min = 0xfffd
        stats.push_seq(0xfffe);
        stats.push_seq(0xffff);
        stats.push_seq(0x0000); // Wraparound; seq_num_16_gt erkennt ihn als Fortsetzung
        assert_eq!(stats.seq_max_for_test(), 0x0000);
        assert_eq!(stats.seq_min_for_test(), 0xfffd);
        // C-Quirk: seq_diff = (0 - 0xfffd) als i32 -> -65533 -> u64-Wrap
        let diff = (0x0000i32 - 0xfffdi32) as u64;
        assert_eq!(stats.get(false), (3, diff - 3));
    }

    #[test]
    fn combined_generation_and_seq() {
        let stats = PacketStats::new();
        stats.push_generation(100, 5);
        stats.push_seq(1);
        stats.push_seq(4); // 2 received, diff = 4 - 0 = 4 -> lost 2
        assert_eq!(stats.get(true), (102, 7));
        assert_eq!(stats.get(false), (0, 0));
    }

    #[test]
    fn reset_freezes_max_as_min() {
        let stats = PacketStats::new();
        stats.push_seq(500);
        stats.reset();
        // Nach reset: seq_min = seq_max = 500 -> diff 0, received 0
        assert_eq!(stats.get(false), (0, 0));
        stats.push_seq(503);
        // diff = 3, received 1 -> lost 2
        assert_eq!(stats.get(false), (1, 2));
    }

    #[test]
    fn shared_across_threads() {
        use std::sync::Arc;
        let stats = Arc::new(PacketStats::new());
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let stats = Arc::clone(&stats);
                std::thread::spawn(move || {
                    for i in 0..100u16 {
                        stats.push_seq(i);
                        stats.push_generation(1, 0);
                    }
                    let _ = t;
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let (received, lost) = stats.get(true);
        // get() summiert beide Zählwege (wie im C): 400 generations + 400 seq.
        // seq_lost: seq_max(99) - seq_min(0) = 99 < seq_received(400)
        // -> seq_lost = seq_diff = 99 (C-Ternary-Quirk).
        assert_eq!(received, 800);
        assert_eq!(lost, 99);
    }
}
