// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/takionsendbuffer.c + lib/include/chiaki/takionsendbuffer.h
// (chiaki-ng).
//
// Puffer der gesendeten Data-Pakete für Re-Transmits und Acks.
//
// Unterschiede zum C (dokumentiert):
// - Mutex/Cond/Thread liegen NICHT im SendBuffer, sondern bei seinem Besitzer
//   (Takion) — der C-Thread (takion_send_buffer_thread_func) wird 1:1 von
//   takion.rs betrieben; hier liegt nur die Datenlogik (unit-testbar wie im
//   C-Test mit takion == NULL).
// - chiaki_takion_send_buffer_ack() füllt optional ein out-Array; hier liefert
//   ack_collect() die acked Seq-Nums als Vec, ack() ist die Contract-Form.
// - Der In-place-memmove-Kompaktierer des C wird durch Vec::retain abgebildet —
//   identische Semantik: Pakete mit seq_num == ack oder seq_num lt ack werden
//   entfernt, alle anderen behalten ihre Reihenfolge.

use crate::error::{ChiakiError, ChiakiResult};
use crate::seqnum;
use crate::time::now_ms;

/// TAKION_DATA_RESEND_TIMEOUT_MS (takionsendbuffer.c)
pub const TAKION_DATA_RESEND_TIMEOUT_MS: u64 = 200;
/// TAKION_DATA_RESEND_WAKEUP_TIMEOUT_MS
pub const TAKION_DATA_RESEND_WAKEUP_TIMEOUT_MS: u64 = TAKION_DATA_RESEND_TIMEOUT_MS / 2;
/// TAKION_DATA_RESEND_TRIES_MAX
pub const TAKION_DATA_RESEND_TRIES_MAX: u64 = 25;
/// TAKION_SEND_BUFFER_SIZE (Slots; wird direkt übergeben, kein Exponent)
pub const TAKION_SEND_BUFFER_SIZE: usize = 16;

/// Port von `ChiakiTakionSendBufferPacket`.
#[derive(Clone)]
struct Packet {
    seq_num: u32,
    tries: u64,
    last_send_ms: u64, // chiaki_time_now_monotonic_ms()
    buf: Vec<u8>,
}

/// Port von `ChiakiTakionSendBuffer` (ohne Thread — siehe Modulkommentar).
pub struct TakionSendBuffer {
    packets: Vec<Packet>,
    packets_size: usize,
}

impl TakionSendBuffer {
    /// Port von `chiaki_takion_send_buffer_init()` (Datenanteil).
    ///
    /// @param size Anzahl der Packet-Slots (C: direkt TAKION_SEND_BUFFER_SIZE)
    pub fn new(size: usize) -> ChiakiResult<Self> {
        Ok(TakionSendBuffer {
            packets: Vec::with_capacity(size),
            packets_size: size,
        })
    }

    /// Anzahl aktuell gepufferter Pakete.
    pub fn count(&self) -> usize {
        self.packets.len()
    }

    /// Port von `chiaki_takion_send_buffer_push()`.
    ///
    /// Ownership von `buf` geht an den Puffer über (C: malloc'd buf wird
    /// übernommen bzw. im Fehlerfall freigegeben).
    pub fn add(&mut self, seq_num: u32, buf: Vec<u8>) -> ChiakiResult<()> {
        if self.packets.len() >= self.packets_size {
            tracing::error!("Takion Send Buffer overflow");
            return Err(ChiakiError::Overflow);
        }

        if self.packets.iter().any(|p| p.seq_num == seq_num) {
            tracing::error!("Tried to push duplicate seqnum into Takion Send Buffer");
            return Err(ChiakiError::InvalidData);
        }

        tracing::trace!("Pushed seq num {:#x} into Takion Send Buffer", seq_num);
        self.packets.push(Packet {
            seq_num,
            tries: 0,
            last_send_ms: now_ms(),
            buf,
        });
        Ok(())
    }

    /// Port von `chiaki_takion_send_buffer_ack()`: entfernt alle Pakete mit
    /// seq_num == seq_num oder seq_num lt seq_num (Lücken-Semantik 1:1) und
    /// liefert deren Seq-Nums in Entfernungsreihenfolge.
    pub fn ack_collect(&mut self, seq_num: u32) -> Vec<u32> {
        let mut acked = Vec::new();
        self.packets.retain(|p| {
            let is_acked = p.seq_num == seq_num || seqnum::seq_num_32_lt(p.seq_num, seq_num);
            if is_acked {
                acked.push(p.seq_num);
            }
            !is_acked
        });
        tracing::trace!("Acked seq num {:#x} from Takion Send Buffer", seq_num);
        acked
    }

    /// Contract-Form von [`ack_collect`](Self::ack_collect).
    pub fn ack(&mut self, seq_num: u32) -> ChiakiResult<()> {
        self.ack_collect(seq_num);
        Ok(())
    }

    /// Port des resend-Scans aus `takion_send_buffer_resend()`:
    ///
    /// Prüft alle Pakete gegen `now_ms`; abgelaufene (`now - last_send >
    /// TAKION_DATA_RESEND_TIMEOUT_MS`) werden mit neuem last_send + tries++
    /// vermerkt und als Puffer zur erneuten Sendung zurückgegeben. Pakete, die
    /// TAKION_DATA_RESEND_TRIES_MAX erreichen, werden — wie im C — per ack()
    /// (inkl. aller älteren) entfernt und deren Seq-Nums zurückgegeben.
    /// Das eigentliche send_raw macht der Aufrufer außerhalb (C: ebenfalls
    /// außerhalb des Array-Scans).
    pub fn take_expired_resends(&mut self, now_ms: u64) -> (Vec<Vec<u8>>, Vec<u32>) {
        let mut to_resend = Vec::new();
        let mut given_up = Vec::new();
        let mut i = 0;
        while i < self.packets.len() {
            if now_ms - self.packets[i].last_send_ms > TAKION_DATA_RESEND_TIMEOUT_MS {
                if self.packets[i].tries >= TAKION_DATA_RESEND_TRIES_MAX {
                    tracing::info!(
                        "Hit max retries of {} tries... giving up on packet with seqnum {:#x}",
                        TAKION_DATA_RESEND_TRIES_MAX,
                        self.packets[i].seq_num
                    );
                    let seq_num = self.packets[i].seq_num;
                    // C: chiaki_takion_send_buffer_ack(send_buffer, packet->seq_num, ...)
                    let acked = self.ack_collect(seq_num);
                    given_up.extend(acked);
                    continue; // i bleibt: Nachrücker sind auf diese Position gerutscht
                }
                tracing::info!(
                    "Takion Send Buffer re-sending packet with seqnum {:#x}, tries: {}",
                    self.packets[i].seq_num,
                    self.packets[i].tries
                );
                self.packets[i].last_send_ms = now_ms;
                self.packets[i].tries += 1;
                to_resend.push(self.packets[i].buf.clone());
            }
            i += 1;
        }
        (to_resend, given_up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministischer xorshift an Stelle von munit_rand (Golden-Struktur
    // aus chiaki-ng test/takion.c, test_takion_send_buffer).
    struct Rng(u64);
    impl Rng {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x >> 16) as u32
        }
        fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next_u32() as i32 % (hi - lo + 1))
        }
    }

    fn random_seqnums(rng: &mut Rng, nums: &mut Vec<u32>, count: usize) {
        while nums.len() < count {
            let seqnum = rng.next_u32();
            if !nums.contains(&seqnum) {
                nums.push(seqnum);
            }
        }
    }

    // check_send_buffer_contents aus dem C-Test: count stimmt und jede
    // erwartete Nummer ist im Puffer (Tests-Modul sieht packets[] privat).
    fn check_send_buffer_contents(send_buffer: &TakionSendBuffer, nums_expected: &[u32]) -> bool {
        if send_buffer.packets.len() != nums_expected.len() {
            return false;
        }
        for expected in nums_expected {
            if !send_buffer.packets.iter().any(|p| p.seq_num == *expected) {
                return false;
            }
        }
        true
    }

    // seqnums_ack aus dem C-Test: Entfernen von nums[i] == ack || lt(ack).
    fn seqnums_ack(nums: &mut Vec<u32>, ack_num: u32) {
        nums.retain(|n| !(*n == ack_num || seqnum::seq_num_32_lt(*n, ack_num)));
    }

    #[test]
    fn test_takion_send_buffer() {
        let nums_count = 0x30usize;
        let mut send_buffer = TakionSendBuffer::new(nums_count).unwrap();

        let mut rng = Rng(0x12345678);
        let mut nums_expected: Vec<u32> = Vec::new();
        random_seqnums(&mut rng, &mut nums_expected, nums_count + 1);
        // C-Test: nums_expected[nums_count] wird nie gepusht (nur für den
        // Overflow-Test) und darf nicht Teil der Ack-Erwartung sein
        // (C: seqnums_ack arbeitet nur auf den ersten nums_count Einträgen).
        let overflow_num = nums_expected.pop().unwrap();

        // Puffer bis auf einen Slot füllen
        for n in &nums_expected[..nums_count - 1] {
            let err = send_buffer.add(*n, vec![0xab; 8]);
            assert_eq!(err, Ok(()), "push {n:#x}");
        }

        // Duplikat -> InvalidData (C: duplicate seqnum). C prüft Overflow
        // VOR dem Duplikat-Check, daher dafür den Puffer nicht voll machen.
        let err = send_buffer.add(nums_expected[0], vec![0xef; 8]);
        assert_eq!(err, Err(ChiakiError::InvalidData));

        // letzter freier Slot
        let err = send_buffer.add(nums_expected[nums_count - 1], vec![0xab; 8]);
        assert_eq!(err, Ok(()));

        // Ein Slot zu viel -> Overflow (C: CHIAKI_ERR_OVERFLOW)
        let err = send_buffer.add(overflow_num, vec![0xcd; 8]);
        assert_eq!(err, Err(ChiakiError::Overflow));

        let mut nums_count_cur = nums_count;
        while nums_count_cur > 0 {
            let offset = rng.range(-1, 1) * rng.range(1, 32);
            let ack_num = nums_expected[nums_count_cur - 1].wrapping_add(offset as u32);
            send_buffer.ack_collect(ack_num); // TODO im C: acked-seqnums-Params testen
            seqnums_ack(&mut nums_expected, ack_num);
            nums_count_cur = nums_expected.len();
            assert!(
                check_send_buffer_contents(&send_buffer, &nums_expected[..nums_count_cur]),
                "Pufferinhalt nach ack {ack_num:#x} falsch"
            );
        }

        assert_eq!(send_buffer.count(), 0);
    }

    #[test]
    fn ack_gap_semantics() {
        let mut sb = TakionSendBuffer::new(16).unwrap();
        // Absichtlich ungeordnete Push-Reihenfolge
        for n in [5u32, 2, 9, 7, 0xffff_fff0, 3, 0x7fffffff] {
            sb.add(n, vec![n as u8]).unwrap();
        }
        assert_eq!(sb.count(), 7);

        // Serial-Number-32-Arithmetik (wie C chiaki_seq_num_32_lt): 0xffff_fff0
        // liegt "19 vor" 3 und ist damit ebenfalls lt 3!
        let mut acked = sb.ack_collect(3);
        acked.sort_unstable();
        assert_eq!(acked, vec![2, 3, 0xffff_fff0]);
        assert_eq!(sb.count(), 4);

        // Ack 8 entfernt 5 und 7 (lt), aber nicht 9 / 0x7fffffff
        let mut acked = sb.ack_collect(8);
        acked.sort_unstable();
        assert_eq!(acked, vec![5, 7]);
        assert_eq!(sb.count(), 2);

        // Ack 0: 0xffff_fff0 ist bereits weg, Rest ist nicht lt 0
        let acked = sb.ack_collect(0);
        assert!(acked.is_empty());
        assert_eq!(sb.count(), 2);

        let acked = sb.ack_collect(9);
        assert_eq!(acked, vec![9]);
        let acked = sb.ack_collect(0x7fffffff);
        assert_eq!(acked, vec![0x7fffffff]);
        assert_eq!(sb.count(), 0);

        // Ack auf leeren Puffer / unbekannte Nummer: kein Fehler, keine Opfer
        assert!(sb.ack_collect(1234).is_empty());
    }

    #[test]
    fn resend_expiry_and_give_up() {
        // now_ms() hat eine willkürliche Epoche (erster Aufruf) und kann am
        // Testanfang 0 sein — dann kann saturating_sub keine "alten"
        // Zeitstempel erzeugen (C: monotonic_ms seit Boot, immer groß).
        while now_ms() <= TAKION_DATA_RESEND_TIMEOUT_MS + 2 {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut sb = TakionSendBuffer::new(4).unwrap();
        sb.add(1, vec![1]).unwrap();
        sb.add(2, vec![2]).unwrap();

        // frisch hinzugefügt: nichts fällig
        let (resend, given_up) = sb.take_expired_resends(now_ms());
        assert!(resend.is_empty() && given_up.is_empty());
        assert_eq!(sb.packets[0].tries, 0);

        // künstlich altern: tries unter Max -> resend, tries wird erhöht
        for p in sb.packets.iter_mut() {
            p.last_send_ms = now_ms().saturating_sub(TAKION_DATA_RESEND_TIMEOUT_MS + 1);
        }
        let (resend, given_up) = sb.take_expired_resends(now_ms());
        assert_eq!(resend.len(), 2);
        assert!(given_up.is_empty());
        assert_eq!(sb.packets[0].tries, 1);
        assert_eq!(sb.packets[1].tries, 1);
        // last_send wurde erneuert: direkt danach nichts fällig
        let (resend, _) = sb.take_expired_resends(now_ms());
        assert!(resend.is_empty());

        // tries auf Max -> nächster Ablauf gibt auf (ack; Reihenfolge 1, dann 2)
        for p in sb.packets.iter_mut() {
            p.tries = TAKION_DATA_RESEND_TRIES_MAX;
            p.last_send_ms = now_ms().saturating_sub(TAKION_DATA_RESEND_TIMEOUT_MS + 1);
        }
        let (resend, given_up) = sb.take_expired_resends(now_ms());
        assert!(resend.is_empty());
        assert_eq!(given_up, vec![1, 2]);
        assert_eq!(sb.count(), 0);
    }

    #[test]
    fn constants_match_c() {
        assert_eq!(TAKION_DATA_RESEND_TIMEOUT_MS, 200);
        assert_eq!(TAKION_DATA_RESEND_WAKEUP_TIMEOUT_MS, 100);
        assert_eq!(TAKION_DATA_RESEND_TRIES_MAX, 25);
        assert_eq!(TAKION_SEND_BUFFER_SIZE, 16);
    }
}
