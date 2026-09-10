// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//! PSN-Login-Flow — Port des PSN-Anmeldeablaufs aus gui/src/qmlbackend.cpp
//! (`psnLoginUrl()`, `handlePsnLoginRedirect()`, `initPsnAuth()`) als
//! eigenständiges wry/WebView2-Fenster (im C++: QtWebEngine-Dialog).
//!
//! # Ablauf (exakt wie im C++)
//!
//! 1. Eigenen gpui-unabhängigen Thread starten, dort ein Win32-Top-Level-
//!    Fenster erzeugen und darin eine wry-WebView mit
//!    [`chiaki_remote::psn_auth::psn_login_url`] öffnen (DUID wie
//!    `chiaki_holepunch_generate_client_device_uid`, Spaces im URL werden
//!    wie bei `QUrl::toEncoded()` zu `%20` kodiert).
//! 2. `navigation_handler`: Landet die Navigation auf
//!    [`chiaki_remote::psn_auth::PSN_REDIRECT_PAGE`]
//!    (`https://remoteplay.dl.playstation.net/remoteplay/redirect`), wird
//!    der Query-Parameter `code` extrahiert (C++:
//!    `QUrlQuery(url).queryItemValue("code")`), die Navigation abgelehnt und
//!    das Fenster geschlossen. Fehlt der Code, wird — wie im C++
//!    ("Redirect URL invalid") — ein Fehler gemeldet.
//! 3. Der Code wird gegen Access-/Refresh-Token getauscht
//!    ([`chiaki_remote::psn_auth::exchange_authorization_code`],
//!    grant_type=authorization_code).
//! 4. [`on_complete`] wird mit dem Ergebnis aufgerufen.
//!
//! # Verkabelung mit der UI (App-Agent)
//!
//! `start_psn_login()` kehrt sofort zurück; der Callback läuft auf dem
//! PSN-Login-Thread, NICHT auf dem GPUI-Hauptthread! Muster:
//!
//! ```ignore
//! let (tx, rx) = std::sync::mpsc::channel();
//! psn_login::start_psn_login(move |result| { let _ = tx.send(result); });
//! // Im GPUI-Executor (z. B. background_spawn): rx.recv() → Ergebnis an
//! // den Hauptthread reichen und dort speichern:
//! //   settings.set_psn_auth_token(r.access_token);
//! //   settings.set_psn_refresh_token(r.refresh_token);
//! //   // Ablauf (C++: QDateTime::currentDateTime().addSecs(expires_in)):
//! //   let expires_at = chiaki_remote::psn_auth::now_unix() + r.expires_in;
//! //   // Account-ID (C++: PSNAccountID-Flow, zweiter Request):
//! //   let bytes = chiaki_remote::psn_auth::fetch_psn_account_id(&r.access_token)?;
//! //   settings.set_psn_account_id(chiaki_settings::psn::account_id_to_b64(&bytes));
//! ```
//!
//! # Win32-Host-Fenster
//!
//! wry 0.44 benötigt ein bestehendes Fenster als Host (`HasWindowHandle`).
//! Da wry selbst kein Fenster erzeugt, wird hier ein schlichtes
//! Top-Level-Fenster der vordefinierten Klasse `"STATIC"` via
//! `CreateWindowExW` angelegt (kein eigener WNDCLASS nötig — die dafür
//! nötige Struktur WNDCLASSW hängt am Feature `Win32_Graphics_Gdi`, das im
//! Workspace nicht aktiviert ist). wry subclassiert das Fenster selbst
//! (WM_SIZE → WebView-Resize). Die Nachrichtenschleife pollt per
//! `PeekMessageW` (10 ms) und endet, wenn das Fenster zerstört wurde
//! (Login abgeschlossen oder vom User geschlossen → [`ChiakiError::Canceled`]).
//!
//! # Tests
//!
//! URL-/Query-/Percent-Decoding-Helper mit Golden-Werten gegen die
//! C++-Strings; KEINE Netzwerk-Calls, kein Fenster in Tests.

// wry 0.44 braucht das Win32-Host-Fenster — der einzige unsafe-Bereich
// dieses Crates (CreateWindowExW/Nachrichtenpump).
#![allow(unsafe_code)]

use std::sync::mpsc;
use std::time::Duration;

use chiaki_core::error::{ChiakiError, ChiakiResult};
use chiaki_remote::holepunch::HolepunchSession;
use chiaki_remote::psn_auth;

/// Ergebnis eines erfolgreichen PSN-Logins (Felder wie in den Settings:
/// `psn_auth_token`, `psn_refresh_token`, `psn_auth_token_expiry`).
pub struct PsnLoginResult {
    pub access_token: String,
    pub refresh_token: String,
    /// `expires_in` der PSN-API in Sekunden; Ablauf-Zeitstempel =
    /// `psn_auth::now_unix() + expires_in` (C++:
    /// `QDateTime::currentDateTime().addSecs(expires_in)`).
    pub expires_in: u64,
}

/// Startet den PSN-Login-Flow in einem eigenen Thread (gpui-unabhängig) und
/// ruft `on_complete` mit dem Ergebnis auf — vom Login-Thread aus! (Zur
/// Übergabe an die GPUI-UI siehe Modulkommentar.)
///
/// Der Callback wird in jedem Fall genau einmal aufgerufen: bei Erfolg mit
/// `Ok(PsnLoginResult)`, sonst mit `Err` (Abbruch durch Fensterschließen →
/// [`ChiakiError::Canceled`], HTTP-Fehler → `HttpNonok`/`Network`,
/// ungültiger Redirect-Code → `InvalidData`).
pub fn start_psn_login(on_complete: impl FnOnce(ChiakiResult<PsnLoginResult>) + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("psn-login".to_owned())
        .spawn(move || {
            let result = run_login_flow();
            on_complete(result);
        });
    if let Err(e) = spawned {
        // Bei Thread-Erzeugungsfehler wurde der Callback mit der Closure
        // wieder gedroppt — er kann nicht mehr aufgerufen werden. (Praktisch
        // nur bei OOM relevant.)
        tracing::error!("psn_login: could not spawn login thread: {e}");
    }
}

/// Der Ablauf auf dem Login-Thread: Fenster + WebView, auf den Redirect-
/// Code warten, Token tauschen.
fn run_login_flow() -> ChiakiResult<PsnLoginResult> {
    // QmlBackend::psnLoginUrl(): LOGIN_URL + "duid=" + duid + "&"
    let duid = HolepunchSession::generate_client_device_uid()?;
    let url = encode_url_spaces(&psn_auth::psn_login_url(&duid));
    tracing::info!("psn_login: opening PSN login URL: {url}");

    let (tx, rx) = mpsc::channel::<ChiakiResult<String>>();

    // Win32-Host-Fenster (siehe Modulkommentar "Win32-Host-Fenster")
    let window = create_login_window()?;

    // WebView in das Fenster; Redirect auf PSN_REDIRECT_PAGE abfangen.
    let webview_result = wry::WebViewBuilder::new(&window)
        .with_url(&url)
        .with_navigation_handler(move |nav_url: String| {
            if !nav_url.starts_with(psn_auth::PSN_REDIRECT_PAGE) {
                return true; // normale Navigation erlauben
            }
            // handlePsnLoginRedirect(): code aus der Query, sonst Fehler
            let code = url_query_param(&nav_url, "code").unwrap_or_default();
            if code.is_empty() {
                tracing::warn!("psn_login: Invalid code from redirect url");
                let _ = tx.send(Err(ChiakiError::InvalidData));
            } else {
                let _ = tx.send(Ok(code));
            }
            // Redirect-Seite nicht laden; Fenster schließt sofort
            destroy_login_window(&window);
            false
        })
        .build();
    let webview = match webview_result {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("psn_login: could not create webview: {e}");
            destroy_login_window(&window);
            return Err(ChiakiError::Unknown);
        }
    };

    pump_until_window_closed(&window);

    // WebView freigeben (WebView2-Controller) und die dabei anstehenden
    // Nachrichten kurz entpumpen (Controller-Close ist async).
    drop(webview);
    drain_messages_bounded(Duration::from_millis(200));

    // Kein Code empfangen → vom User abgebrochen
    let code = match rx.try_recv() {
        Ok(Ok(code)) => code,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            tracing::info!("psn_login: login window closed without completing login");
            return Err(ChiakiError::Canceled);
        }
    };

    // Authorization-Code → Tokens (psntoken.cpp: InitPsnToken)
    let token = psn_auth::exchange_authorization_code(&code)?;
    Ok(PsnLoginResult {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in: token.expires_in,
    })
}

// ---------------------------------------------------------------------------
// Win32-Host-Fenster + Nachrichtenschleife
// ---------------------------------------------------------------------------

/// Das Host-Fenster als wry-`HasWindowHandle` (Copy: HWND ist Copy — so
/// kann der Navigation-Handler eine Kopie behalten).
#[derive(Clone, Copy)]
struct LoginWindow {
    hwnd: windows::Win32::Foundation::HWND,
}

impl wry::raw_window_handle::HasWindowHandle for LoginWindow {
    fn window_handle(
        &self,
    ) -> Result<wry::raw_window_handle::WindowHandle<'_>, wry::raw_window_handle::HandleError> {
        use wry::raw_window_handle::{
            HandleError, RawWindowHandle, Win32WindowHandle, WindowHandle,
        };
        let hwnd = std::num::NonZeroIsize::new(self.hwnd.0 as isize)
            .ok_or(HandleError::Unavailable)?;
        // Sicherheit: hwnd ist ein gültiger, von create_login_window
        // erzeugter Win32-Handle (nur solange das Fenster lebt — der
        // Navigation-Handler läuft vor DestroyWindow).
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(hwnd))) })
    }
}

/// Top-Level-Fenster der vordefinierten Klasse "STATIC" (Größe wie ein
/// Login-Popup, C++ `layout_type=popup`).
fn create_login_window() -> ChiakiResult<LoginWindow> {
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, SetForegroundWindow, SW_SHOW, ShowWindow, WINDOW_EX_STYLE,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::w!("STATIC"),
            windows::core::w!("PSN Login"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            440,
            680,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| {
            tracing::error!("psn_login: CreateWindowExW failed: {e}");
            ChiakiError::Unknown
        })?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        Ok(LoginWindow { hwnd })
    }
}

/// Fenster zerstören (muss vom Thread des Fensters aus geschehen — passiert
/// hier im Navigation-Handler/Flow auf dem Login-Thread).
fn destroy_login_window(window: &LoginWindow) {
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
    unsafe {
        let _ = DestroyWindow(window.hwnd);
    }
}

/// Nachrichtenschleife: arbeitet alle anstehenden Nachrichten ab
/// (WebView2/WebView-Eingaben, WM_SIZE-Resize durch den wry-Subclass) und
/// endet, wenn das Fenster zerstört wurde (Login fertig/abgebrochen).
fn pump_until_window_closed(window: &LoginWindow) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, IsWindow, MSG, PeekMessageW, PM_REMOVE, TranslateMessage,
    };
    unsafe {
        let mut msg = MSG::default();
        while IsWindow(window.hwnd).as_bool() {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// Kurzer, zeitlich begrenzter Nachrichten-Drain (z. B. nach dem
/// WebView-Drop, damit der asynchrone WebView2-Close abgearbeitet wird).
fn drain_messages_bounded(max: Duration) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, PeekMessageW, PM_REMOVE, TranslateMessage,
    };
    let deadline = std::time::Instant::now() + max;
    unsafe {
        let mut msg = MSG::default();
        while std::time::Instant::now() < deadline {
            if !PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------------------
// URL-Helper (QUrlQuery-/QUrl::toEncoded-Äquivalente)
// ---------------------------------------------------------------------------

/// Query-Parameter aus einer URL lesen (C++:
/// `QUrlQuery(url).queryItemValue(key)` — inkl. Percent-Decoding).
pub fn url_query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1.split('#').next()?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Percent-Decoding (`%XX` → Byte, `+` → Leerzeichen).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Leerzeichen im URL für die Übergabe an die WebView kodieren (das C++
/// lädt `QUrl(PSNAuth::LOGIN_URL + "duid=...")` — QUrl kodiert die Spaces
/// des scope-Parameters beim Navigieren als `%20`, vgl. `url.toEncoded()`).
pub fn encode_url_spaces(url: &str) -> String {
    url.replace(' ', "%20")
}

// ---------------------------------------------------------------------------
// Verdrahtung mit der App (Settings + UiEventQueue) — der PSN-Agent-Pfad
// ---------------------------------------------------------------------------

/// Führt den kompletten PSN-Login aus und schreibt das Ergebnis in die
/// Settings — Port von `QmlBackend::initPsnAuth` + `PSNToken`-/
/// `PSNAccountID`-Handlern (psntoken.cpp / psnaccountid.cpp):
///
/// 1. [`start_psn_login`] (wry/WebView2-Fenster) → Access-/Refresh-Token.
/// 2. `settings/psn_auth_token`, `settings/psn_refresh_token`,
///    `settings/psn_auth_token_expiry` (= `now_unix() + expires_in`, als
///    Anzeige-String im UTC-Format — das C++ formatiert
///    `QDateTime::currentDateTime().addSecs(expires_in)` ebenfalls nur für
///    die Anzeige, siehe `psntoken.cpp handleAccessTokenResponse`).
/// 3. `fetch_psn_account_id` (zweiter Request, C++ `PSNAccountID::
///    GetPsnAccountId`) → `settings/psn_account_id` (Base64) — danach ist
///    die Account-ID im Registrierungs-Wizard vorausgefüllt (der liest sie
///    bei jedem `open()` aus den Settings, regist_wizard.rs `open`).
/// 4. Ergebnis als [`UiEvent::Toast`](crate::backend::UiEvent::Toast) in die
///    Event-Queue — die Status-Row der Settings-Seite („PSN & Network“) und
///    die Info-Karte rendern beim nächsten Frame die neuen Werte.
///
/// Thread-Modell: Läuft komplett ohne GPUI-Bezug. Der [`start_psn_login`]-
/// Callback läuft auf dem psn-login-Thread (NICHT GPUI-Thread) — Settings-
/// Lock und Netzrequest sind thread-sicher, deshalb kann der Apply-Teil
/// direkt im Callback passieren (das im Modulkopf dokumentierte
/// mpsc→UiEventQueue-Muster in Reinform: die UI wird ausschließlich über
/// die [`UiEventSender`]-Queue informiert, nie vom Thread aus berührt).
pub fn start_psn_login_for_settings(
    settings: std::sync::Arc<std::sync::Mutex<chiaki_settings::settings::Settings>>,
    events: crate::backend::events::UiEventSender,
) {
    use crate::backend::events::UiEvent;
    use crate::components::{ToastData, ToastKind};

    start_psn_login(move |result| {
        let r = match result {
            Ok(r) => r,
            Err(ChiakiError::Canceled) => {
                // Fenster vom User geschlossen — kein Toast (bewusst still,
                // wie das C++ bei Abbruch).
                tracing::info!("psn_login: abgebrochen (Fenster geschlossen)");
                return;
            }
            Err(err) => {
                tracing::error!("psn_login: Login fehlgeschlagen: {err}");
                events.send(UiEvent::Toast(
                    ToastData::new(ToastKind::Danger, "PSN sign-in failed")
                        .message(err.to_string()),
                ));
                return;
            }
        };

        // Tokens + Ablauf speichern (C++ handleAccessTokenResponse).
        let expires_at_unix = psn_auth::now_unix() + r.expires_in;
        let expiry_text = format_unix_utc(expires_at_unix);
        let access = r.access_token.clone();
        let save = settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .update(|s| {
                s.set_psn_auth_token(access);
                s.set_psn_refresh_token(r.refresh_token.clone());
                s.set_psn_auth_token_expiry(expiry_text.clone());
            });
        if let Err(err) = save {
            tracing::error!("psn_login: Tokens konnten nicht gespeichert werden: {err}");
            events.send(UiEvent::Toast(
                ToastData::new(ToastKind::Danger, "PSN sign-in failed")
                    .message(format!("Could not save settings: {err}")),
            ));
            return;
        }

        // Account-ID holen (zweiter Request, C++ PSNAccountID); schlägt sie
        // fehl, bleiben die Tokens gültig — der Wizard hat dann halt keine
        // vorausgefüllte Account-ID.
        match psn_auth::fetch_psn_account_id(&r.access_token) {
            Ok(account_id) => {
                let b64 = chiaki_settings::psn::account_id_to_b64(&account_id);
                let save = settings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .update(|s| s.set_psn_account_id(b64));
                if let Err(err) = save {
                    tracing::error!("psn_login: Account-ID konnte nicht gespeichert werden: {err}");
                }
                tracing::info!("psn_login: PSN-Anmeldung erfolgreich (Account-ID gespeichert)");
                events.send(UiEvent::Toast(
                    ToastData::new(ToastKind::Success, "PSN connected")
                        .message("Sign-in successful — PSN Remote Play is active."),
                ));
            }
            Err(err) => {
                tracing::error!("psn_login: Account-ID konnte nicht geholt werden: {err}");
                events.send(UiEvent::Toast(
                    ToastData::new(ToastKind::Warn, "PSN partially connected")
                        .message(format!(
                            "Tokens saved, but account ID failed: {err}"
                        )),
                ));
            }
        }
    });
}

/// Unix-Sekunden → „YYYY-MM-DD HH:MM UTC“ (Anzeige-String für
/// `settings/psn_auth_token_expiry`; das C++ speichert ebenfalls einen
/// formatierten Anzeige-String, `expiry.toString(settings->GetTimeFormat())`).
/// Tage→Datum über die zivile Umkehrrechnung (Hinnant), rein auf `std`.
pub fn format_unix_utc(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    // Hinnant: civil_from_days (Epoch 1970-01-01 = Tag 0).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_remote::psn_auth::{PSN_REDIRECT_PAGE, psn_login_url};

    #[test]
    fn redirect_code_extraction() {
        // Redirect-URL wie sie nach erfolgreichem PSN-Login kommt
        let url = concat!(
            "https://remoteplay.dl.playstation.net/remoteplay/redirect",
            "?code=v3.Redirect.Code-abc&state=xyz"
        );
        assert!(url.starts_with(PSN_REDIRECT_PAGE));
        assert_eq!(url_query_param(url, "code"), Some("v3.Redirect.Code-abc".to_owned()));

        // percent-encoded Code
        let url_enc = "https://remoteplay.dl.playstation.net/remoteplay/redirect?code=abc%2Bdef%3D";
        assert_eq!(url_query_param(url_enc, "code"), Some("abc+def=".to_owned()));
    }

    #[test]
    fn redirect_without_code_is_invalid() {
        // handlePsnLoginRedirect(): code.isEmpty() → Fehler
        let url = "https://remoteplay.dl.playstation.net/remoteplay/redirect?other=1";
        let code = url_query_param(url, "code").unwrap_or_default();
        assert!(code.is_empty());
        assert!(!url.starts_with("https://auth.api.sonyentertainmentnetwork.com"));
    }

    #[test]
    fn login_url_encoding_matches_qurl() {
        // Der Login-URL (mit duid-Suffix, wie QmlBackend::psnLoginUrl)
        // werden die scope-Spaces als %20 kodiert übergeben.
        let url = encode_url_spaces(&psn_login_url("DUID123"));
        assert!(url.starts_with(&chiaki_remote::psn_auth::PSN_LOGIN_URL.replace(' ', "%20")));
        assert!(url.ends_with("&duid=DUID123&"), "duid-Suffix bleibt erhalten");
        assert!(!url.contains(' '), "keine rohen Spaces mehr");
        assert!(url.contains("clientapp%20referenceDataService"));
        assert!(url.contains("countryConfig.read%20pushNotification"));
        // Dekodiert wieder der C++-String
        assert_eq!(percent_decode(&url), psn_login_url("DUID123"));
    }

    #[test]
    fn percent_decode_golden() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("%3D%26"), "=&");
        assert_eq!(percent_decode("plain"), "plain");
        // kaputte Sequenz → '%' bleibt erhalten
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn expiry_format_golden() {
        // Bekannte Zeitpunkte (Kalender-Rechnung, keine Sommerzeit — UTC).
        assert_eq!(format_unix_utc(0), "1970-01-01 00:00 UTC");
        // 2026-09-05 12:34:56 UTC (Tage seit Epoch: 20701 → 1_788_566_400)
        assert_eq!(format_unix_utc(1_788_611_696), "2026-09-05 12:34 UTC");
        // Schaltjahr-Tag (2000-02-29 12:00 UTC = 11016 Tage + 12 h)
        assert_eq!(format_unix_utc(951_825_600), "2000-02-29 12:00 UTC");
        // Jahreswechsel (2024-12-31 23:59 UTC, 2024 ist Schaltjahr)
        assert_eq!(format_unix_utc(1_735_689_540), "2024-12-31 23:59 UTC");
    }
}
