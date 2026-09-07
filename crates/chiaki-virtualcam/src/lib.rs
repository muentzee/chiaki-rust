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
//! * **Frames** — das OBS-Backend konsumiert NV12; die Quelle liefert sie
//!   passend: bei aktivem VSR der **VSR-Output** (RGBA→NV12 auf der GPU +
//!   Download — die Upscale-Schärfe geht an die Viewer, User-Vorgabe),
//!   sonst der dekodierte Stream-Frame. Nur das Entstrippen der NVDEC-
//!   Strides in einen gepackten Buffer ([`scaler::pack_nv12_strided`]),
//!   optional Downscale auf 720p/1080p via [`scaler`] (ohne VSR).
//! * **Tap** — der Media-Thread der Session (chiaki-ui `sessions.rs`) bzw.
//!   der Headless-Runner (chiaki-app) schieben den jeweils schärfsten
//!   verfügbaren Frame in den [`feed::CamFeed`].
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
