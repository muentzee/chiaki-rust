// SPDX-License-Identifier: AGPL-3.0-only
//! Start/Stop des fensterlosen Virtual-Cam-Feeds aus der GUI (HANDOFF §8/V2).
//!
//! Der Feed ist ein **eigener, detachierter Prozess** (`chiaki.exe
//! --virtualcam <adresse>`) — er überlebt das Schließen des GUI-Fensters
//! und lässt sich über den Stop-Event ([`request_stop`]) sauber beenden.
//! Liveness prüft die GUI über den Instanz-Mutex ([`is_running`]).

use std::os::windows::process::CommandExt as _;

use super::Backend;

/// DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP — ohne Konsole, eigener
/// Prozessgruppen-Verbund; überlebt das Schließen des GUI-Fensters.
const SPAWN_FLAGS: u32 = 0x0000_0008 | 0x0000_0200;

/// Host-Adresse für den Headless-Start auflösen (gleiche Reihenfolge wie
/// `virtualcam_headless::resolve_host`, plus Discovery-Fallback): manueller
/// Host mit zugeordnetem registrierten Host zuerst; andernfalls genau eine
/// registrierte + eine gefundene Konsole (typischer 1-Konsole-Haushalt).
pub fn resolve_feed_addr(backend: &Backend) -> Result<String, String> {
    let settings = backend.settings().lock().unwrap_or_else(|e| e.into_inner());
    let registered = settings.registered_hosts();
    let manual = settings.manual_hosts();
    for r in &registered {
        if let Some(m) = manual
            .iter()
            .find(|m| m.registered && m.registered_mac.mac() == r.server_mac.mac())
        {
            return Ok(m.host.clone());
        }
    }
    let discovered = backend.discovery().hosts();
    if registered.len() == 1 && discovered.len() == 1 {
        let addr = discovered[0].host_addr.clone();
        tracing::info!("Headless-Start: manueller Host fehlt — Discovery-Fallback → {addr}");
        return Ok(addr);
    }
    Err("Kein zugeordneter manueller Host und keine eindeutige Discovery-Adresse".to_string())
}

/// Startet den fensterlosen Feed als detachierten Prozess. Schlägt fehl,
/// wenn bereits eine Instanz läuft (der Kindprozess prüft den Instanz-
/// Mutex selbst und beendet sich dann — die GUI bekommt trotzdem eine
/// PID zurück, deshalb hier der Vorab-Check).
pub fn start_headless_now(backend: &Backend) -> Result<u32, String> {
    if chiaki_virtualcam::is_running() {
        return Err("Es läuft bereits ein Headless-Feed".to_string());
    }
    let addr = resolve_feed_addr(backend)?;
    spawn_detached(&addr)
}

/// Spawnt den Headless-Prozess detached (überlebt das GUI-Fenster).
fn spawn_detached(addr: &str) -> Result<u32, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("eigenes exe-Pfad nicht ermittelbar: {e}"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--virtualcam").arg(addr);
    cmd.creation_flags(SPAWN_FLAGS);
    let child = cmd
        .spawn()
        .map_err(|e| format!("Headless-Prozess konnte nicht gestartet werden: {e}"))?;
    tracing::info!("Headless-Feed gestartet (PID {}, Ziel {addr})", child.id());
    Ok(child.id())
}

/// Startet den fensterlosen Feed mit **expliziter Adresse** (Kachel-Kontext:
/// die Kachel kennt die Adresse der Konsole, kein Registry-Lookup nötig).
pub fn start_headless_now_with_addr(backend: &Backend, addr: &str) -> Result<u32, String> {
    if chiaki_virtualcam::is_running() {
        return Err("Es läuft bereits ein Headless-Feed".to_string());
    }
    if addr.is_empty() {
        return Err("Keine Adresse für diese Konsole bekannt".to_string());
    }
    spawn_detached(addr)
}

/// Stop-Signal an die laufende Headless-Instanz. `false` = läuft nicht.
pub fn stop_headless() -> bool {
    chiaki_virtualcam::request_stop()
}
