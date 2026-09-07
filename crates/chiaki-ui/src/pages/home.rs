//! Home (ui-v2-spec §2.1): Hero-Karte der zuletzt verbundenen Konsole,
//! Reihe „Deine Konsolen“ (240×140-Kacheln mit Kontextmenü), Schnellaktionen
//! und Erststart-Leerzustand (Willkommens-Fluss).
//!
//! Referenz: `qml2/pages/HomePage.qml` + `QmlBackend::hosts()` (C++). Der
//! Host-Mix (Discovery + Manuell, versteckte Hosts raus, Registrierungs-
//! Abgleich über die MAC aus `host_id`) ist 1:1 wie im C++ aufgebaut; das
//! Modell ([`ConsoleEntry`]) und die Aktionen werden mit der Konsolen-Seite
//! geteilt (Spec §2.2: identisches Kontextmenü).
//!
//! Abweichung Hero (wie im QML dokumentiert): „zuletzt verbunden“ gibt es
//! nicht in der Settings-API — Proxy ist `settings/auto_connect_mac`
//! (`auto_connect_host()`), sonst der erste Host. Bei genau EINER Konsole
//! versteckt der Hero sich (keine Doppelanzeige), die Kachel ist der
//! einzige Einstieg.

use gpui::{
    div, px, Context, FocusHandle, FontWeight, IntoElement, InteractiveElement as _, MouseButton,
    ParentElement as _, Styled, Window,
};

use chiaki_settings::hosts::{HostMac, ManualHost, RegisteredHost};

use crate::app::{AppShell, Route};
use crate::backend::HostId;
use crate::components::{
    Button, ButtonVariant, Card, Dialog, DialogButton, EmptyState, SectionLabel, StatusBadge,
    StatusKind, ToastData, ToastKind,
};
use crate::icons;
use crate::pages::regist_wizard;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

// ---------------------------------------------------------------------------
// Geteiltes Konsolen-Modell (Home + Konsolen — Spec §2.1/§2.2)
// ---------------------------------------------------------------------------

/// Eine Konsole für die UI — Port des `QVariantMap` aus `QmlBackend::hosts()`.
#[derive(Debug, Clone)]
pub(crate) struct ConsoleEntry {
    pub name: String,
    /// IP/Host (leer bei reinen PSN-Remote-Hosts).
    pub addr: String,
    pub ps5: bool,
    /// Reiner PSN-Remote-Host — Klick startet den Holepunch-PSN-Flow.
    pub psn: bool,
    /// DUID des PSN-Remote-Hosts (nur bei `psn == true`).
    pub duid: Option<String>,
    pub status: StatusKind,
    pub status_label: &'static str,
    /// Laufende App (Discovery `running_app_name`).
    pub running_app: Option<String>,
    /// MAC (aus Discovery `host_id` oder der Registry) — Schlüssel für
    /// Aufwachen/Verstecken/Löschen wie im C++ (`HostMAC`).
    pub mac: Option<[u8; 6]>,
    pub registered: Option<RegisteredHost>,
    pub manual: Option<ManualHost>,
}

/// Port von `QByteArray::fromHex` + `DiscoveryHost::GetHostMAC`:
/// Hex-Zeichen sammeln (Nicht-Hex übersprungen), exakt 6 Bytes → MAC.
pub(crate) fn mac_from_host_id(host_id: &str) -> Option<[u8; 6]> {
    let mut bytes = Vec::with_capacity(6);
    let mut nibble = None;
    for c in host_id.bytes() {
        let v = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => continue,
        };
        match nibble.take() {
            None => nibble = Some(v << 4),
            Some(hi) => bytes.push(hi | v),
        }
    }
    <[u8; 6]>::try_from(bytes).ok()
}

/// Host-Mix wie `QmlBackend::hosts()`: Discovery-Hosts (versteckte raus,
/// MAC-Abgleich mit der Registry) + manuelle Hosts (ohne Discovery-Dublette)
/// + PSN-Remote-Hosts (nur solche, die nicht lokal entdeckt wurden — C++:
/// "Only list PSN remote hosts that aren't discovered locally").
pub(crate) fn console_entries(shell: &AppShell) -> Vec<ConsoleEntry> {
    let settings = shell.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<ConsoleEntry> = Vec::new();

    for host in shell.backend.discovery().hosts() {
        let addr = host.host_addr.clone();
        let name = host
            .host_name
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| addr.clone());
        let mac = host.host_id.as_deref().and_then(mac_from_host_id);
        let registered = mac
            .as_ref()
            .and_then(|m| settings.registered_host(HostMac::new(*m)))
            .cloned();
        // C++: `display = !hidden` — versteckte Konsolen zeigen wir nicht
        // (das C++-Auto-Unhide für registrierte Hosts ist ein Settings-
        // Nebeneffekt, den wir bewusst nicht nachbauen).
        let hidden = mac
            .as_ref()
            .map(|m| settings.hidden_host_hidden(HostMac::new(*m)))
            .unwrap_or(false);
        if hidden {
            continue;
        }
        let (status, status_label) = match host.state {
            chiaki_core::discovery::DiscoveryHostState::Ready => (StatusKind::Ready, "Bereit"),
            chiaki_core::discovery::DiscoveryHostState::Standby => (StatusKind::Standby, "Standby"),
            chiaki_core::discovery::DiscoveryHostState::Unknown => {
                if registered.is_some() {
                    (StatusKind::Offline, "Offline")
                } else {
                    (StatusKind::Unregistered, "Nicht registriert")
                }
            }
        };
        out.push(ConsoleEntry {
            name,
            addr,
            ps5: host.is_ps5(),
            psn: false,
            duid: None,
            status,
            status_label,
            running_app: host.running_app_name.clone().filter(|a| !a.is_empty()),
            mac,
            registered,
            manual: None,
        });
    }

    // Manuelle Hosts — bereits entdeckte überspringen (C++: `display = false`
    // für `discovered_manual_hosts`, keine Dubletten im Grid).
    for manual in settings.manual_hosts().iter().map(|m| (*m).clone()) {
        if out.iter().any(|e| e.addr == manual.host) {
            continue;
        }
        let registered = manual
            .registered
            .then(|| settings.registered_host(manual.registered_mac))
            .flatten()
            .cloned();
        let name = registered
            .as_ref()
            .map(|r| r.server_nickname.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| manual.host.clone());
        let (status, status_label) = if registered.is_some() {
            (StatusKind::Offline, "Offline")
        } else {
            (StatusKind::Unregistered, "Nicht registriert")
        };
        out.push(ConsoleEntry {
            name,
            addr: manual.host.clone(),
            ps5: registered.as_ref().map(|r| r.target.is_ps5()).unwrap_or(false),
            psn: false,
            duid: None,
            status,
            status_label,
            running_app: None,
            mac: registered.as_ref().map(|r| *r.server_mac.mac()),
            registered,
            manual: Some(manual),
        });
    }

    // PSN-Remote-Hosts aus der PSN-Geräteliste (C++ updatePsnHostsThread →
    // hosts(): nur PSN-Hosts, die nicht lokal entdeckt wurden; Klick startet
    // den Holepunch-Connect-Flow).
    let discovered_names: Vec<String> = out.iter().map(|e| e.name.clone()).collect();
    for device in shell.backend.psn().devices() {
        if discovered_names.iter().any(|n| *n == device.nickname) {
            continue;
        }
        let (status, status_label) = if device.remoteplay_enabled {
            (StatusKind::Ready, "Bereit")
        } else {
            (StatusKind::Offline, "Remote Play aus")
        };
        out.push(ConsoleEntry {
            name: device.nickname,
            addr: String::new(),
            ps5: device.ps5,
            psn: true,
            duid: Some(device.duid),
            status,
            status_label,
            running_app: None,
            mac: None,
            registered: None,
            manual: None,
        });
    }

    out
}

// ---------------------------------------------------------------------------
// Geteilte Aktionen (Verbinden / Aufwachen / Kontextmenü)
// ---------------------------------------------------------------------------

fn toast(kind: ToastKind, title: &str, message: impl Into<gpui::SharedString>) -> ToastData {
    ToastData::new(kind, title.to_string()).message(message)
}

/// Konsole aufwecken — Port des Wake-Flows (`QmlBackend::wakeUpHost` →
/// `DiscoveryManager::SendWakeup` → `chiaki_discovery_wakeup`): der
/// RP-Regist-Key des registrierten Hosts wird als Discovery-Wakeup-
/// Paket an die Konsole geschichtet (`discovery().wake()`).
pub(crate) fn wake_entry(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let Some(registered) = entry.registered.as_ref() else {
        shell.push_toast(
            toast(ToastKind::Warn, "Nicht registriert", "Nur registrierte Konsolen können aufgeweckt werden."),
            cx,
        );
        return;
    };
    if entry.addr.is_empty() {
        shell.push_toast(toast(ToastKind::Warn, "Keine Adresse", "Für diese Konsole ist keine IP bekannt."), cx);
        return;
    }
    match shell
        .backend
        .discovery()
        .wake(&entry.addr, &registered.rp_regist_key, entry.ps5)
    {
        Ok(()) => shell.push_toast(
            toast(
                ToastKind::Success,
                "Aufwachen gesendet",
                format!("„{}“ wird geweckt — der Start dauert einen Moment.", entry.name),
            ),
            cx,
        ),
        Err(err) => shell.push_toast(
            toast(ToastKind::Danger, "Aufwachen fehlgeschlagen", err.to_string()),
            cx,
        ),
    }
}

/// Verbinden — Port von `connectHost`/`startSession`: PSN-Remote-Kacheln
/// starten den Holepunch-Flow (`build_psn_connect_request` prüft Token/
/// Account-ID und liefert saubere Fehlermeldungen, wenn keine PSN-Anmeldung
/// vorliegt — der erwartete Smoke-Pfad ohne echten PSN-Account), sonst
/// ConnectRequest aus dem registrierten Host (+ ggf. manuellem Host-Eintrag)
/// bauen, Session non-blocking starten und auf die Stream-Ansicht wechseln.
pub(crate) fn connect_entry(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    // PSN-Remote-Host (C++ connectToHost-PSN-Zweig): Token-Check + Request.
    if entry.psn {
        let Some(duid) = entry.duid.clone() else {
            shell.push_toast(
                toast(ToastKind::Warn, "Keine PSN-Konsole", "Dieser Eintrag hat keine DUID."),
                cx,
            );
            return;
        };
        // Vorab-Check (Token + Account-ID) — der Request selbst wird in der
        // Stream-Ansicht gebaut (genau EIN Session-Start, siehe unten).
        let check = {
            let settings = shell
                .backend
                .settings()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            crate::backend::psn::build_psn_connect_request(&settings, &duid, entry.ps5)
        };
        match check {
            Ok(_) => {
                // Vorab-Check bestanden (Token + Account-ID): nur navigieren —
                // den Session-Start übernimmt die Stream-Ansicht (resolve_request
                // baut dort den Request mit der Holepunch-Session; genau EIN
                // Start, sonst würde die PSN-Session auf dem Server doppelt
                // angelegt). Geräteliste parallel still auffrischen.
                let settings = shell.backend.settings().clone();
                shell
                    .backend
                    .psn()
                    .list_devices(settings, shell.backend.event_sender(), true);
                shell.push_toast(
                    toast(
                        ToastKind::Info,
                        "PSN-Verbindung …",
                        format!(
                            "\u{201e}{}\u{201c} ({}) wird über PSN verbunden …",
                            entry.name,
                            if entry.ps5 { "PS5" } else { "PS4" }
                        ),
                    ),
                    cx,
                );
                shell.navigate(Route::Stream(HostId::Psn { duid }), cx);
            }
            Err(err) => {
                // Erwarteter Smoke-Fall: keine PSN-Anmeldung/Account-ID —
                // saubere Meldung, kein Navigate, kein Session-Start.
                shell.push_toast(toast(ToastKind::Warn, "PSN-Verbindung nicht möglich", err), cx);
            }
        }
        return;
    }
    if entry.registered.is_none() {
        shell.push_toast(
            toast(
                ToastKind::Warn,
                "Nicht registriert",
                "Diese Konsole ist nicht registriert — starte den Registrierungs-Wizard.",
            ),
            cx,
        );
        return;
    }
    if entry.addr.is_empty() {
        shell.push_toast(
            toast(ToastKind::Warn, "Keine Adresse", "Für diese Konsole ist keine IP bekannt."),
            cx,
        );
        return;
    }
    // Standby: erst aufwecken (wie das C++ im Session-Start; unser Session-
    // Layer hat den Wake noch nicht integriert — deshalb hier explizit).
    if entry.status == StatusKind::Standby {
        wake_entry(shell, entry, cx);
        return;
    }
    // Virtual-Cam-Popup (settings/virtualcam_enabled): statt direkt zu
    // verbinden fragt ein Dialog, wie gestartet werden soll — normaler
    // Stream im Fenster oder fensterloser Headless-Feed in die virtuelle
    // Kamera (User-Vorgabe HANDOFF §8).
    let vcam_popup = {
        let settings = shell
            .backend
            .settings()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        settings.virtualcam_enabled()
    };
    if vcam_popup {
        open_vcam_choice(shell, entry, cx);
        return;
    }
    connect_normal(shell, entry, cx);
}

/// Normaler LAN-Stream (Fenster-Modus): NUR navigieren — den Session-Start
/// besitzt die Stream-Ansicht allein (resolve_request + connect_started-
/// Guard). Ein connect() hier würde (a) den UI-Thread blockieren und (b)
/// einen ZWEITEN Start neben dem der Stream-Ansicht erzeugen (zwei
/// Sessions konkurrieren um die Konsole).
fn connect_normal(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let host_id = match (entry.registered.as_ref(), entry.manual.as_ref(), entry.duid.as_ref()) {
        (Some(registered), _, _) => HostId::Registered { mac: *registered.server_mac.mac() },
        (_, Some(manual), _) => HostId::Manual { id: manual.id },
        (_, _, Some(duid)) => HostId::Psn { duid: duid.clone() },
        _ => {
            shell.push_toast(
                toast(ToastKind::Warn, "Nicht verbunden", "Konsole ist nicht registriert."),
                cx,
            );
            return;
        }
    };
    shell.push_toast(
        toast(ToastKind::Info, "Verbinde …", format!("\u{201e}{}\u{201c} ({})", entry.name, entry.addr)),
        cx,
    );
    shell.navigate(Route::Stream(host_id), cx);
}

/// Virtual-Cam-Auswahl (settings/virtualcam_enabled an + Kachel-Klick):
/// Dialog „Normal streamen / Headless-Feed starten (/ stoppen)“. Der
/// Headless-Feed läuft als detachierter Prozess weiter, das Chiaki-Fenster
/// bleibt offen und kann ihn über diesen Dialog bzw. die Settings stoppen.
fn open_vcam_choice(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let weak = cx.entity().downgrade();
    let running = chiaki_virtualcam::is_running();
    let body = if running {
        format!(
            "Der fensterlose Kamera-Feed läuft bereits. „{}“ zusätzlich normal streamen oder den Feed stoppen?",
            entry.name
        )
    } else {
        format!(
            "Wie soll „{}“ gestartet werden? Der fensterlose Feed spielt in die virtuelle Kamera (mit VSR, wenn aktiv) und der Ton bleibt lokal — ohne sichtbares Chiaki-Fenster.",
            entry.name
        )
    };
    let entry_normal = entry.clone();
    let mut dialog = Dialog::new("vcam-choice", "Virtuelle Kamera", body).button(
        DialogButton::new("Normal streamen", ButtonVariant::Primary).action(move |_window, cx| {
            let _ = weak.update(cx, |shell, cx| connect_normal(shell, &entry_normal, cx));
        }),
    );
    let weak_stop = cx.entity().downgrade();
    let entry_start = entry.clone();
    if running {
        dialog = dialog.button(
            DialogButton::new("Headless-Feed stoppen", ButtonVariant::Danger).action(
                move |_window, cx| {
                    let _ = weak_stop.update(cx, |shell, cx| {
                        if crate::backend::vcam::stop_headless() {
                            shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Info,
                                    "Headless-Feed",
                                )
                                .message("Stop-Signal gesendet — die Session wird sauber beendet"),
                                cx,
                            );
                        } else {
                            shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Warn,
                                    "Headless-Feed",
                                )
                                .message("Läuft nicht mehr"),
                                cx,
                            );
                        }
                    });
                },
            ),
        );
    } else {
        dialog = dialog.button(
            DialogButton::new("Headless starten", ButtonVariant::Ghost).action(
                move |_window, cx| {
                    let _ = weak_stop.update(cx, |shell, cx| {
                        match crate::backend::vcam::start_headless_now_with_addr(
                            &shell.backend,
                            &entry_start.addr,
                        ) {
                            Ok(pid) => shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Success,
                                    "Headless-Feed gestartet",
                                )
                                .message(format!(
                                    "Fensterlose Session läuft (PID {pid}) — Kamera in OBS/Discord prüfen"
                                )),
                                cx,
                            ),
                            Err(err) => shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Warn,
                                    "Headless-Feed",
                                )
                                .message(err),
                                cx,
                            ),
                        }
                    });
                },
            ),
        );
    }
    let dialog = dialog.button(DialogButton::new("Abbrechen", ButtonVariant::Ghost));
    shell.push_dialog(dialog, cx);
}

/// Kontextmenü einer Kachel (Spec: Aufwachen, Verstecken, Registrierung
/// löschen) — als Dialog über den bestehenden ModalLayer (gleiche Elemente
/// wie `ContextMenuLayer` im QML: Connect/Wake/Hide/Delete).
pub(crate) fn open_host_menu(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let weak = cx.entity().downgrade();

    let connect_entry_clone = entry.clone();
    let mut dialog = Dialog::new("host-menu", entry.name.clone(), "Aktion auswählen").button(
        DialogButton::new("Verbinden", ButtonVariant::Ghost).action(move |_window, cx| {
            let _ = weak.update(cx, |shell, cx| connect_entry(shell, &connect_entry_clone, cx));
        }),
    );

    if entry.registered.is_some() && !entry.addr.is_empty() {
        let weak = cx.entity().downgrade();
        let wake_entry_clone = entry.clone();
        dialog = dialog.button(
            DialogButton::new("Aufwecken", ButtonVariant::Ghost).action(move |_window, cx| {
                let _ = weak.update(cx, |shell, cx| wake_entry(shell, &wake_entry_clone, cx));
            }),
        );
    }

    // Verstecken (settings/hidden_hosts) — wie im C++ nur sinnvoll für
    // nicht registrierte Discovery-Hosts (registrierte würden sofort wieder
    // auftauchen).
    if let Some(mac) = entry.mac {
        if entry.registered.is_none() {
            let weak = cx.entity().downgrade();
            let name = entry.name.clone();
            dialog = dialog.button(
                DialogButton::new("Verstecken", ButtonVariant::Ghost).action(move |_window, cx| {
                    let _ = weak.update(cx, |shell, cx| {
                        shell
                            .backend
                            .settings()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .update(|s| s.add_hidden_host(chiaki_settings::hosts::HiddenHost::new(
                                HostMac::new(mac),
                                name.clone(),
                            )))
                            .ok();
                        shell.push_toast(
                            toast(ToastKind::Info, "Konsole versteckt", name.clone()),
                            cx,
                        );
                    });
                }),
            );
        }
    }

    if entry.registered.is_some() {
        let weak = cx.entity().downgrade();
        let delete_entry = entry.clone();
        dialog = dialog.button(
            DialogButton::new("Registrierung löschen …", ButtonVariant::Danger).action(
                move |_window, cx| {
                    let _ = weak.update(cx, |shell, cx| {
                        open_delete_confirm(shell, &delete_entry, cx)
                    });
                },
            ),
        );
    }

    dialog = dialog.button(DialogButton::new("Abbrechen", ButtonVariant::Ghost));
    shell.push_dialog(dialog, cx);
}

/// Bestätigungsdialog „Registrierung löschen“ (C++: shell.confirm(...) →
/// settings.deleteRegisteredHost).
fn open_delete_confirm(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let weak = cx.entity().downgrade();
    let name = entry.name.clone();
    let mac = entry.mac;
    let dialog = Dialog::new(
        "delete-confirm",
        "Konsole löschen",
        format!("Registrierung von „{name}“ wirklich löschen?"),
    )
    .button(DialogButton::new("Abbrechen", ButtonVariant::Ghost))
    .button(
        DialogButton::new("Löschen", ButtonVariant::Danger).action(move |_window, cx| {
            let _ = weak.update(cx, |shell, cx| {
                if let Some(mac) = mac {
                    shell
                        .backend
                        .settings()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .update(|s| s.remove_registered_host(HostMac::new(mac)))
                        .ok();
                }
                shell.push_toast(
                    toast(ToastKind::Success, "Registrierung gelöscht", name.clone()),
                    cx,
                );
            });
        }),
    );
    shell.push_dialog(dialog, cx);
}

// ---------------------------------------------------------------------------
// Geteilte Kachel (240×140, Spec §2.1)
// ---------------------------------------------------------------------------

/// Konsolen-Kachel: Klick = Verbinden, Rechtsklick = Kontextmenü.
/// Fokus-Handle kommt aus dem stabilen Puffer ([`regist_wizard::WizardState`]).
pub(crate) fn console_tile(
    i: usize,
    entry: &ConsoleEntry,
    focus: FocusHandle,
    _shell: &mut AppShell,
    cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    let click_entry = entry.clone();
    let menu_entry = entry.clone();

    // Fixe Kachelgröße außen (Card wächst via flex_1 → füllt den Rahmen).
    div()
        .w(px(240.0))
        .h(px(140.0))
        .flex_none()
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |shell, _ev, _window, cx| open_host_menu(shell, &menu_entry, cx)),
        )
        .child(
            Card::new(("tile", i))
                .focus_handle(focus)
                .on_click(cx.listener(move |shell, _ev, _window, cx| {
                    connect_entry(shell, &click_entry, cx)
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(icons::icon(icons::paths::CONSOLE, 26.0, theme::ACCENT))
                        .child(StatusBadge::new(entry.status).label(entry.status_label)),
                )
                .child(
                    div()
                        // Feste 240-px-Kachel: lange Konsolennamen bekommen
                        // eine Ellipse statt eines hässlichen Wortumbruchs.
                        .truncate()
                        .text_size(px(theme::SIZE_HEADLINE))
                        .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme::SP_1))
                        .min_w_0()
                        .child(
                            div()
                                // gpui-0.2.2-Quirk: `text_ellipsis` ellipsiert in
                                // verschachtelten Flex-Spalten bereits bei kurzen
                                // Texten — deshalb hier nur Clip + Einzeilig.
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(theme::SIZE_CAPTION))
                                .text_color(theme::TEXT_SECONDARY)
                                .child(if entry.addr.is_empty() {
                                    "PSN remote".to_string()
                                } else {
                                    entry.addr.clone()
                                }),
                        )
                        .children(entry.running_app.clone().map(|app| {
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .min_w_0()
                                .child(icons::icon(icons::paths::PULSE, 12.0, theme::SUCCESS))
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_size(px(theme::SIZE_CAPTION))
                                        .text_color(theme::TEXT_PRIMARY)
                                        .child(app),
                                )
                        })),
                ),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Seite
// ---------------------------------------------------------------------------

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let entries = console_entries(shell);
    let focus_handles = shell.regist_wizard.sync_tile_focus(entries.len(), cx);
    let ui_focus = shell.regist_wizard.sync_ui_focus(4, cx);

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Home", Some("Willkommen zurück"), window, cx));

    if entries.is_empty() {
        // Leerzustand (Erststart): Willkommens-Fluss statt leerer Liste.
        children.push(
            Card::new("home-empty")
                .hero()
                .child(
                    EmptyState::new(
                        icons::paths::CONSOLE,
                        "Willkommen beim Chiaki Remaster",
                        "Registriere deine erste Konsole, um zu streamen.\n\
                         Konsole und dieser Rechner müssen im selben Netzwerk sein.",
                    )
                    .action(
                        Button::new("home-welcome-regist", "Konsole registrieren")
                            .variant(ButtonVariant::Primary)
                            .on_click(cx.listener(|shell, _ev, _window, cx| {
                                // Wizard ist Overlay der Konsolen-Seite
                                // (CONTRACT-UI §4) → dorthin navigieren.
                                regist_wizard::open(shell, cx);
                                shell.navigate(Route::Consoles, cx);
                            })),
                    ),
                )
                .into_any_element(),
        );
    } else {
        // Hero (nur ab 2 Konsolen — bei einer wäre es die Doppelanzeige der
        // Kachel, siehe Moduldoku): zuletzt verbunden = auto_connect_mac.
        if entries.len() > 1 {
            if let Some((hero_index, hero)) = hero_entry(shell, &entries) {
                children.push(
                    hero_card(hero_index, hero, ui_focus[0].clone(), cx).into_any_element(),
                );
            }
        }

        // Reihe „Deine Konsolen“.
        children.push(SectionLabel::new("Deine Konsolen").into_any_element());
        children.push(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(theme::SP_4))
                .children(entries.iter().enumerate().map(|(i, entry)| {
                    console_tile(i, entry, focus_handles[i].clone(), shell, cx)
                }))
                .into_any_element(),
        );

        // Schnellaktionen (schmale Zeile, Spec §2.1).
        children.push(SectionLabel::new("Schnellaktionen").into_any_element());
        children.push(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_2()
                .child(
                    Button::new("qa-psn", "PSN Remote aktivieren")
                        .focus_handle(ui_focus[1].clone())
                        .on_click(cx.listener(|shell, _ev, _window, _cx| {
                            // Gleicher PSN-Login-Flow wie Settings/Info
                            // (psn_login.rs): Tokens + Account-ID in den
                            // Settings, Feedback als Toast-UiEvent.
                            crate::psn_login::start_psn_login_for_settings(
                                shell.backend.settings().clone(),
                                shell.backend.event_sender(),
                            );
                        })),
                )
                .child(
                    Button::new("qa-regist", "Konsole registrieren")
                        .focus_handle(ui_focus[2].clone())
                        .on_click(cx.listener(|shell, _ev, _window, cx| {
                            // Wizard ist Overlay der Konsolen-Seite.
                            regist_wizard::open(shell, cx);
                            shell.navigate(Route::Consoles, cx);
                        })),
                )
                .child(
                    Button::new("qa-manual", "Manuellen Host hinzufügen")
                        .focus_handle(ui_focus[3].clone())
                        .on_click(cx.listener(|shell, _ev, _window, cx| {
                            shell.regist_wizard.manual_focus_pending = true;
                            shell.navigate(Route::Consoles, cx);
                        })),
                )
                .into_any_element(),
        );
    }

    // Status-/Tipp-Zeile (wie HomePage.qml-Footer).
    children.push(
        div()
            .flex()
            .items_center()
            .gap_2()
            .pt_1()
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_DISABLED)
                    .child("Enter — Verbinden · Rechtsklick — Konsolen-Aktionen"),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}

/// Hero-Host: MAC-Abgleich mit `auto_connect_mac` (Proxy für „zuletzt
/// verbunden“), sonst der erste Host — wie HomePage.qml.
fn hero_entry<'a>(
    shell: &AppShell,
    entries: &'a [ConsoleEntry],
) -> Option<(usize, &'a ConsoleEntry)> {
    let auto_mac = shell
        .backend
        .settings()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .auto_connect_host()
        .server_mac;
    let auto = *auto_mac.mac();
    let by_mac = entries
        .iter()
        .enumerate()
        .find(|(_, e)| e.mac == Some(auto) && e.registered.is_some());
    by_mac.or_else(|| entries.first().map(|e| (0usize, e)))
}

/// Hero-Karte (Spec §2.1): großer Name, Live-Status, „Verbinden“-Primary.
/// Der Live-Status kommt aus den Discovery-Events: HostFound/HostsChanged
/// notifyen die Shell (app.rs) → Kachel/Hero rendern den aktuellen Zustand.
fn hero_card(
    index: usize,
    entry: &ConsoleEntry,
    focus: FocusHandle,
    cx: &mut Context<AppShell>,
) -> Card {
    let click_entry = entry.clone();
    let context_line = format!(
        "{}  ·  {}",
        if entry.ps5 { "PlayStation 5" } else { "PlayStation 4" },
        if entry.addr.is_empty() { "PSN remote" } else { entry.addr.as_str() },
    );
    Card::new(("hero", index))
        .hero()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(theme::SP_5))
                .child(icons::icon(icons::paths::CONSOLE, 64.0, theme::ACCENT))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .flex_1()
                        .child(StatusBadge::new(entry.status).label(entry.status_label))
                        .child(
                            div()
                                // Einzeilig halten; gpui-0.2.2-Quirk: siehe
                                // console_tile (text_ellipsis ellipsiert in
                                // Flex-Spalten verfrüht) → nur Clip.
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(theme::SIZE_DISPLAY))
                                .font_weight(FontWeight(theme::WEIGHT_DISPLAY))
                                .text_color(theme::TEXT_PRIMARY)
                                .child(entry.name.clone()),
                        )
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(theme::SIZE_CAPTION))
                                .text_color(theme::TEXT_SECONDARY)
                                .child(context_line),
                        ),
                )
                .child(
                    Button::new("hero-connect", "Verbinden")
                        .variant(ButtonVariant::Primary)
                        .focus_handle(focus)
                        .on_click(cx.listener(move |shell, _ev, _window, cx| {
                            connect_entry(shell, &click_entry, cx)
                        })),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_aus_host_id_wie_cpp_fromhex() {
        // 12 Hex-Zeichen → 6 Bytes (Discovery-Standardfall).
        assert_eq!(
            mac_from_host_id("AABBCCDDEEFF"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
        // falsche Länge → None (GetHostMAC: zero-MAC-Abweisung).
        assert_eq!(mac_from_host_id("AABBCC"), None);
        assert_eq!(mac_from_host_id("C0FFEE1122334455"), None);
        // Nicht-Hex-Zeichen werden übersprungen (QByteArray::fromHex).
        assert_eq!(
            mac_from_host_id("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
    }
}
