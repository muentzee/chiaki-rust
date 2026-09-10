//! PSN-Remote-Backend (Port der PSN-Flows aus gui/src/qmlbackend.cpp):
//!
//! * Token-Refresh beim App-Start / vor PSN-Aktionen (`refreshPsnToken`:
//!   expiry − 60 s-Puffer im C++, hier 5 min gemäß Portierungsauftrag —
//!   `refreshAuth()`-Ersatz über [`chiaki_remote::psn_auth::refresh_psn_token`]).
//! * Geräteliste des PSN-Accounts (`updatePsnHostsThread`:
//!   `chiaki_holepunch_list_devices`, 2 Versuche, nur Geräte mit aktiviertem
//!   Remote Play, ergänzt um den „Main PS4 Console“-Platzhalter-DUID).
//! * PSN-Verbindungsaufbau (`connectToHost`/`InitiatePsnConnection`):
//!   HolepunchSession aus den Settings-Token bauen, Port-Guessing-Settings
//!   übernehmen; die eigentliche Holepunch-Sequenz (upnp_discover → create →
//!   create_offer → start → punch_hole(Ctrl)) läuft im Connect-Thread von
//!   [`super::sessions::SessionManager::connect`].
//!
//! Thread-Modell wie im restlichen Backend: alle Netzaufrufe laufen in
//! Wegwerf-Threads, Ergebnisse fließen als [`UiEvent::Psn`] in die Queue; die
//! Geräte-/Status-Schnappschüsse liest die UI 1×/Frame über [`PsnHandle`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chiaki_remote::holepunch::{ConsoleType, DeviceInfo, HolepunchSession};
use chiaki_remote::psn_auth;
use chiaki_settings::settings::Settings;

use super::events::{UiEvent, UiEventSender};
use super::sessions::{ConnectRequest, SessionManager};

/// C++ `PSN_DEVICES_TRIES` (qmlbackend.cpp).
const PSN_DEVICES_TRIES: usize = 2;

/// Refresh-Vorlauf (Portierungsauftrag 5 min; das C++ nutzt 60 s Puffer).
const TOKEN_REFRESH_MARGIN_SECS: u64 = 5 * 60;

fn lock<'a, T>(
    guard: Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>>,
) -> MutexGuard<'a, T> {
    guard.unwrap_or_else(PoisonError::into_inner)
}

/// Ein PSN-Remote-Host für die Konsolen-Liste (C++ `PsnHost`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PsnDeviceInfo {
    /// `device_name` der PSN-Antwort (Anzeigename / Nickname).
    pub nickname: String,
    /// DUID — Hex-Darstellung der 32 `device_uid`-Bytes.
    pub duid: String,
    /// Konsolentyp: true = PS5, false = PS4.
    pub ps5: bool,
    /// C++ `dev.remoteplay_enabled` — Remote Play auf der Konsole aktiv.
    pub remoteplay_enabled: bool,
}

impl PsnDeviceInfo {
    /// Holepunch-Konsolentyp für `HolepunchSession::start()`.
    pub fn console_type(&self) -> ConsoleType {
        if self.ps5 {
            ConsoleType::Ps5
        } else {
            ConsoleType::Ps4
        }
    }

    /// Status-Label für die Kachel.
    pub fn state(&self) -> &'static str {
        if self.remoteplay_enabled {
            "Ready"
        } else {
            "Remote Play off"
        }
    }
}

/// PSN-Geräteliste aus der PSN-Antwort mappen (C++ updatePsnHostsThread):
/// nur Geräte mit aktiviertem Remote Play, DUID = Hex der UID-Bytes.
fn devices_from_psn(devices: &[DeviceInfo]) -> Vec<PsnDeviceInfo> {
    devices
        .iter()
        .filter(|dev| dev.remoteplay_enabled)
        .map(|dev| PsnDeviceInfo {
            nickname: dev.device_name.clone(),
            duid: chiaki_remote::regist_psn::bytes_to_duid(&dev.device_uid),
            ps5: dev.type_ == ConsoleType::Ps5,
            remoteplay_enabled: dev.remoteplay_enabled,
        })
        .collect()
}

/// Inhalt von [`UiEvent::Psn`] — Geräteliste + Verbindungsfortschritt.
#[derive(Debug, Clone)]
pub enum PsnUiEvent {
    /// Geräteliste aktualisiert (list_devices-Thread).
    Devices(Vec<PsnDeviceInfo>),
    /// Geräteliste fehlgeschlagen (Grund; zusätzlich kommt ein Toast).
    DevicesFailed(String),
    /// Holepunch-/Verbindungsaufbau-Fortschritt (C++ PsnConnectState).
    Connecting(PsnConnectState),
}

/// Geteilter PSN-Zustand (Geräteliste + Verbindungszustand für die UI).
#[derive(Clone)]
pub struct PsnHandle {
    devices: Arc<Mutex<Vec<PsnDeviceInfo>>>,
    updating: Arc<AtomicBool>,
    connect_state: Arc<Mutex<Option<PsnConnectState>>>,
}

/// Verbindungszustand des PSN-Aufbaus (Port der C++ `PsnConnectState`-Werte,
/// die die Connecting-Anzeige steuern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PsnConnectState {
    /// Holepunch-Sequenz läuft (upnp/create/offer/start/punch Ctrl).
    InitiatingConnection,
    /// Control-Hole steht — Session-Start (C++ `LinkingConsole`).
    LinkingConsole,
    /// Data-Hole wird gepuncht (SessionEvent::Holepunch !finished).
    DataConnectionStart,
    /// Data-Hole steht (SessionEvent::Holepunch finished).
    DataConnectionFinished,
}

impl Default for PsnHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl PsnHandle {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(Mutex::new(Vec::new())),
            updating: Arc::new(AtomicBool::new(false)),
            connect_state: Arc::new(Mutex::new(None)),
        }
    }

    /// Aktueller Geräteschnappschuss (UI liest 1×/Frame).
    pub fn devices(&self) -> Vec<PsnDeviceInfo> {
        lock(self.devices.lock()).clone()
    }

    /// Geräte aus dem List-Devices-Thread übernehmen (app.rs wendet das
    /// `UiEvent::Psn::Devices` an).
    pub fn apply_devices(&self, devices: Vec<PsnDeviceInfo>) {
        *lock(self.devices.lock()) = devices;
    }

    /// Letzter Verbindungszustand (Connecting-Anzeige).
    pub fn connect_state(&self) -> Option<PsnConnectState> {
        *lock(self.connect_state.lock())
    }

    pub fn apply_connect_state(&self, state: PsnConnectState) {
        *lock(self.connect_state.lock()) = Some(state);
    }

    /// PSN-Gerät per DUID nachschlagen (für ps5/Label-Auflösung).
    pub fn device(&self, duid: &str) -> Option<PsnDeviceInfo> {
        lock(self.devices.lock()).iter().find(|d| d.duid == duid).cloned()
    }

    /// Token-Refresh, falls nötig, danach Geräteliste aktualisieren — Port
    /// von `QmlBackend::refreshPsnToken()` (App-Start / vor PSN-Aktionen).
    /// Ohne Refresh-Token/Expiry ein stiller No-Op (wie das C++, damit der
    /// Start ohne PSN-Login keine Fehlermeldungen produziert).
    pub fn refresh_tokens_if_needed(
        &self,
        settings: Arc<Mutex<Settings>>,
        events: UiEventSender,
    ) {
        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("psn-token-refresh".into())
            .spawn(move || {
                let (refresh, expiry) = {
                    let s = lock(settings.lock());
                    (s.psn_refresh_token(), s.psn_auth_token_expiry())
                };
                // C++ refreshPsnToken: leere Werte → still return.
                if refresh.is_empty() || expiry.is_empty() {
                    return;
                }
                let needs_refresh = match parse_expiry_unix(&expiry) {
                    Some(expires_at) => {
                        psn_auth::now_unix() + TOKEN_REFRESH_MARGIN_SECS >= expires_at
                    }
                    // Nicht parsebares Ablaufdatum → sicherheitshalber refreshen.
                    None => true,
                };
                if !needs_refresh {
                    tracing::info!("PSN-Token noch gültig bis {expiry} — kein Refresh nötig");
                    this.list_devices(settings, events, true);
                    return;
                }
                tracing::info!("PSN-Token läuft ab ({expiry}) — Refresh …");
                match psn_auth::refresh_psn_token(&refresh) {
                    Ok(token) => {
                        let expiry_text = crate::psn_login::format_unix_utc(token.expires_at_unix);
                        let save = lock(settings.lock()).update(|s| {
                            s.set_psn_auth_token(token.access_token.clone());
                            s.set_psn_refresh_token(token.refresh_token.clone());
                            s.set_psn_auth_token_expiry(expiry_text);
                        });
                        if let Err(err) = save {
                            tracing::error!("PSN-Token konnte nicht gespeichert werden: {err}");
                            return;
                        }
                        tracing::info!("PSN-Token aktualisiert");
                        this.list_devices(settings, events, true);
                    }
                    Err(err) => {
                        // C++: UnauthorizedError → psnCredsExpired (erneute
                        // Anmeldung nötig); sonst nur Log + kein Update.
                        tracing::error!("PSN-Token-Refresh fehlgeschlagen: {err}");
                        events.send(UiEvent::Toast(
                            crate::components::ToastData::new(
                                crate::components::ToastKind::Warn,
                                "PSN sign-in expired",
                            )
                            .message(format!(
                                "Token refresh failed ({err}) — please sign in again in \
                                 Settings."
                            )),
                        ));
                    }
                }
            });
        if let Err(err) = spawned {
            tracing::error!("PSN-Refresh-Thread konnte nicht gestartet werden: {err}");
        }
    }

    /// Geräteliste laden (C++ `updatePsnHosts`) — Thread + UiEvent.
    /// `quiet=false` meldet fehlende Anmeldung als Toast (Nutzer-Aktion),
    /// `quiet=true` ist still (App-Start-Pfad, wie `updatePsnHostsThread`
    /// mit leerem Token).
    pub fn list_devices(
        &self,
        settings: Arc<Mutex<Settings>>,
        events: UiEventSender,
        quiet: bool,
    ) {
        // Läuft bereits ein Update? (C++ `updating_psn_hosts`-Guard)
        if self.updating.swap(true, Ordering::Relaxed) {
            tracing::info!("Already updating psn hosts, skipping...");
            return;
        }
        let token = lock(settings.lock()).psn_auth_token();
        if token.is_empty() {
            self.updating.store(false, Ordering::Relaxed);
            if !quiet {
                events.send(UiEvent::Toast(
                    crate::components::ToastData::new(
                        crate::components::ToastKind::Warn,
                        "Not signed in to PSN",
                    )
                    .message("Connect your PSN account in Settings first."),
                ));
            }
            return;
        }

        let this = self.clone();
        let spawned = std::thread::Builder::new()
            .name("psn-devices".into())
            .spawn(move || {
                let result = HolepunchSession::list_devices(&token, ConsoleType::Ps5);
                let devices = match result {
                    Ok(devices) => devices,
                    Err(first_err) => {
                        // C++: zweiter Versuch, dann aufgeben.
                        match HolepunchSession::list_devices(&token, ConsoleType::Ps5) {
                            Ok(devices) => devices,
                            Err(_) => {
                                tracing::error!(
                                    "Failed to get PS5 devices after max tries: {PSN_DEVICES_TRIES} \
                                     ({first_err})"
                                );
                                this.updating.store(false, Ordering::Relaxed);
                                events.send(UiEvent::Psn(super::psn::PsnUiEvent::DevicesFailed(
                                    format!("PSN device list failed: {first_err}"),
                                )));
                                events.send(UiEvent::Toast(
                                    crate::components::ToastData::new(
                                        crate::components::ToastKind::Danger,
                                        "PSN device list failed",
                                    )
                                    .message(first_err.to_string()),
                                ));
                                return;
                            }
                        }
                    }
                };
                let mut list = devices_from_psn(&devices);
                // C++: „Main PS4 Console“-Platzhalter (PS4 listet sich nicht
                // über die Device-Liste; 32×'A'-DUID), sobald PS4-Hosts in
                // der Registry stehen.
                let has_ps4_registered = lock(settings.lock())
                    .registered_hosts()
                    .iter()
                    .any(|h| !h.target.is_ps5());
                if has_ps4_registered && !list.iter().any(|d| d.duid == PS4_PLACEHOLDER_DUID) {
                    list.push(PsnDeviceInfo {
                        nickname: "Main PS4 Console".to_string(),
                        duid: PS4_PLACEHOLDER_DUID.to_string(),
                        ps5: false,
                        remoteplay_enabled: true,
                    });
                }
                this.apply_devices(list);
                this.updating.store(false, Ordering::Relaxed);
                events.send(UiEvent::Psn(super::psn::PsnUiEvent::Devices(this.devices())));
                tracing::info!("Updated PSN hosts");
            });
        if let Err(err) = spawned {
            self.updating.store(false, Ordering::Relaxed);
            tracing::error!("PSN-Devices-Thread konnte nicht gestartet werden: {err}");
        }
    }
}

/// C++-Platzhalter-DUID für die „Main PS4 Console“
/// (`QByteArray(32, 'A')` → 32 Bytes 0x41 → Hex).
pub const PS4_PLACEHOLDER_DUID: &str = "4141414141414141414141414141414141414141414141414141414141414141";

/// Baut den ConnectRequest für eine PSN-Remote-Verbindung (Port von
/// `connectToHost`-PSN-Zweig + `StreamSession::InitiatePsnConnection`):
/// prüft Token/Account-ID, erzeugt die HolepunchSession und übernimmt die
/// Port-Guessing-Settings. Netzwerkfrei — von der UI aufrufbar.
pub fn build_psn_connect_request(
    settings: &Settings,
    duid: &str,
    ps5: bool,
) -> Result<ConnectRequest, String> {
    let token = settings.psn_auth_token();
    if token.is_empty() {
        return Err(
            "Nicht bei PSN angemeldet \u{2014} bitte in den Einstellungen \u{201e}PSN\u{201c} \
             verbinden."
                .to_string(),
        );
    }
    // C++ StreamSession-Konstruktor: psn_account_id muss 8 Base64-decodierte
    // Bytes sein, sonst Exception "Invalid Account-ID".
    let account_id = settings.psn_account_id_bytes().ok_or_else(|| {
        "PSN-Account-ID fehlt oder ist ung\u{fc}ltig \u{2014} bitte PSN-Anmeldung in den \
         Einstellungen wiederholen."
            .to_string()
    })?;

    // C++ InitiatePsnConnection: chiaki_holepunch_session_init + Port-Guessing.
    let holepunch = HolepunchSession::new(&token)
        .map_err(|e| format!("Failed to initialize PSN holepunch session: {e}"))?;
    holepunch.force_port_guessing(settings.port_guessing_enabled());
    holepunch.set_port_guessing_ports(settings.port_guess_count() as i32);
    holepunch.set_port_guessing_socks(settings.port_guess_socket_count() as i32);

    Ok(ConnectRequest::from_psn(
        duid.to_string(),
        ps5,
        account_id,
        Arc::new(holepunch),
    ))
}

/// Baut den Request + startet die Session über den SessionManager
/// (Port des `QmlBackend::connectToHost`-PSN-Zweigs). Fehler (kein Token,
/// keine Account-ID) kommen als `Err` zurück — die UI zeigt sie als Toast.
pub fn connect_psn_device(
    settings: &Arc<Mutex<Settings>>,
    sessions: &SessionManager,
    duid: &str,
    ps5: bool,
) -> Result<(), String> {
    let request = {
        let guard = lock(settings.lock());
        build_psn_connect_request(&guard, duid, ps5)?
    };
    sessions.start_session(request);
    Ok(())
}

/// Parst den Ablaufzeit-String der Settings (`format_unix_utc`-Format
/// „YYYY-MM-DD HH:MM UTC“) zurück in Unix-Sekunden; `None` bei fremden
/// Formaten.
pub fn parse_expiry_unix(text: &str) -> Option<u64> {
    parse_expiry_unix_impl(text.trim_end_matches(" UTC"))
}

fn parse_expiry_unix_impl(text: &str) -> Option<u64> {
    // "YYYY-MM-DD HH:MM"
    let (date, time) = text.split_once(' ')?;
    let date_parts: Vec<&str> = date.split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let y = date_parts[0].parse::<i64>().ok()?;
    let m = date_parts[1].parse::<u32>().ok()?;
    let d = date_parts[2].parse::<i64>().ok()?;
    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 2 {
        return None;
    }
    let hh = time_parts[0].parse::<u64>().ok()?;
    let mm = time_parts[1].parse::<u64>().ok()?;
    if !(1..=12).contains(&m) || hh > 23 || mm > 59 {
        return None;
    }
    // Hinnant days_from_civil.
    let m = i64::from(m);
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days.max(0) as u64) * 86_400 + hh * 3600 + mm * 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::events::UiEventQueue;
    use crate::backend::tests::test_settings;

    #[test]
    fn connect_request_ohne_token_liefert_sauberen_fehler() {
        // Erwarteter Smoke-Pfad ohne PSN-Login: Token leer → verständliche
        // Fehlermeldung statt Panic/Netzcall.
        let settings = test_settings();
        let err = build_psn_connect_request(&settings, &"41".repeat(32), true).unwrap_err();
        assert!(err.contains("Nicht bei PSN angemeldet"), "err={err}");

        // Mit Token, aber ohne Account-ID → likewise sauber.
        let mut settings = test_settings();
        settings.set_psn_auth_token("dummy-token".to_string());
        let err = build_psn_connect_request(&settings, &"41".repeat(32), true).unwrap_err();
        assert!(err.contains("Account-ID"), "err={err}");
    }

    #[test]
    fn connect_request_mit_token_baut_holepunch_session() {
        // Mit (fake-)Token + Account-ID entsteht ein ConnectRequest mit
        // Holepunch-Session, Remote-Profil und Account-ID (kein Netzcall —
        // HolepunchSession::new ist trivial).
        let mut settings = test_settings();
        settings.set_psn_auth_token("dummy-token".to_string());
        settings.set_psn_account_id(
            chiaki_settings::psn::account_id_to_b64(&[1, 2, 3, 4, 5, 6, 7, 8]),
        );
        let req = build_psn_connect_request(&settings, &"41".repeat(32), false).expect("request");
        assert!(req.psn.is_some());
        assert!(!req.ps5);
        assert_eq!(req.psn_account_id, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(req.link, crate::backend::sessions::LinkQuality::Remote);
        assert_eq!(req.regist_key, [0; 16], "PSN: Regist-Daten liefert das PSN-Regist");
        assert_eq!(req.morning, [0; 16]);
    }

    #[test]
    fn list_devices_ohne_token_toastet_nur_bei_nutzeraktion() {
        // Smoke ohne PSN-Login: quiet=true (App-Start) bleibt komplett still;
        // quiet=false (Nutzer-Aktion) produziert genau einen Warn-Toast.
        let handle = PsnHandle::new();

        let queue_quiet = UiEventQueue::new();
        handle.list_devices(
            Arc::new(Mutex::new(test_settings())),
            queue_quiet.sender(),
            true,
        );
        assert!(
            queue_quiet.poll().is_empty(),
            "App-Start-Pfad ohne Token darf kein Event erzeugen"
        );
        assert!(handle.devices().is_empty());

        let queue_loud = UiEventQueue::new();
        handle.list_devices(
            Arc::new(Mutex::new(test_settings())),
            queue_loud.sender(),
            false,
        );
        let events = queue_loud.poll();
        assert_eq!(events.len(), 1, "genau ein Toast-Event, events={events:?}");
        assert!(matches!(&events[0], UiEvent::Toast(t) if t.title.contains("PSN")));
    }

    #[test]
    fn refresh_ohne_psn_credentials_ist_stiller_noop() {
        // C++ refreshPsnToken: leere Refresh/Expiry-Werte → still return.
        let handle = PsnHandle::new();
        let queue = UiEventQueue::new();
        handle.refresh_tokens_if_needed(
            Arc::new(Mutex::new(test_settings())),
            queue.sender(),
        );
        // Der Thread terminiert sofort ohne Netzcall und ohne Events.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            queue.poll().is_empty(),
            "ohne PSN-Credentials darf der Start-Refresh nichts tun"
        );
    }

    #[test]
    fn connect_state_wird_gespiegelt() {
        let handle = PsnHandle::new();
        assert_eq!(handle.connect_state(), None);
        handle.apply_connect_state(PsnConnectState::InitiatingConnection);
        assert_eq!(handle.connect_state(), Some(PsnConnectState::InitiatingConnection));
        handle.apply_connect_state(PsnConnectState::LinkingConsole);
        assert_eq!(handle.connect_state(), Some(PsnConnectState::LinkingConsole));
        handle.apply_devices(vec![PsnDeviceInfo {
            nickname: "PS5-Test".into(),
            duid: "abcd".into(),
            ps5: true,
            remoteplay_enabled: true,
        }]);
        assert_eq!(handle.device("abcd").map(|d| d.nickname), Some("PS5-Test".into()));
        assert!(handle.device("ffff").is_none());
    }

    #[test]
    fn expiry_roundtrip_with_format_unix_utc() {
        // Roundtrips gegen den Formatierer aus psn_login.rs (minute-genau —
        // format_unix_utc schneidet Sekunden ab).
        for unix in [
            0u64,
            1_788_611_640, // 2026-09-05 12:34 UTC
            951_825_600,   // 2000-02-29 12:00 UTC (Schaltjahr)
            1_735_689_540, // 2024-12-31 23:59 UTC
        ] {
            let text = crate::psn_login::format_unix_utc(unix);
            assert_eq!(parse_expiry_unix(&text), Some(unix), "text={text}");
        }
        // Sekunden werden abgeschnitten (Settings speichern nur Minute-genau).
        assert_eq!(
            parse_expiry_unix(&crate::psn_login::format_unix_utc(1_788_611_699)),
            Some(1_788_611_640)
        );
    }

    #[test]
    fn expiry_parse_rejects_foreign_formats() {
        // Das C++ speichert locale-Strings („… MESZ“) — solche Altstände
        // führen zu None → sicherheitshalber Refresh.
        assert_eq!(parse_expiry_unix(""), None);
        assert_eq!(parse_expiry_unix("2026-09-05 14:27:36 MESZ"), None);
        assert_eq!(parse_expiry_unix("nonsense"), None);
        assert_eq!(parse_expiry_unix("2026-13-05 10:00 UTC"), None);
    }

    #[test]
    fn devices_mapping_filters_and_maps() {
        let devices = vec![
            DeviceInfo {
                type_: ConsoleType::Ps5,
                device_name: "Wohnzimmer".to_string(),
                device_uid: [0x41; 32],
                remoteplay_enabled: true,
            },
            DeviceInfo {
                type_: ConsoleType::Ps5,
                device_name: "Aus".to_string(),
                device_uid: [0x42; 32],
                remoteplay_enabled: false,
            },
        ];
        let mapped = devices_from_psn(&devices);
        assert_eq!(mapped.len(), 1, "remoteplay_enabled=false wird gefiltert");
        assert_eq!(mapped[0].nickname, "Wohnzimmer");
        assert_eq!(mapped[0].duid, PS4_PLACEHOLDER_DUID);
        assert!(mapped[0].ps5);
        assert_eq!(mapped[0].console_type(), ConsoleType::Ps5);
        assert_eq!(mapped[0].state(), "Ready");
    }

    #[test]
    fn ps4_placeholder_duid_matches_cpp() {
        // QByteArray(32, 'A') → Hex
        assert_eq!(PS4_PLACEHOLDER_DUID, format!("{}", "41".repeat(32)));
        assert_eq!(chiaki_remote::regist_psn::parse_duid(PS4_PLACEHOLDER_DUID).unwrap(), [0x41; 32]);
    }
}
