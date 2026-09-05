// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/remote/rudpsendbuffer.c + lib/include/chiaki/remote/rudpsendbuffer.h
// (chiaki-ng) — analog zu chiaki-core takionsendbuffer.
//
// Puffer der gesendeten RUDP-Ctrl-Pakete für Re-Transmits und Acks. Anders als
// der Takion-Pendant besitzt der C-RudpSendBuffer seinen eigenen Thread
// (rudp_send_buffer_thread_func), der alle RUDP_DATA_RESEND_TIMEOUT_MS
// abgelaufene Pakete erneut versendet und nach RUDP_DATA_RESEND_TRIES_MAX
// Versuchen aufgibt (inkl. aller älteren Pakete per Ack).
//
// Umsetzung:
// - Die Paketdaten (packets) und should_stop liegen hinter einem Mutex, der
//   Condvar gehört dazu — exakt wie im C (cond wartet auf denselben Mutex).
// - Der Thread wird von `attach()` gestartet und hält nur einen `Weak` auf den
//   RudpShared (kein Zirkel-Referenz-Leak). `fini()` stoppt und joint ihn.
// - C-Rufe `chiaki_rudp_send_buffer_ack(...)` geben die acked Seq-Nums in ein
//   Out-Array; hier liefert `ack()` sie als Vec.
// - Im C-Testmodus (CHIAKI_UNIT_TEST / rudp == NULL) tut der Thread nichts;
//   hier gibt es keinen Thread, solange `attach()` nicht gerufen wurde.

use std::sync::{Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use chiaki_core::error::{ChiakiError, ChiakiResult};
use chiaki_core::seqnum;
use chiaki_core::time::now_ms;

use crate::rudp::{RudpPacketType, RudpShared};

/// RUDP_DATA_RESEND_TIMEOUT_MS (rudpsendbuffer.c)
pub const RUDP_DATA_RESEND_TIMEOUT_MS: u64 = 400;
/// RUDP_DATA_RESEND_WAKEUP_TIMEOUT_MS
pub const RUDP_DATA_RESEND_WAKEUP_TIMEOUT_MS: u64 = RUDP_DATA_RESEND_TIMEOUT_MS / 2;
/// RUDP_DATA_RESEND_TRIES_MAX
pub const RUDP_DATA_RESEND_TRIES_MAX: u64 = 25;
/// RUDP_SEND_BUFFER_SIZE (rudpsendbuffer.c; muss mit der acked-Array-Größe in
/// rudp_handle_message_ack() konsistent sein — RUDP_SEND_BUFFER_SIZE in rudp.c)
pub const RUDP_SEND_BUFFER_SIZE: usize = 16;

/// Port von `struct chiaki_rudp_send_buffer_packet_t`.
#[derive(Clone)]
struct Packet {
    seq_num: u16,
    tries: u64,
    last_send_ms: u64, // chiaki_time_now_monotonic_ms()
    buf: Vec<u8>,
}

struct Inner {
    packets: Vec<Packet>,
    packets_size: usize, // allocated size
    should_stop: bool,
}

/// Port von `ChiakiRudpSendBuffer`.
pub struct RudpSendBuffer {
    inner: Mutex<Inner>,
    cond: Condvar,
    rudp: Mutex<Weak<RudpShared>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl RudpSendBuffer {
    /// Port von `chiaki_rudp_send_buffer_init()` (Datenanteil; ohne Thread).
    ///
    /// @param size Anzahl der Packet-Slots
    pub fn new(size: usize) -> Self {
        RudpSendBuffer {
            inner: Mutex::new(Inner {
                packets: Vec::with_capacity(size),
                packets_size: size,
                should_stop: false,
            }),
            cond: Condvar::new(),
            rudp: Mutex::new(Weak::new()),
            thread: Mutex::new(None),
        }
    }

    /// Startet den Re-Send-Thread (`chiaki_thread_create` im C-Init).
    /// Genau einmal aufrufen (macht `Rudp::new`).
    pub(crate) fn attach(self: &std::sync::Arc<Self>, rudp: Weak<RudpShared>) -> ChiakiResult<()> {
        *self
            .rudp
            .lock()
            .map_err(|_| ChiakiError::MutexLocked)? = rudp.clone();
        let sb = std::sync::Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("Chiaki Rudp Send Buffer".to_owned())
            .spawn(move || send_buffer_thread_func(sb))
            .map_err(|_| ChiakiError::Thread)?;
        *self
            .thread
            .lock()
            .map_err(|_| ChiakiError::MutexLocked)? = Some(handle);
        Ok(())
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        // Poisoned Mutex: Paketliste bleibt strukturell konsistent (keine
        // panics in den kritischen Abschnitten).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Port von `chiaki_rudp_send_buffer_push()`.
    ///
    /// Ownership von `buf` geht an den Puffer über (C: malloc'd buf wird
    /// übernommen bzw. im Fehlerfall sofort freigegeben).
    pub fn push(&self, seq_num: u16, buf: Vec<u8>) -> ChiakiResult<()> {
        let mut inner = self.lock_inner();

        if inner.packets.len() >= inner.packets_size {
            tracing::error!("Rudp Send Buffer overflow");
            return Err(ChiakiError::Overflow);
        }

        for packet in &inner.packets {
            if packet.seq_num == seq_num {
                tracing::error!("Tried to push duplicate seqnum into Rudp Send Buffer");
                return Err(ChiakiError::InvalidData);
            }
        }

        inner.packets.push(Packet {
            seq_num,
            tries: 0,
            last_send_ms: now_ms(),
            buf,
        });

        tracing::trace!("Pushed seq num {:#x} into Rudp Send Buffer", seq_num);

        if inner.packets.len() == 1 {
            // buffer was empty before, so it will sleep without timeout => WAKE UP!!
            self.cond.notify_all();
        }

        Ok(())
    }

    /// Port von `chiaki_rudp_send_buffer_ack()`: entfernt alle Pakete mit
    /// seq_num == seq_num oder seq_num lt seq_num (Serial-Arithmetik, 1:1 zur
    /// C-Lücken-Semantik; der In-place-Shift-Kompaktierer des C wird durch
    /// `Vec::retain` abgebildet, wie in chiaki-core takionsendbuffer) und
    /// liefert deren Seq-Nums in Entfernungsreihenfolge.
    pub fn ack(&self, seq_num: u16) -> ChiakiResult<Vec<u16>> {
        let mut inner = self.lock_inner();

        let mut acked_seq_nums = Vec::new();
        inner.packets.retain(|p| {
            let is_acked = p.seq_num == seq_num || seqnum::seq_num_16_lt(p.seq_num, seq_num);
            if is_acked {
                acked_seq_nums.push(p.seq_num);
            }
            !is_acked
        });

        tracing::trace!("Acked seq num {:#x} from Rudp Send Buffer", seq_num);

        Ok(acked_seq_nums)
    }

    /// Anzahl aktuell gepufferter Pakete.
    pub fn count(&self) -> usize {
        self.lock_inner().packets.len()
    }

    /// Port von `rudp_send_buffer_resend()` (sperrt selbst; der Give-up-Pfad
    /// entsperrt kurzfristig für das Ack, wie im C).
    fn resend(&self, now: u64) {
        let rudp = self
            .rudp
            .lock()
            .map_err(|_| ChiakiError::MutexLocked)
            .and_then(|w| w.upgrade().ok_or(ChiakiError::Disconnected));
        let Ok(shared) = rudp else {
            // C: if(!send_buffer->rudp) return;
            return;
        };

        let mut inner = self.lock_inner();
        let mut i = 0usize;
        loop {
            if i >= inner.packets.len() {
                break;
            }
            let (give_up, seq_num) = {
                let packet = &mut inner.packets[i];
                if now - packet.last_send_ms > RUDP_DATA_RESEND_TIMEOUT_MS {
                    if packet.tries >= RUDP_DATA_RESEND_TRIES_MAX {
                        tracing::info!(
                            "Hit max retries of {} tries giving up on packet with seqnum {:#x}",
                            RUDP_DATA_RESEND_TRIES_MAX,
                            packet.seq_num
                        );
                        (true, packet.seq_num)
                    } else {
                        let packet_type = packet
                            .buf
                            .get(6..8)
                            .map(|b| u16::from_be_bytes([b[0], b[1]]))
                            .and_then(RudpPacketType::from_u16)
                            .map_or_else(
                                || "Undefined packet type".to_owned(),
                                |t| t.name().to_owned(),
                            );
                        tracing::info!(
                            "rudp Send Buffer re-sending packet with seqnum {:#x} and type {}, tries: {}",
                            packet.seq_num,
                            packet_type,
                            packet.tries
                        );
                        packet.last_send_ms = now;
                        let _ = shared.send_raw(&packet.buf);
                        packet.tries += 1;
                        (false, 0)
                    }
                } else {
                    (false, 0)
                }
            };
            if give_up {
                // C: Mutex kurzfristig freigeben, ack() holt ihn sich selbst
                drop(inner);
                let _acked = self.ack(seq_num);
                inner = self.lock_inner();
                if i > 0 {
                    i -= 1;
                }
                continue; // dieselbe Position erneut prüfen (wie im C)
            }
            i += 1;
        }
    }

    /// Port von `chiaki_rudp_send_buffer_fini()`: stoppt und joint den Thread.
    /// Idempotent; wird von `Rudp::fini`/Drop gerufen.
    pub(crate) fn fini(&self) {
        {
            let mut inner = self.lock_inner();
            inner.should_stop = true;
        }
        self.cond.notify_all();
        let handle = self
            .thread
            .lock()
            .map_err(|_| ChiakiError::MutexLocked)
            .ok()
            .and_then(|mut h| h.take());
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// Port von `rudp_send_buffer_thread_func()`.
fn send_buffer_thread_func(sb: std::sync::Arc<RudpSendBuffer>) {
    loop {
        let inner = sb.lock_inner();

        let inner = if !inner.packets.is_empty() {
            // if there are packets, wait with timeout
            let (g, _) = sb
                .cond
                .wait_timeout_while(
                    inner,
                    Duration::from_millis(RUDP_DATA_RESEND_WAKEUP_TIMEOUT_MS),
                    |i| !i.should_stop,
                )
                .unwrap_or_else(|e| e.into_inner());
            g
        } else {
            // if not, wait without timeout, but also wakeup if packets become available
            sb.cond
                .wait_while(inner, |i| !i.should_stop && i.packets.is_empty())
                .unwrap_or_else(|e| e.into_inner())
        };

        if inner.should_stop {
            break;
        }

        drop(inner);
        sb.resend(now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C-Unit-Test-Modus (rudp == NULL): nur Datenlogik, kein Thread.
    #[test]
    fn push_duplicate_overflow() {
        let sb = RudpSendBuffer::new(4);
        assert_eq!(sb.push(1, vec![0xab; 8]), Ok(()));
        assert_eq!(sb.push(2, vec![0xab; 8]), Ok(()));
        assert_eq!(sb.push(3, vec![0xab; 8]), Ok(()));
        // Duplikat
        assert_eq!(sb.push(1, vec![0xef; 8]), Err(ChiakiError::InvalidData));
        assert_eq!(sb.push(4, vec![0xab; 8]), Ok(()));
        // Voll
        assert_eq!(sb.push(5, vec![0xcd; 8]), Err(ChiakiError::Overflow));
        assert_eq!(sb.count(), 4);
    }

    #[test]
    fn ack_gap_semantics_16bit() {
        let sb = RudpSendBuffer::new(16);
        // Absichtlich ungeordnete Push-Reihenfolge
        for n in [5u16, 2, 9, 7, 0xfff0, 3, 0x7fff] {
            sb.push(n, vec![n as u8]).unwrap();
        }
        assert_eq!(sb.count(), 7);

        // Serial-Number-16-Arithmetik: 0xfff0 liegt "19 vor" 3
        let mut acked = sb.ack(3).unwrap();
        acked.sort_unstable();
        assert_eq!(acked, vec![2, 3, 0xfff0]);
        assert_eq!(sb.count(), 4);

        // Ack 8 entfernt 5 und 7, aber nicht 9 / 0x7fff
        let mut acked = sb.ack(8).unwrap();
        acked.sort_unstable();
        assert_eq!(acked, vec![5, 7]);
        assert_eq!(sb.count(), 2);

        let acked = sb.ack(9).unwrap();
        assert_eq!(acked, vec![9]);
        let acked = sb.ack(0x7fff).unwrap();
        assert_eq!(acked, vec![0x7fff]);
        assert_eq!(sb.count(), 0);

        // Ack auf leeren Puffer / unbekannte Nummer: kein Fehler
        assert!(sb.ack(1234).unwrap().is_empty());
    }

    #[test]
    fn resend_delivers_and_gives_up() {
        // Loopback-Paar: der Puffer-Thread resendet an die Peer-Socket
        let a = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let b = chiaki_core::sock::create_udp_socket(
            "127.0.0.1:0".parse().unwrap(),
            &Default::default(),
        )
        .unwrap();
        let b_addr = b.local_addr().unwrap();
        a.connect(b_addr).unwrap();

        // now_ms()-Epoche kann bei 0 beginnen → abwarten, bis Differenzen
        // zum Timeout messbar sind
        while now_ms() <= RUDP_DATA_RESEND_TIMEOUT_MS + 10 {
            std::thread::sleep(Duration::from_millis(5));
        }

        let rudp = crate::rudp::Rudp::new(a).expect("rudp init");
        rudp.send_buffer_push(0x1234, vec![1, 2, 3]).unwrap();
        // nach > 1 Timeout-Periode mindestens 1 Resend empfangen
        std::thread::sleep(Duration::from_millis(
            RUDP_DATA_RESEND_TIMEOUT_MS + RUDP_DATA_RESEND_WAKEUP_TIMEOUT_MS + 150,
        ));
        b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = b.recv_from(&mut buf).expect("resend empfangen");
        assert_eq!(&buf[..n], &[1, 2, 3]);

        // Ack entfernt das Paket
        rudp.ack_packet(0x1234).unwrap();
        assert_eq!(rudp.shared.send_buffer.get().unwrap().count(), 0);

        rudp.fini();
    }

    #[test]
    fn constants_match_c() {
        assert_eq!(RUDP_DATA_RESEND_TIMEOUT_MS, 400);
        assert_eq!(RUDP_DATA_RESEND_WAKEUP_TIMEOUT_MS, 200);
        assert_eq!(RUDP_DATA_RESEND_TRIES_MAX, 25);
        assert_eq!(RUDP_SEND_BUFFER_SIZE, 16);
    }
}
