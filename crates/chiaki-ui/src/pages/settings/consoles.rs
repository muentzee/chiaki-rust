//! Settings-Kategorie „Consoles“ (QML: `SettingsConsoles.qml`): registrierte
//! Konsolen (Auto-Connect-Schalter, Umbenennen, Registrierung löschen),
//! versteckte Konsolen (Wiederherstellen), manuelle Hosts (Hinzufügen mit
//! Host-IP + registrierter MAC, Entfernen).

use chiaki_settings::hosts::{HostMac, ManualHost};

use crate::app::AppShell;
use crate::theme;

use super::{action_row, custom_row, info_row, ui_select_row, Section, SettingsUiState, SRow};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    let streamer_mode;
    let auto_connect_mac: String;
    let registered: Vec<chiaki_settings::hosts::RegisteredHost>;
    let hidden: Vec<chiaki_settings::hosts::HiddenHost>;
    let manual: Vec<chiaki_settings::hosts::ManualHost>;
    {
        let settings = shell.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
        streamer_mode = settings.streamer_mode();
        auto_connect_mac = settings.auto_connect_host().server_mac.to_hex_string();
        registered = settings.registered_hosts().into_iter().cloned().collect();
        hidden = settings.hidden_hosts().into_iter().cloned().collect();
        manual = settings.manual_hosts().into_iter().cloned().collect();
    }
    // Seiten-Zustand (existiert — `page()` ruft `ensure_state` auf).
    let (renaming, rename_pending, rename_focus, manual_ip, manual_mac, manual_focus) = {
        let state = cx.global_mut::<SettingsUiState>();
        (
            state.renaming.clone(),
            state.rename_pending.clone(),
            state.rename_focus.clone(),
            state.manual_ip.clone(),
            state.manual_mac.clone(),
            state.manual_focus.clone(),
        )
    };

    let mut registered_section = Section::new("Registered Consoles");
    for host in &registered {
        let mac_hex = host.server_mac.to_hex_string();
        let mac = host.server_mac;
        let is_ps5 = host.target.is_ps5();
        let name = host.server_nickname.clone();

        // MAC im Streamer mode verstecken (wie QML).
        let sub = if streamer_mode {
            format!("hidden \u{00B7} {}", if is_ps5 { "PS5" } else { "PS4" })
        } else {
            format!("{mac_hex} \u{00B7} {}", if is_ps5 { "PS5" } else { "PS4" })
        };

        if renaming.as_deref() == Some(mac_hex.as_str()) {
            registered_section.push(custom_row(
                &format!("{name} rename"),
                Some(&sub),
                "registered console rename",
                true,
                rename_row(name, rename_pending.clone(), rename_focus.clone(), mac, mac_hex),
            ));
            continue;
        }

        let is_auto_connect = auto_connect_mac == mac_hex;
        registered_section.push(registered_host_row(name, sub, mac_hex, is_auto_connect, mac));
    }
    if registered.is_empty() {
        registered_section.push(info_row(
            "No consoles registered yet. Use \u{201C}Register a console\u{201D} below to add one.",
            "register empty none",
        ));
    }
    registered_section.push(action_row(
        "consoles-register",
        "Register a console",
        Some("Launches the registration assistant (broadcast or manual address)"),
        "register new console pair",
        true,
        "Register\u{2026}",
        false,
        |shell, cx| {
            shell.show_regist_wizard = true;
            cx.notify();
        },
    ));

    let mut hidden_section = Section::new("Hidden Consoles");
    for host in &hidden {
        let mac_hex = host.server_mac.to_hex_string();
        let mac = host.server_mac;
        let (name_line, search) = if streamer_mode {
            ("hidden".to_string(), format!("hidden {}", host.server_nickname))
        } else {
            (
                format!("{} \u{00B7} {}", host.server_nickname, mac_hex),
                format!("{} {mac_hex}", host.server_nickname),
            )
        };
        let element = unhide_row(name_line.clone(), mac);
        hidden_section.push(custom_row(
            &search,
            Some(&name_line),
            "hidden console unhide",
            true,
            element,
        ));
    }
    if hidden.is_empty() {
        hidden_section.push(info_row(
            "No hidden consoles. Hidden consoles are excluded from discovery; hide them from \
             the tile context menu on the Consoles page.",
            "hidden empty none",
        ));
    }

    let mut manual_section = Section::new("Manual Hosts");
    for host in &manual {
        let mac_hex = host.registered_mac.to_hex_string();
        let id = host.id;
        let tail = if streamer_mode {
            "(hidden)".to_string()
        } else if host.registered {
            format!("registered ({mac_hex})")
        } else {
            "unregistered".to_string()
        };
        let text = format!("{} \u{00B7} {}", host.host, tail);
        let element = manual_host_row(text.clone(), id);
        manual_section.push(custom_row(
            &format!("manual host {} {mac_hex}", host.host),
            Some(&text),
            "manual host remove",
            true,
            element,
        ));
    }
    if manual.is_empty() {
        manual_section.push(info_row(
            "No manual hosts. A manual host pairs an address with a registered console.",
            "manual host empty none",
        ));
    }

    // Hinzufügen: Host-IP + registrierte MAC (Select aus der Registry).
    let registered_pairs: Vec<(String, String)> = registered
        .iter()
        .map(|h| (h.server_mac.to_hex_string(), h.server_nickname.clone()))
        .collect();
    manual_section.push(custom_row(
        "add manual host address ip mac",
        Some("Host address + registered console"),
        "manual host add",
        true,
        add_manual_host_row(manual_ip, manual_mac, manual_focus, &registered_pairs),
    ));

    vec![registered_section, hidden_section, manual_section]
}

// ---------------------------------------------------------------------------
// Individuelle Host-Rows
// ---------------------------------------------------------------------------

fn registered_host_row(
    name: String,
    sub: String,
    mac_hex: String,
    auto_connect: bool,
    mac: HostMac,
) -> SRow {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::{Button, ButtonVariant, Toggle};

    let mac_delete = mac;

    let left = div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(
            div()
                .text_size(px(theme::SIZE_BODY))
                .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                .text_color(theme::TEXT_PRIMARY)
                .child(name.clone()),
        )
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_DISABLED)
                .child(sub.clone()),
        );

    let controls = div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_CAPTION))
                        .text_color(theme::TEXT_SECONDARY)
                        .child("AUTO-CONNECT"),
                )
                .child(
                    Toggle::new(
                        gpui::ElementId::Name(format!("host-auto-{mac_hex}").into()),
                        auto_connect,
                    )
                    .on_change(move |value, _w, cx| {
                        super::commit_setting(cx, move |s| {
                            if value {
                                s.set_auto_connect_host(mac.mac());
                            }
                            // Abwählen: der Auto-Connect-Eintrag bleibt wie im
                            // C++ bestehen (nur eine Konsole markierbar —
                            // Umschalten überschreibt).
                        });
                    }),
                ),
        )
        .child(
            Button::new(
                gpui::ElementId::Name(format!("host-rename-{mac_hex}").into()),
                "Rename",
            )
            .variant(ButtonVariant::Ghost)
            .on_click(move |_: &gpui::ClickEvent, _w, cx| {
                let mac = mac.to_hex_string();
                super::mutate_state(cx, move |s| {
                    s.renaming = Some(mac);
                    s.rename_pending = String::new();
                });
            }),
        )
        .child(
            Button::new(
                gpui::ElementId::Name(format!("host-delete-{mac_hex}").into()),
                "Delete",
            )
            .variant(ButtonVariant::Danger)
            .on_click(move |_: &gpui::ClickEvent, _w, cx| {
                super::commit_setting(cx, move |s| {
                    s.remove_registered_host(mac_delete);
                });
            }),
        );

    custom_row(
        &format!("{name} {sub}"),
        None,
        "registered console auto-connect",
        true,
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme::SP_4))
            .child(left)
            .child(controls)
            .into_any_element(),
    )
}

fn rename_row(
    name: String,
    pending: String,
    focus: gpui::FocusHandle,
    mac: HostMac,
    mac_hex: String,
) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::{Button, TextField};

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(format!("Rename \u{201C}{name}\u{201D} (current key stays, only the display name changes)")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    TextField::new(
                        gpui::ElementId::Name(format!("host-rename-field-{mac_hex}").into()),
                    )
                    .value(pending.clone())
                    .placeholder(name)
                    .width(260.0)
                    .focus_handle(focus)
                    .on_change(move |v: &gpui::SharedString, _w, cx| {
                        let v: String = (&**v).to_string();
                        super::mutate_state(cx, move |s| s.rename_pending = v);
                    }),
                )
                .child(
                    Button::new(
                        gpui::ElementId::Name(format!("host-rename-save-{mac_hex}").into()),
                        "Save",
                    )
                    .variant(crate::components::ButtonVariant::Primary)
                    .on_click(move |_: &gpui::ClickEvent, _w, cx| {
                        super::commit_setting_and_state(cx, move |s, state| {
                            let new_name = state.rename_pending.trim().to_string();
                            if let Some(host) = s.registered_host(mac).cloned() {
                                let mut renamed = host;
                                renamed.server_nickname = new_name;
                                s.add_registered_host(renamed);
                            }
                            state.renaming = None;
                            state.rename_pending = String::new();
                        });
                    }),
                )
                .child(
                    Button::new(
                        gpui::ElementId::Name(format!("host-rename-cancel-{mac_hex}").into()),
                        "Cancel",
                    )
                    .variant(crate::components::ButtonVariant::Ghost)
                    .on_click(move |_: &gpui::ClickEvent, _w, cx| {
                        super::mutate_state(cx, |s| {
                            s.renaming = None;
                            s.rename_pending = String::new();
                        });
                    }),
                ),
        )
        .into_any_element()
}

fn unhide_row(text: String, mac: HostMac) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::{Button, ButtonVariant};

    let left = div()
        .text_size(px(theme::SIZE_BODY))
        .text_color(theme::TEXT_SECONDARY)
        .child(text);
    let button = Button::new(
        gpui::ElementId::Name(format!("host-unhide-{}", mac.to_hex_string()).into()),
        "Unhide",
    )
    .variant(ButtonVariant::Primary)
    .on_click(move |_: &gpui::ClickEvent, _w, cx| {
        super::commit_setting(cx, move |s| {
            s.remove_hidden_host(mac);
        });
    });

    row_layout(left.into_any_element(), button.into_any_element())
}

fn manual_host_row(text: String, id: i32) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::{Button, ButtonVariant};

    let left = div()
        .text_size(px(theme::SIZE_BODY))
        .text_color(theme::TEXT_SECONDARY)
        .child(text);
    let button = Button::new(gpui::ElementId::Name(format!("manual-remove-{id}").into()), "Remove")
        .variant(ButtonVariant::Danger)
        .on_click(move |_: &gpui::ClickEvent, _w, cx| {
            super::commit_setting(cx, move |s| {
                s.remove_manual_host(id);
            });
        });

    row_layout(left.into_any_element(), button.into_any_element())
}

fn add_manual_host_row(
    ip: String,
    mac: String,
    focus: gpui::FocusHandle,
    registered: &[(String, String)],
) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    use crate::components::{Button, TextField};

    let options: Vec<crate::components::SelectOption> = registered
        .iter()
        .map(|(m, n)| crate::components::SelectOption::new(m.clone(), n.clone()))
        .collect();

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child("Add manual host: address + registered console"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    TextField::new("manual-host-ip")
                        .value(ip)
                        .placeholder("192.168.1.50")
                        .width(200.0)
                        .focus_handle(focus)
                        .on_change(|v: &gpui::SharedString, _w, cx| {
                            let v: String = (&**v).to_string();
                            super::mutate_state(cx, move |s| s.manual_ip = v);
                        }),
                )
                .child(
                    ui_select_row(
                        "manual-host-mac-select",
                        "Registered console",
                        None,
                        "manual host mac registered console",
                        true,
                        options,
                        mac.clone(),
                        |v, state| state.manual_mac = v.to_string(),
                    )
                    .element,
                )
                .child(
                    Button::new("manual-host-add", "Add")
                        .variant(crate::components::ButtonVariant::Primary)
                        .on_click(|_: &gpui::ClickEvent, _w, cx| {
                            super::commit_setting_and_state(cx, |s, state| {
                                let host = state.manual_ip.trim().to_string();
                                if host.is_empty() {
                                    return;
                                }
                                let registered_mac =
                                    mac_from_hex(&state.manual_mac).unwrap_or_default();
                                s.set_manual_host(ManualHost {
                                    id: -1,
                                    host,
                                    registered: true,
                                    registered_mac,
                                });
                                state.manual_ip = String::new();
                            });
                        }),
                ),
        )
        .into_any_element()
}

fn mac_from_hex(hex: &str) -> Option<HostMac> {
    if hex.len() != 12 {
        return None;
    }
    let bytes: Vec<u8> = (0..6)
        .filter_map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok())
        .collect();
    HostMac::from_slice(&bytes)
}

/// Label links, Control rechts (für die custom Rows dieser Datei).
fn row_layout(left: gpui::AnyElement, control: gpui::AnyElement) -> gpui::AnyElement {
    use gpui::{div, px, IntoElement as _, ParentElement as _, Styled as _};
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(theme::SP_4))
        .child(left)
        .child(control)
        .into_any_element()
}
