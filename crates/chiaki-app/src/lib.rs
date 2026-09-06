//! chiaki-app — Bibliotheksanteil des Binaries (Glue/Lifecycle).
//!
//! Enthält aktuell die Steam-Library-Shortcut-Verwaltung ([`steam`]),
//! einen 1:1-Port von `third-party/cpp-steam-tools` (VDF-Shortcuts-Editor)
//! plus der Parameter-Logik aus `gui/src/qmlbackend.cpp`
//! (`QmlBackend::createSteamShortcut`, "Add to Steam library").

#![deny(unsafe_code)]

pub mod steam;
