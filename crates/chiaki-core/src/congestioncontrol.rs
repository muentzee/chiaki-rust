// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/congestioncontrol.c + lib/include/chiaki/congestioncontrol.h (chiaki-ng).
//
// Thread, der alle CONGESTION_CONTROL_INTERVAL_MS die Packet-Statistik
// abholt (mit Reset) und daraus ein Congestion-Packet (received/lost) an die
// Konsole sendet. Gemessener Packet Loss über packet_loss_max wird geclampt.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use super::error::{ChiakiError, ChiakiResult};
use super::packetstats::PacketStats;
use super::stoppipe::StopPipe;
use super::takion::{CongestionPacket, Takion};

/// `CONGESTION_CONTROL_INTERVAL_MS` (congestioncontrol.c).
pub const CONGESTION_CONTROL_INTERVAL_MS: u64 = 200;

/// Port von `ChiakiCongestionControl`.
pub struct CongestionControl {
    #[allow(dead_code)] // C-Struktur-Parität; der Thread erhält Kopien im Ctx
    takion: Arc<Takion>,
    #[allow(dead_code)] // C-Struktur-Parität; der Thread erhält Kopien im Ctx
    stats: Arc<PacketStats>,
    thread: Option<JoinHandle<()>>,
    /// `ChiakiBoolPredCond stop_cond` — die StopPipe bietet dieselbe Semantik
    /// (timedwait -> Timeout / stop -> Canceled).
    stop_cond: Arc<StopPipe>,
    #[allow(dead_code)]
    packet_loss_max: f64,
    /// `packet_loss` als f64-Bits (atomar lesbar für den Getter).
    packet_loss: Arc<AtomicU64>,
}

/// Kontext für den Thread (C: Felder von `ChiakiCongestionControl`).
struct CongestionThreadCtx {
    takion: Arc<Takion>,
    stats: Arc<PacketStats>,
    stop_cond: Arc<StopPipe>,
    packet_loss_max: f64,
    packet_loss: Arc<AtomicU64>,
}

/// Berechnet das zu sendende Congestion-Packet inkl. Clamping — als reine
/// Funktion aus `congestion_control_thread_func()` extrahiert (1:1), damit
/// sie ohne Takion testbar ist. Liefert `(packet, gemessener packet_loss)`.
fn compute_congestion_packet(
    mut received: u64,
    mut lost: u64,
    packet_loss_max: f64,
) -> (CongestionPacket, f64) {
    let total = received + lost;
    let packet_loss = if total > 0 {
        lost as f64 / total as f64
    } else {
        0.0
    };
    if packet_loss > packet_loss_max {
        tracing::debug!(
            "Clamping reported packet loss: measured={:.1}% reported_max={:.1}%",
            packet_loss * 100.0,
            packet_loss_max * 100.0
        );
        lost = (total as f64 * packet_loss_max) as u64;
        received = total - lost;
    }
    let packet = CongestionPacket {
        word_0: 0,
        received: received as u16,
        lost: lost as u16,
    };
    tracing::trace!(
        "Sending Congestion Control Packet, received: {}, lost: {}",
        packet.received,
        packet.lost
    );
    (packet, packet_loss)
}

/// Port von `congestion_control_thread_func()`.
fn congestion_control_thread_func(ctx: &CongestionThreadCtx) {
    // chiaki_bool_pred_cond_timedwait: TIMEOUT = weiterticken, alles
    // andere (Signal/stop) = beenden.
    while let ChiakiError::Timeout = ctx
        .stop_cond
        .wait_timeout(Duration::from_millis(CONGESTION_CONTROL_INTERVAL_MS))
    {

        let (received, lost) = ctx.stats.get(true);
        let (packet, packet_loss) = compute_congestion_packet(received, lost, ctx.packet_loss_max);
        ctx.packet_loss.store(packet_loss.to_bits(), Ordering::Relaxed);
        // C ignoriert den Rückgabewert von chiaki_takion_send_congestion().
        let _ = ctx.takion.send_congestion(packet);
    }
}

impl CongestionControl {
    /// Port von `chiaki_congestion_control_start()`.
    pub fn start(
        takion: Arc<Takion>,
        stats: Arc<PacketStats>,
        packet_loss_max: f64,
    ) -> ChiakiResult<CongestionControl> {
        let stop_cond = Arc::new(StopPipe::new());
        let packet_loss = Arc::new(AtomicU64::new(0.0f64.to_bits()));

        let ctx = CongestionThreadCtx {
            takion: Arc::clone(&takion),
            stats: Arc::clone(&stats),
            stop_cond: Arc::clone(&stop_cond),
            packet_loss_max,
            packet_loss: Arc::clone(&packet_loss),
        };

        // Wie im C mit Thread-Namen "Chiaki Congestion Control"
        let thread = Builder::new()
            .name("Chiaki Congestion Control".to_string())
            .spawn(move || congestion_control_thread_func(&ctx))
            .map_err(|e| {
                tracing::error!("CongestionControl: thread create failed: {e}");
                ChiakiError::Thread
            })?;

        Ok(CongestionControl {
            takion,
            stats,
            thread: Some(thread),
            stop_cond,
            packet_loss_max,
            packet_loss,
        })
    }

    /// Port von `chiaki_congestion_control_stop()`: signalisieren und joinen.
    pub fn stop(&mut self) -> ChiakiResult<()> {
        self.stop_cond.stop();
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| {
                tracing::error!("CongestionControl: thread panicked");
                ChiakiError::Thread
            })?;
        }
        Ok(())
    }

    /// Letzter gemessener Packet Loss (0..1) — C-Feld `packet_loss`.
    pub fn packet_loss(&self) -> f64 {
        f64::from_bits(self.packet_loss.load(Ordering::Relaxed))
    }
}

impl Drop for CongestionControl {
    /// Wie im C üblich: stoppen, falls der Aufrufer `stop()` vergessen hat.
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(received: u64, lost: u64, max: f64) -> CongestionPacket {
        compute_congestion_packet(received, lost, max).0
    }

    #[test]
    fn no_loss_passes_through() {
        let p = packet(200, 0, 0.04);
        assert_eq!(p.word_0, 0);
        assert_eq!(p.received, 200);
        assert_eq!(p.lost, 0);
    }

    #[test]
    fn loss_within_limit_passes_through() {
        // 3/100 = 3% <= 4%
        let p = packet(97, 3, 0.04);
        assert_eq!(p.received, 97);
        assert_eq!(p.lost, 3);
        // genau am Limit: loss == max wird NICHT geclampt (C: `>`)
        let p = packet(96, 4, 0.04);
        assert_eq!(p.received, 96);
        assert_eq!(p.lost, 4);
    }

    #[test]
    fn loss_above_limit_is_clamped() {
        // 10/100 = 10% > 4% -> lost = 100*0.04 = 4, received = 96
        let (p, loss) = compute_congestion_packet(90, 10, 0.04);
        assert!((loss - 0.1).abs() < 1e-9);
        assert_eq!(p.received, 96);
        assert_eq!(p.lost, 4);
    }

    #[test]
    fn clamping_truncates_like_c() {
        // lost = (u64)(total * max) mit Bruchteil: total=7, max=0.5 -> 3.5 -> 3
        // 6/7 = 85.7% > 50% -> clamp
        let (p, _) = compute_congestion_packet(1, 6, 0.5);
        assert_eq!(p.lost, 3);
        assert_eq!(p.received, 4);
    }

    #[test]
    fn zero_total_gives_zero_loss() {
        let (p, loss) = compute_congestion_packet(0, 0, 0.04);
        assert_eq!(loss, 0.0);
        assert_eq!(p.received, 0);
        assert_eq!(p.lost, 0);
    }

    #[test]
    fn u16_truncation_like_c() {
        // (uint16_t) cast schneidet ab — große Werte wrappen wie im C
        let (p, _) = compute_congestion_packet(0x1_0001, 0, 0.0);
        assert_eq!(p.received, 1); // (uint16_t)0x10001 = 1
    }

    #[test]
    fn interval_constant_matches_c() {
        assert_eq!(CONGESTION_CONTROL_INTERVAL_MS, 200);
    }

    #[test]
    fn stats_feed_congestion_calculation() {
        // Zusammenspiel PacketStats -> compute (ohne Takion-Thread)
        let stats = PacketStats::new();
        stats.push_generation(970, 30);
        let (received, lost) = stats.get(true);
        assert_eq!((received, lost), (970, 30));
        // 30/1000 = 3% <= 4% -> unangetastet
        let p = packet(received, lost, 0.04);
        assert_eq!(p.received, 970);
        assert_eq!(p.lost, 30);

        stats.push_generation(700, 300);
        let (received, lost) = stats.get(true);
        // 300/1000 = 30% -> geclampt auf 4%
        let p = packet(received, lost, 0.04);
        assert_eq!(p.received, 960);
        assert_eq!(p.lost, 40);
    }
}
