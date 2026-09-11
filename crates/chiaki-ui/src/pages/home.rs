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
    ParentElement as _, StatefulInteractiveElement as _, Styled, StyledImage as _, Window,
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
use crate::pages::page_scaffold;
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
            chiaki_core::discovery::DiscoveryHostState::Ready => (StatusKind::Ready, "Ready"),
            chiaki_core::discovery::DiscoveryHostState::Standby => (StatusKind::Standby, "Standby"),
            chiaki_core::discovery::DiscoveryHostState::Unknown => {
                if registered.is_some() {
                    (StatusKind::Offline, "offline")
                } else {
                    (StatusKind::Unregistered, "Not registered")
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
            (StatusKind::Offline, "offline")
        } else {
            (StatusKind::Unregistered, "Not registered")
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
            (StatusKind::Ready, "Ready")
        } else {
            (StatusKind::Offline, "Remote Play off")
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
            toast(ToastKind::Warn, "Not registered", "Only registered consoles can be woken up."),
            cx,
        );
        return;
    };
    if entry.addr.is_empty() {
        shell.push_toast(toast(ToastKind::Warn, "No address", "No IP address is known for this console."), cx);
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
                "Wake sent",
                format!("Waking up \"{}\" — startup takes a moment.", entry.name),
            ),
            cx,
        ),
        Err(err) => shell.push_toast(
            toast(ToastKind::Danger, "Wake failed", err.to_string()),
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
                toast(ToastKind::Warn, "Not a PSN console", "This entry has no DUID."),
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
                        "PSN connection …",
                        format!(
                            "\"{}\" ({}) is connecting via PSN …",
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
                shell.push_toast(toast(ToastKind::Warn, "PSN connection not possible", err), cx);
            }
        }
        return;
    }
    if entry.registered.is_none() {
        shell.push_toast(
            toast(
                ToastKind::Warn,
                "Not registered",
                "This console is not registered — start the registration wizard.",
            ),
            cx,
        );
        return;
    }
    if entry.addr.is_empty() {
        shell.push_toast(
            toast(ToastKind::Warn, "No address", "No IP address is known for this console."),
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
                toast(ToastKind::Warn, "Not connected", "Console is not registered."),
                cx,
            );
            return;
        }
    };
    shell.push_toast(
        toast(ToastKind::Info, "Connecting …", format!("\"{}\" ({})", entry.name, entry.addr)),
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
            "The windowless camera feed is already running. Stream \"{}\" normally as well, or stop the feed?",
            entry.name
        )
    } else {
        format!(
            "How should \"{}\" be started? The windowless feed plays into the virtual camera (with VSR, if enabled) and audio stays local — without a visible Chiaki window.",
            entry.name
        )
    };
    let entry_normal = entry.clone();
    let mut dialog = Dialog::new("vcam-choice", "Virtual camera", body).button(
        DialogButton::new("Stream normally", ButtonVariant::Primary).action(move |_window, cx| {
            let _ = weak.update(cx, |shell, cx| connect_normal(shell, &entry_normal, cx));
        }),
    );
    let weak_stop = cx.entity().downgrade();
    let entry_start = entry.clone();
    if running {
        dialog = dialog.button(
            DialogButton::new("Stop headless feed", ButtonVariant::Danger).action(
                move |_window, cx| {
                    let _ = weak_stop.update(cx, |shell, cx| {
                        if crate::backend::vcam::stop_headless() {
                            shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Info,
                                    "Headless feed",
                                )
                                .message("Stop signal sent — the session will shut down cleanly"),
                                cx,
                            );
                        } else {
                            shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Warn,
                                    "Headless feed",
                                )
                                .message("No longer running"),
                                cx,
                            );
                        }
                    });
                },
            ),
        );
    } else {
        dialog = dialog.button(
            DialogButton::new("Start headless", ButtonVariant::Ghost).action(
                move |_window, cx| {
                    let _ = weak_stop.update(cx, |shell, cx| {
                        match crate::backend::vcam::start_headless_now_with_addr(
                            &shell.backend,
                            &entry_start.addr,
                        ) {
                            Ok(pid) => shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Success,
                                    "Headless feed started",
                                )
                                .message(format!(
                                    "Windowless session running (PID {pid}) — check the camera in OBS/Discord"
                                )),
                                cx,
                            ),
                            Err(err) => shell.push_toast(
                                crate::components::ToastData::new(
                                    crate::components::ToastKind::Warn,
                                    "Headless feed",
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
    let dialog = dialog.button(DialogButton::new("Cancel", ButtonVariant::Ghost));
    shell.push_dialog(dialog, cx);
}

/// Kontextmenü einer Kachel (Spec: Aufwachen, Verstecken, Registrierung
/// löschen) — als Dialog über den bestehenden ModalLayer (gleiche Elemente
/// wie `ContextMenuLayer` im QML: Connect/Wake/Hide/Delete).
pub(crate) fn open_host_menu(shell: &mut AppShell, entry: &ConsoleEntry, cx: &mut Context<AppShell>) {
    let weak = cx.entity().downgrade();

    let connect_entry_clone = entry.clone();
    let mut dialog = Dialog::new("host-menu", entry.name.clone(), "Choose an action").button(
        DialogButton::new("Connect", ButtonVariant::Ghost).action(move |_window, cx| {
            let _ = weak.update(cx, |shell, cx| connect_entry(shell, &connect_entry_clone, cx));
        }),
    );

    if entry.registered.is_some() && !entry.addr.is_empty() {
        let weak = cx.entity().downgrade();
        let wake_entry_clone = entry.clone();
        dialog = dialog.button(
            DialogButton::new("Wake", ButtonVariant::Ghost).action(move |_window, cx| {
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
                DialogButton::new("Hide", ButtonVariant::Ghost).action(move |_window, cx| {
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
                            toast(ToastKind::Info, "Console hidden", name.clone()),
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
            DialogButton::new("Delete", ButtonVariant::Danger).action(
                move |_window, cx| {
                    let _ = weak.update(cx, |shell, cx| {
                        open_delete_confirm(shell, &delete_entry, cx)
                    });
                },
            ),
        );
    }

    dialog = dialog.button(DialogButton::new("Cancel", ButtonVariant::Ghost));
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
        "Delete console",
        format!("Really delete the registration of \"{name}\"?"),
    )
    .button(DialogButton::new("Cancel", ButtonVariant::Ghost))
    .button(
        DialogButton::new("Delete", ButtonVariant::Danger).action(move |_window, cx| {
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
                    toast(ToastKind::Success, "Registration deleted", name.clone()),
                    cx,
                );
            });
        }),
    );
    shell.push_dialog(dialog, cx);
}

// ---------------------------------------------------------------------------
// Geteilte Kachel (ui-v3: Karte mit PS5-Render, 290×150)
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

    // Fixe Kachelgröße außen (Inhalt füllt den Rahmen über size_full).
    div()
        .w(px(290.0))
        .h(px(150.0))
        .flex_none()
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |shell, _ev, _window, cx| open_host_menu(shell, &menu_entry, cx)),
        )
        .child(
            div()
                .id(("tile", i))
                .flex()
                .items_center()
                .gap(px(theme::SP_4))
                .size_full()
                .p(px(theme::SP_4))
                .rounded(px(theme::RADIUS_LG))
                .bg(theme::SURFACE)
                .border_1()
                .border_color(theme::HAIRLINE)
                .cursor_pointer()
                .hover(|s| s.bg(theme::SURFACE2))
                .track_focus(&focus)
                .focus(|s| s.border_2().border_color(theme::ACCENT))
                .on_click(cx.listener(move |shell, _ev, _window, cx| {
                    connect_entry(shell, &click_entry, cx)
                }))
                // PS5-Render (Prototyp-Asset, identisch zur Hero-Karte).
                .child(
                    gpui::img(icons::image_paths::PS5)
                        .w(px(78.0))
                        .h(px(112.0))
                        .flex_none()
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div().flex().flex_row().child(
                                StatusBadge::new(entry.status).label(entry.status_label),
                            ),
                        )
                        .child(
                            div()
                                // gpui-0.2.2-Quirk: `text_ellipsis` ellipsiert in
                                // verschachtelten Flex-Spalten bereits bei kurzen
                                // Texten — deshalb hier nur Clip + Einzeilig.
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(px(theme::SIZE_HEADLINE))
                                .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                .text_color(theme::TEXT_PRIMARY)
                                .child(entry.name.clone()),
                        )
                        .child(
                            div()
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
    _window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    let query = shell.home_search.to_lowercase();
    let all_entries = console_entries(shell);
    let entries: Vec<ConsoleEntry> = if query.is_empty() {
        all_entries
    } else {
        all_entries
            .into_iter()
            .filter(|e| {
                e.name.to_lowercase().contains(&query) || e.addr.to_lowercase().contains(&query)
            })
            .collect()
    };
    let focus_handles = shell.regist_wizard.sync_tile_focus(entries.len().max(1), cx);
    let ui_focus = shell.regist_wizard.sync_ui_focus(4, cx);

    let mut children: Vec<gpui::AnyElement> = Vec::new();

    // Topbar-Zeile: Suche rechts (ui-v3-Prototyp).
    children.push(
        div()
            .flex()
            .justify_end()
            .child(
                crate::components::TextField::new("home-search")
                    .placeholder("Search consoles…")
                    .value(shell.home_search.clone())
                    .width(280.0)
                    .focus_handle(shell.home_search_focus.clone())
                    .on_change(cx.listener(|shell, value: &gpui::SharedString, _w, cx| {
                        shell.home_search = value.to_string();
                        cx.notify();
                    })),
            )
            .into_any_element(),
    );

    // Hero-Header: große Typo + Untertitel.
    children.push(
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_size(px(theme::SIZE_HERO))
                    .font_weight(FontWeight(theme::WEIGHT_DISPLAY))
                    .text_color(theme::TEXT_PRIMARY)
                    .child("Home"),
            )
            .child(
                div()
                    .text_size(px(theme::SIZE_BODY))
                    .text_color(theme::TEXT_SECONDARY)
                    .child("Welcome back! Your games are closer than you think."),
            )
            .into_any_element(),
    );

    if entries.is_empty() {
        // Leerzustand: entweder Erststart (keine Konsolen) oder Suchfilter
        // ohne Treffer.
        if query.is_empty() {
            children.push(
                Card::new("home-empty")
                    .hero()
                    .child(
                        EmptyState::new(
                            icons::paths::CONSOLE,
                            "Welcome to Chiaki Remaster",
                            "Register your first console to start streaming.\n\
                             The console and this computer must be on the same network.",
                        )
                        .action(
                            Button::new("home-welcome-regist", "Register console")
                                .variant(ButtonVariant::Primary)
                                .on_click(cx.listener(|shell, _ev, _window, cx| {
                                    regist_wizard::open(shell, cx);
                                    shell.navigate(Route::Consoles, cx);
                                })),
                        ),
                    )
                    .into_any_element(),
            );
        } else {
            children.push(
                Card::new("home-search-empty")
                    .child(
                        EmptyState::new(
                            icons::paths::SEARCH,
                            "No matches",
                            format!("No console matches \"{query}\"."),
                        ),
                    )
                    .into_any_element(),
            );
        }
    } else {
        // Hero (ui-v3): die erste Konsole als große Karte mit Render.
        children.push(
            hero_card(0, &entries[0], ui_focus[0].clone(), cx).into_any_element(),
        );

        // Restliche Konsolen als Kacheln (entfällt, wenn nur der Hero da ist).
        if entries.len() > 1 {
            children.push(SectionLabel::new("Your consoles").into_any_element());
            children.push(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(theme::SP_4))
                    .children(entries.iter().enumerate().skip(1).map(|(i, entry)| {
                        console_tile(i, entry, focus_handles[i].clone(), shell, cx)
                    }))
                    .into_any_element(),
            );
        }

        // Schnellaktionen (ui-v3: Karten mit Icon-Tile + Chevron).
        children.push(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(icons::icon(icons::paths::PULSE, 16.0, theme::ACCENT_SOFT))
                        .child(
                            div()
                                .text_size(px(theme::SIZE_TITLE))
                                .font_weight(FontWeight(theme::WEIGHT_TITLE))
                                .text_color(theme::TEXT_PRIMARY)
                                .child("Quick Actions"),
                        ),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(theme::TEXT_SECONDARY)
                        .child("Get started or manage your remote play setup."),
                )
                .into_any_element(),
        );
        children.push(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(theme::SP_4))
                .child(
                    action_card(
                        "qa-psn",
                        icons::paths::LINK,
                        "Set up PSN Remote Play",
                        "Guide to enable remote play\non your PS5.",
                        true,
                        ui_focus[1].clone(),
                        cx.listener(|shell, _ev, _window, _cx| {
                            // Gleicher PSN-Login-Flow wie Settings/Info
                            // (psn_login.rs): Tokens + Account-ID in den
                            // Settings, Feedback als Toast-UiEvent.
                            crate::psn_login::start_psn_login_for_settings(
                                shell.backend.settings().clone(),
                                shell.backend.event_sender(),
                            );
                        }),
                    )
                    .into_any_element(),
                )
                .child(
                    action_card(
                        "qa-regist",
                        icons::paths::USER_PLUS,
                        "Register Console",
                        "Add your PS5 with an\nactivation code.",
                        false,
                        ui_focus[2].clone(),
                        cx.listener(|shell, _ev, _window, cx| {
                            regist_wizard::open(shell, cx);
                            shell.navigate(Route::Consoles, cx);
                        }),
                    )
                    .into_any_element(),
                )
                .child(
                    action_card(
                        "qa-manual",
                        icons::paths::NETWORK,
                        "Add Manual Host",
                        "Connect directly using\nIP address.",
                        false,
                        ui_focus[3].clone(),
                        cx.listener(|shell, _ev, _window, cx| {
                            shell.regist_wizard.manual_focus_pending = true;
                            shell.navigate(Route::Consoles, cx);
                        }),
                    )
                    .into_any_element(),
                )
                .into_any_element(),
        );

        // Recent Sessions + Tips (ui-v3-Zweierreihe).
        children.push(sessions_and_tips_row().into_any_element());
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
                    .child("Enter — Connect · Right-click — Console actions"),
            )
            .into_any_element(),
    );

    page_scaffold(children)
}

/// Aktions-Karte (ui-v3): Icon-Tile, Titel, Beschreibung, Chevron rechts;
/// `primary` bekommt den Akzent-Rahmen. Der Klick-Handler kommt als
/// gpui-Rohhandler (typischerweise `cx.listener(...)`).
#[allow(clippy::too_many_arguments)]
fn action_card(
    id: &'static str,
    icon: &'static str,
    title: &str,
    desc: &str,
    primary: bool,
    focus: FocusHandle,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(theme::SP_4))
        .w(px(300.0))
        .p(px(theme::SP_4))
        .rounded(px(theme::RADIUS_LG))
        .bg(theme::SURFACE)
        .border_1()
        .border_color(if primary { theme::ACCENT } else { theme::HAIRLINE })
        .cursor_pointer()
        .hover(|s| s.bg(theme::SURFACE2))
        .track_focus(&focus)
        .on_click(on_click)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .size(px(44.0))
                .rounded(px(theme::RADIUS_MD))
                .bg(theme::SURFACE2)
                .border_1()
                .border_color(theme::HAIRLINE)
                .child(icons::icon(icon, 20.0, theme::ACCENT_SOFT)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(theme::SIZE_HEADLINE))
                        .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(desc.to_string()),
                ),
        )
        .child(icons::icon(icons::paths::CHEVRON_RIGHT, 16.0, theme::TEXT_SECONDARY))
        .into_any_element()
}

/// Zweierreihe „Recent Sessions“ + „Tips & Guides“ (ui-v3).
fn sessions_and_tips_row() -> gpui::AnyElement {
    div()
        .flex()
        .flex_row()
        .gap(px(theme::SP_4))
        // Recent Sessions: leerer Zustand mit gestrichelter Andeutung —
        // eine Sitzungshistorie existiert (noch) nicht als Feature.
        .child(
            div().flex_1().min_w_0().child(
                Card::new("recent-sessions")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icons::icon(icons::paths::CLOCK, 16.0, theme::ACCENT_SOFT))
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_HEADLINE))
                                    .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                    .text_color(theme::TEXT_PRIMARY)
                                    .child("Recent Sessions"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .w_full()
                            .py(px(theme::SP_6))
                            .rounded(px(theme::RADIUS_MD))
                            .border_1()
                            .border_color(theme::HAIRLINE)
                            .child(icons::icon(
                                icons::paths::CONSOLE,
                                28.0,
                                theme::TEXT_DISABLED,
                            ))
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_HEADLINE))
                                    .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                    .text_color(theme::TEXT_PRIMARY)
                                    .child("No recent sessions"),
                            )
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_CAPTION))
                                    .text_color(theme::TEXT_SECONDARY)
                                    .child(
                                        "Your play history will appear here once you connect.",
                                    ),
                            ),
                    ),
            ),
        )
        // Tips & Guides: statischer Einsteiger-Tipp mit Swoosh-Thumb.
        .child(
            div().flex_1().min_w_0().child(
                Card::new("tips-guides")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(icons::icon(
                                icons::paths::LIGHTBULB,
                                16.0,
                                theme::ACCENT_SOFT,
                            ))
                            .child(
                                div()
                                    .text_size(px(theme::SIZE_HEADLINE))
                                    .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                    .text_color(theme::TEXT_PRIMARY)
                                    .child("Tips & Guides"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(theme::SP_3))
                            .child(
                                gpui::img(icons::image_paths::BG_SWOOSH)
                                    .w(px(96.0))
                                    .h(px(56.0))
                                    .rounded(px(theme::RADIUS_MD))
                                    .object_fit(gpui::ObjectFit::Cover),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_size(px(theme::SIZE_BODY))
                                            .font_weight(FontWeight(theme::WEIGHT_HEADLINE))
                                            .text_color(theme::TEXT_PRIMARY)
                                            .child("Get the best experience"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(theme::SIZE_CAPTION))
                                            .text_color(theme::TEXT_SECONDARY)
                                            .child(
                                                "Use a wired connection or 5 GHz Wi-Fi for \
                                                 lower latency and smoother gameplay.",
                                            ),
                                    ),
                            ),
                    ),
            ),
        )
        .into_any_element()
}

/// Hero-Karte (ui-v3): PS5-Render links, Name/Status/Metadaten in der
/// Mitte, Connect + Kebab-Menü rechts mit Zitat. Rechtsklick öffnet wie
/// bei den Kacheln das Kontextmenü. Hero = erste Konsole (Discovery-
/// Reihenfolge); der auto_connect-Proxy des alten Designs entfällt.
fn hero_card(
    index: usize,
    entry: &ConsoleEntry,
    focus: FocusHandle,
    cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    let menu_entry_kebab = entry.clone();
    let menu_entry_right = entry.clone();
    let click_entry = entry.clone();

    let meta_row = |icon: &'static str, text: String| {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(icons::icon(icon, 14.0, theme::TEXT_SECONDARY))
            .child(
                div()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(text),
            )
    };

    div()
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |shell, _ev, _window, cx| {
                open_host_menu(shell, &menu_entry_right, cx)
            }),
        )
        .child(
            Card::new(("hero", index))
                .hero()
                .focus_handle(focus.clone())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(theme::SP_6))
                        // PS5-Render (Prototyp-Asset).
                        .child(
                            gpui::img(icons::image_paths::PS5)
                                .h(px(170.0))
                                .w(px(190.0))
                                .flex_none()
                                .object_fit(gpui::ObjectFit::Contain),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .flex_1()
                                .min_w_0()
                                .child(SectionLabel::new("Your console"))
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_size(px(theme::SIZE_DISPLAY))
                                        .font_weight(FontWeight(theme::WEIGHT_DISPLAY))
                                        .text_color(theme::TEXT_PRIMARY)
                                        .child(entry.name.clone()),
                                )
                                .child(
                                    div().flex().flex_row().child(
                                        StatusBadge::new(entry.status).label(entry.status_label),
                                    ),
                                )
                                .child(meta_row(
                                    icons::paths::MAP_PIN,
                                    if entry.addr.is_empty() {
                                        "PSN remote".to_string()
                                    } else {
                                        entry.addr.clone()
                                    },
                                ))
                                .child(meta_row(
                                    icons::paths::PULSE,
                                    entry
                                        .running_app
                                        .clone()
                                        .unwrap_or_else(|| match entry.status {
                                            StatusKind::Ready => "Good connection".to_string(),
                                            StatusKind::Standby => {
                                                "In standby — connect to wake it up".to_string()
                                            }
                                            StatusKind::Offline => {
                                                "Offline — not responding to discovery"
                                                    .to_string()
                                                }
                                            StatusKind::Unregistered => {
                                                "Not registered to this account".to_string()
                                            }
                                        }),
                                ))
                                .child(
                                    // Row-Wrapper: gpui 0.2.2 kennt kein align-self —
                                    // in einer Row behält der Button seine Inhaltsbreite.
                                    div().flex().flex_row().child(
                                        Button::new("hero-connect", "Connect")
                                            .variant(ButtonVariant::Primary)
                                            .focus_handle(focus)
                                            .on_click(cx.listener(move |shell, _ev, _window, cx| {
                                                connect_entry(shell, &click_entry, cx)
                                            })),
                                    ),
                                ),
                        )
                        .child(
                            // Rechte Spalte: Kebab-Menü oben, Zitat unten.
                            div()
                                .flex()
                                .flex_col()
                                .items_end()
                                .justify_between()
                                .h_full()
                                .child(
                                    div()
                                        .id("hero-menu")
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .size(px(32.0))
                                        .rounded(px(theme::RADIUS_MD))
                                        .border_1()
                                        .border_color(theme::HAIRLINE)
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme::SURFACE2))
                                        .on_click(cx.listener(
                                            move |shell, _ev, _window, cx| {
                                                open_host_menu(shell, &menu_entry_kebab, cx)
                                            },
                                        ))
                                        .child(icons::icon(
                                            icons::paths::MORE,
                                            16.0,
                                            theme::TEXT_SECONDARY,
                                        )),
                                )
                                .child(
                                    div()
                                        .w(px(150.0))
                                        .italic()
                                        .text_size(px(theme::SIZE_HEADLINE))
                                        .text_color(theme::TEXT_SECONDARY)
                                        .child("“Great games travel further.”"),
                                ),
                        ),
                ),
        )
        .into_any_element()
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
