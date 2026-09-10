// SPDX-License-Identifier: AGPL-3.0-only
//! CamFeed: Besitzer der virtuellen Kamera (OBS-Backend) und NV12-Writer.
//!
//! Ein [`CamFeed`] wird pro Session geöffnet — bei aktivem VSR im
//! VSR-Output-Format (Upscale-Schärfe für die Viewer, User-Vorgabe),
//! sonst in der Stream-Auflösung bzw. dem [`CamResolution`]-Preset — und
//! lebt im Media-Thread: `push_nv12_*` entstrippt die Quell-Strides,
//! skaliert bei Bedarf auf die Zielauflösung und publiziert den Frame in
//! die OBS-Shmem-Queue (Triple-Buffering — die Queue kollabiert keine
//! Frames, ein langsamer Konsument verliert einfach alte Slots). Beim Drop
//! schließt die Kamera (Queue-State STOPPING + Mapping freigeben) —
//! Konsumenten zeigen danach kein Bild.

use crate::scaler;
use virtualcam::{BackendKind, Camera, PixelFormat};

/// Zielauflösung der Kamera (settings/virtualcam_resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CamResolution {
    /// Stream-Auflösung VOR VSR (Default — Kamera sieht das native Video).
    Stream,
    /// 1280x720, nur wenn der Stream größer ist (kein Upscale).
    P720,
    /// 1920x1080, nur wenn der Stream größer ist (kein Upscale).
    P1080,
}

impl CamResolution {
    /// INI-String → Auflösung (unbekannt = Stream, wie `video_output`).
    pub fn from_ini_value(value: &str) -> Self {
        match value {
            "720p" => CamResolution::P720,
            "1080p" => CamResolution::P1080,
            _ => CamResolution::Stream,
        }
    }

    pub fn ini_value(self) -> &'static str {
        match self {
            CamResolution::Stream => "stream",
            CamResolution::P720 => "720p",
            CamResolution::P1080 => "1080p",
        }
    }
}

/// Konfiguration des Kamera-Feeds (Session-Start eingefroren).
#[derive(Debug, Clone)]
pub struct CamFeedConfig {
    /// Stream-Breite/-Höhe **vor** VSR (die Dekoder-Output-Auflösung).
    pub width: u32,
    pub height: u32,
    /// Stream-FPS (Kamera-Intervall in der Queue — Konsumenten takteten
    /// daran, die Kamera liefert mit der Stream-Rate).
    pub fps: u32,
    /// Gewünschte Kamera-Auflösung.
    pub resolution: CamResolution,
}

/// Offene virtuelle Kamera + Schreibpuffer. `Send` (Kamera-Handle ist es),
/// aber nicht `Sync` — gehört genau einem Thread (Media-Thread).
pub struct CamFeed {
    camera: Camera,
    /// Zielauflösung (Kamera-Format).
    width: u32,
    height: u32,
    /// Gepackter Stream-Auflösungs-Buffer (wiederverwendet).
    packed: Vec<u8>,
    /// Downscale-Ziel (nur bei Auflösung < Stream).
    scaled: Vec<u8>,
    frames_sent: u64,
    errors: u64,
}

impl CamFeed {
    /// Öffnet die virtuelle Kamera (OBS-Backend). Fehler, wenn OBS Virtual
    /// Camera nicht installiert/registriert ist oder die Queue bereits von
    /// einem anderen Writer belegt ist (z. B. OBS startet seine eigene VC).
    pub fn open(config: CamFeedConfig) -> Result<Self, String> {
        let (w, h) = Self::target_dims(&config);
        if w % 2 != 0 || h % 2 != 0 || w == 0 || h == 0 {
            return Err(format!("Invalid camera dimensions {w}x{h}"));
        }
        if config.fps == 0 {
            return Err("Camera FPS is 0".into());
        }
        if !crate::registry::obs_virtualcam_available() {
            return Err(
                "OBS Virtual Camera is not installed — install OBS Studio (the Virtual \
                 Camera is registered with OBS) or let OBS start its Virtual Camera once"
                    .into(),
            );
        }
        let camera = Camera::builder(w, h, f64::from(config.fps))
            .format(PixelFormat::NV12)
            .backend(BackendKind::Obs)
            .build()
            .map_err(|err| format!("Could not open virtual camera: {err}"))?;
        tracing::info!(
            "Virtuelle Kamera aktiv: „{}“ ({}x{} @ {}, Quelle {}x{})",
            camera.device(),
            w,
            h,
            config.fps,
            config.width,
            config.height
        );
        Ok(CamFeed {
            camera,
            width: w,
            height: h,
            packed: Vec::new(),
            scaled: Vec::new(),
            frames_sent: 0,
            errors: 0,
        })
    }

    /// Tatsächliche Kamera-Dimensionen: Presets nur, wenn der Stream in
    /// BEIDEN Dimensionen größer ist (kein Upscale, kein Aspect-Crop).
    fn target_dims(config: &CamFeedConfig) -> (u32, u32) {
        let (tw, th) = match config.resolution {
            CamResolution::Stream => (config.width, config.height),
            CamResolution::P720 => (1280, 720),
            CamResolution::P1080 => (1920, 1080),
        };
        if tw <= config.width && th <= config.height {
            (tw, th)
        } else {
            (config.width, config.height)
        }
    }

    /// Nimmt einen Frame aus gestrideten NV12-Planes (DecodedFrame-Layout)
    /// und publiziert ihn. Fehler werden gezählt (erster geloggt); ein
    /// fehlgeschlagener Frame killt nicht den Feed.
    pub fn push_nv12(
        &mut self,
        w: u32,
        h: u32,
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
    ) {
        let result = self.push_nv12_checked(w, h, y, y_stride, uv, uv_stride);
        if let Err(err) = result {
            self.errors += 1;
            if self.errors == 1 {
                tracing::warn!("Kamera-Feed verwirft Frame: {err}");
            }
        }
    }

    /// Publiziert einen bereits gepackten NV12-Frame (`w*h*3/2`, Zeilen-
    /// abstand = width) — Schnellpfad für kontiguierliche Quellen (VSR-
    /// Output-Download mit 64er-Pitch; 3840 ist 64-aligned, dort ist der
    /// Pitch exakt die Breite und die Zwischenkopie entfällt).
    pub fn push_nv12_packed(&mut self, w: u32, h: u32, packed: &[u8]) {
        let expected = w as usize * h as usize * 3 / 2;
        if packed.len() != expected {
            self.reject(format!(
                "gepackter Frame hat {} Bytes, erwartet {} für {w}x{h}",
                packed.len(),
                expected
            ));
            return;
        }
        if (w, h) != (self.width, self.height) {
            self.reject(format!(
                "gepackter Frame {w}x{h} passt nicht zum Kamera-Format {}x{}",
                self.width, self.height
            ));
            return;
        }
        self.send(packed);
    }

    /// Zähler + Einmal-Log für verworfene Frames.
    fn reject(&mut self, reason: String) {
        self.errors += 1;
        if self.errors == 1 {
            tracing::warn!("Kamera-Feed verwirft Frame: {reason}");
        }
    }

    fn push_nv12_checked(
        &mut self,
        w: u32,
        h: u32,
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
    ) -> Result<(), String> {
        if (w, h) != (self.width, self.height) && w < self.width {
            // Quelle kleiner als die Kamera — Upscale kann der Feed nicht
            // (z. B. VSR zur Laufzeit ausgefallen, Kamera läuft im VSR-Format).
            return Err(format!(
                "Quelle {w}x{h} kleiner als Kamera {}x{} — Upscale nicht möglich",
                self.width, self.height
            ));
        }
        // Zielauflösung kleiner als der Frame → erst packen, dann
        // herunterskalieren (Upscale kann der Feed nicht).
        let scaled = self.width < w || self.height < h;
        if scaled {
            // Downscale nötig: erst packen (Stream-Auflösung), dann filtern.
            scaler::pack_nv12_strided(y, y_stride, uv, uv_stride, w, h, &mut self.packed)?;
            scaler::downscale_nv12(&self.packed, w, h, self.width, self.height, &mut self.scaled)?;
        } else {
            scaler::pack_nv12_strided(y, y_stride, uv, uv_stride, w, h, &mut self.packed)?;
        }
        // Feld-Split-Borrows: der Frame ist der fertige Puffer, die Kamera
        // wird mutabel gebraucht.
        let frame: &[u8] = if scaled { &self.scaled } else { &self.packed };
        let CamFeed { camera, frames_sent, errors, .. } = self;
        match camera.send_native(frame) {
            Ok(()) => *frames_sent += 1,
            Err(err) => {
                *errors += 1;
                if *errors == 1 {
                    tracing::warn!("Kamera-Feed: Senden fehlgeschlagen: {err}");
                }
            }
        }
        Ok(())
    }

    /// Sendet einen passenden Frame und zählt ihn.
    fn send(&mut self, frame: &[u8]) {
        match self.camera.send_native(frame) {
            Ok(()) => self.frames_sent += 1,
            Err(err) => {
                self.errors += 1;
                if self.errors == 1 {
                    tracing::warn!("Kamera-Feed: Senden fehlgeschlagen: {err}");
                }
            }
        }
    }

    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    pub fn errors(&self) -> u64 {
        self.errors
    }

    pub fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(w: u32, h: u32, resolution: CamResolution) -> CamFeedConfig {
        CamFeedConfig { width: w, height: h, fps: 60, resolution }
    }

    #[test]
    fn zielmasse_kein_upscale() {
        assert_eq!(CamFeed::target_dims(&config(1920, 1080, CamResolution::Stream)), (1920, 1080));
        assert_eq!(CamFeed::target_dims(&config(1920, 1080, CamResolution::P720)), (1280, 720));
        assert_eq!(CamFeed::target_dims(&config(1920, 1080, CamResolution::P1080)), (1920, 1080));
        // Stream kleiner als Preset → Stream (kein Upscale).
        assert_eq!(CamFeed::target_dims(&config(1280, 720, CamResolution::P1080)), (1280, 720));
        assert_eq!(CamFeed::target_dims(&config(640, 360, CamResolution::P720)), (640, 360));
    }

    #[test]
    fn resolution_ini_roundtrip() {
        for value in ["stream", "720p", "1080p", "quatsch"] {
            let parsed = CamResolution::from_ini_value(value);
            if value == "quatsch" {
                assert_eq!(parsed, CamResolution::Stream, "unbekannt → Stream");
            } else {
                assert_eq!(parsed.ini_value(), value);
            }
        }
    }

    #[test]
    fn open_ohne_obs_schlaegt_sauber_fehl() {
        // Auf einer Maschine ohne OBS-Registrierung liefert open eine
        // verständliche Fehlermeldung statt zu panicken (auf der Dev-
        // Maschine ist OBS installiert — dann öffnet der Test die echte
        // Kamera nicht, weil er nur bei fehlender Registrierung early-
        // returnt; dort deckt das Spike-Example den Öffnungs-Pfad ab).
        if crate::registry::obs_virtualcam_available() {
            return;
        }
        let err = CamFeed::open(config(1280, 720, CamResolution::Stream)).err().expect("muss fehlen");
        assert!(err.contains("OBS Virtual Camera") || err.contains("Kamera"), "{err}");
    }
}
