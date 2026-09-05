// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/src/pidecoder.c + lib/include/chiaki/pidecoder.h (chiaki-ng).
//
// WICHTIG: `pidecoder.c` ist der Raspberry-Pi-spezifische Videodecoder auf
// Basis von OpenMAX IL (bcm_host/ilclient). Im C-Build wird die Datei nur mit
// `CHIAKI_ENABLE_PI_DECODER` übersetzt (AUTO → nur auf dem Pi aktiv); auf
// Windows ist der Decoder immer AUS.
//
// Dieser Rust-Port ist Windows-only und enthält kein unsafe — die OMX/ilclient-
// API ist dort nicht verfügbar und wäre reines FFI. Das Modul bildet daher den
// "Decoder deaktiviert"-Zustand des C-Builds 1:1 ab: `PiDecoder::new()`
// schlägt mit `ChiakiError::Unknown` fehl (entspricht dem Fehlschlag von
// `ilclient_init()`/`OMX_Init()` in `chiaki_pi_decoder_init`), und
// `video_sample_cb` liefert `false` wie `push_buffer` bei fehlgeschlagenem
// Buffer-Push. Der Rest der C-Logik (Tunnel-Setup, Latency-Target, Rotation,
// Display-Region) wäre ohne OMX nicht sinnvoll abbildbar und entfällt.

use super::error::{ChiakiError, ChiakiResult};

/// Port von `ChiakiPiDecoder` (nur der Zustands-Teil, der ohne OMX existiert).
#[derive(Debug, Default)]
pub struct PiDecoder {
    /// C: `port_settings_changed`
    pub port_settings_changed: bool,
    /// C: `first_packet`
    pub first_packet: bool,
}

impl PiDecoder {
    /// Port von `chiaki_pi_decoder_init()`.
    ///
    /// Liefert — wie das C-Original, wenn `ilclient_init()`/`OMX_Init()`
    /// fehlschlagen — `ChiakiError::Unknown`. Auf Windows (ohne OMX) ist das
    /// immer der Fall; damit entspricht dieses Ergebnis dem deaktivierten
    /// C-Build (`CHIAKI_ENABLE_PI_DECODER=OFF`).
    pub fn new() -> ChiakiResult<PiDecoder> {
        tracing::error!("ilclient_init failed");
        Err(ChiakiError::Unknown)
    }

    /// Port von `chiaki_pi_decoder_set_params()` — ohne OMX-Render-Komponente
    /// eine No-op (im deaktivierten C-Build wird die Funktion nie verlinkt).
    pub fn set_params(&mut self, _x: i32, _y: i32, _w: i32, _h: i32, _visible: bool) {}

    /// Port von `chiaki_pi_decoder_video_sample_cb()` /
    /// `push_buffer()`: ohne initialisierten Decoder kann kein Buffer
    /// gepusht werden — immer `false`.
    pub fn video_sample_cb(&mut self, _buf: &[u8]) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_fails_like_disabled_c_build() {
        // PiDecoder ist nicht PartialEq; auf das Fehler-Muster prüfen.
        assert!(matches!(PiDecoder::new(), Err(ChiakiError::Unknown)));
    }

    #[test]
    fn video_sample_cb_returns_false() {
        let mut decoder = PiDecoder::default();
        assert!(!decoder.port_settings_changed);
        assert!(!decoder.first_packet);
        assert!(!decoder.video_sample_cb(&[1, 2, 3]));
    }

    #[test]
    fn set_params_is_noop() {
        let mut decoder = PiDecoder::default();
        decoder.set_params(0, 0, 1920, 1080, true);
        assert!(!decoder.port_settings_changed);
    }
}
