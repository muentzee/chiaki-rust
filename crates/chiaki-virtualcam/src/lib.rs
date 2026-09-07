// SPDX-License-Identifier: AGPL-3.0-only
//! chiaki-virtualcam: Feed des Stream-Videopaths in eine virtuelle Kamera.
//!
//! Der gestreamte Inhalt wird als virtuelle Webcam („OBS Virtual Camera")
//! veröffentlicht, die Discord/OBS/… wie eine normale Kamera einbinden
//! können (HANDOFF §8). Architektur:
//!
//! * **Transport** — `virtualcam`-Crate, OBS-Backend (Entscheidung D7 aus
//!   HANDOFF §8): wir sind der **Writer** der Shared-Memory-Queue
//!   (`OBSVirtualCamVideo`, 80-Byte-Header + 3 NV12-Slots), der bei der
//!   OBS-Installation registrierte DirectShow-Filter ist der Reader. OBS
//!   selbst muss nur INSTALLIERT sein (Filter-Registrierung), nicht laufen;
//!   dafür darf OBS seine Virtual Camera nicht parallel selbst starten
//!   (zwei Writer = Konflikt). OBS kann die Kamera gleichzeitig als
//!   Video-Capture-Quelle einbinden (Twitch-Szenario) — es ist dann nur
//!   Konsument.
//! * **Frames** — der Decoder liefert NV12; das OBS-Backend konsumiert
//!   NV12. Kein I420-Umbau nötig, nur das Entstrippen der NVDEC-/VSR-
//!   Strides in einen gepackten Buffer ([`scaler::pack_nv12_strided`],
//!   optional Downscale auf Kamera-Standardauflösung via [`scaler`]).
//! * **Tap** — der Media-Thread der Session (chiaki-ui `sessions.rs`) bzw.
//!   der Headless-Runner (chiaki-app) schieben jeden dekodierten Frame
//!   **vor** VSR in den [`feed::CamFeed`] (Default: Stream-Auflösung —
//!   Kamera-Standard; VSR-Output als Quelle ist bewusst nicht v1).
//! * **Lebenszyklus** — Kamera existiert nur während einer Session
//!   (Mapping wird beim Session-Ende freigegeben; Konsumenten zeigen dann
//!   kein Bild). Erste Nutzung: Discord ggf. neu starten, damit die Kamera
//!   in der Geräteliste auftaucht.
//!
//! UI-frei (CONVENTIONS §7) und ohne eigene Filter-DLL/Registrierung.

pub mod feed;
pub mod registry;
pub mod scaler;

pub use feed::{CamFeed, CamFeedConfig, CamResolution};
pub use registry::{autostart_active, obs_virtualcam_available, set_autostart};
