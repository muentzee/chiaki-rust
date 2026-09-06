//! Einstellungen (ui-v2-spec §2.3): zweispaltig — Kategorie-Rail links,
//! SettingsRows rechts (Label links, Control rechts, Erklärzeile darunter),
//! Suchfeld oben filtert live über alle Kategorien (Label + Erklärtext +
//! Tags, case-insensitive, Treffer-Zählung, leere Sektionen kollabieren).
//! Jede Row wirkt sofort (`Settings::update` = ändern + speichern) — kein
//! Speichern-Button, kein Toast pro Änderung (wie im C++).
//!
//! **Struktur-Entscheidung:** `settings.rs` bleibt das Modul-Stammverzeichnis
//! (`page()`, Seiten-Zustand als gpui-`Global`, Row-Modelle/-Builder, Suche)
//! und deklariert die Kategorien als Unterdateien `settings/{general,video,
//! stream,audio,controls,consoles,psn,system}.rs` — Rust-2018-Style
//! (`settings.rs` + `settings/`-Verzeichnis), damit `pages::settings::page`
//! unverändert bindend bleibt (CONTRACT-UI §5).
//!
//! **Seiten-Zustand** (aktive Kategorie, Suchtext, offenes Select-Popup,
//! Key-Capture …) lebt in [`SettingsUiState`] als gpui-`Global` — app.rs
//! (AppShell-Felder) bleibt unangetastet; Re-Renders laufen über das dort
//! gespeicherte `WeakEntity<AppShell>`.
//!
//! Label-/Erklärtexte 1:1 aus der QML-v2-Referenz
//! (`chiaki-rust-remaster/gui/src/qml2/pages/settings/*.qml`, für die
//! Display-/Placebo-Feintuning-Rows die v1-Dialoge `DisplaySettingsDialog.qml`
//! / `PlaceboSettingsDialog.qml`).

mod audio;
mod consoles;
mod controls;
mod general;
mod psn;
mod stream;
mod system;
mod video;

use gpui::{
    div, px, App, Context, ElementId, FocusHandle, Global, IntoElement, InteractiveElement as _,
    KeyDownEvent, ParentElement as _, StatefulInteractiveElement as _, Styled, WeakEntity, Window,
};

use crate::app::AppShell;
use crate::components::{
    Button, ButtonVariant, Card, EmptyState, SectionLabel, Select, SelectOption, Slider, TextField,
    ToastData, ToastKind, Toggle,
};
use crate::icons;
use crate::pages::{page_header, page_scaffold};
use crate::theme;

/// Kategorien (Reihenfolge wie QML-v2-SettingsPage, Spec §2.3).
pub const CATEGORIES: [&str; 8] = [
    "General",
    "Video",
    "Stream",
    "Audio & Latency",
    "Controls",
    "Consoles",
    "PSN & Network",
    "System",
];

// ---------------------------------------------------------------------------
// Seiten-Zustand (gpui-Global; app.rs bleibt unangetastet)
// ---------------------------------------------------------------------------

/// UI-Zustand der Settings-Seite (siehe Modul-Doku).
pub struct SettingsUiState {
    /// Shell-Handle für Re-Render-Notifies aus Komponenten-Callbacks.
    shell: WeakEntity<AppShell>,
    /// Aktive Kategorie (Index in [`CATEGORIES`]).
    pub category: usize,
    /// Suchtext (Live-Filter über alle Kategorien).
    pub search: String,
    /// Fokus-Handle des Suchfelds.
    pub search_focus: FocusHandle,
    /// ID des aktuell offenen Select-Popups (nur eines offen).
    pub open_select: Option<String>,
    /// Key-Capture: Controller-Button-Bit, das gerade belegt wird.
    pub key_capture: Option<u32>,
    /// Fokus-Handle des Capture-Overlays.
    pub capture_focus: FocusHandle,
    /// Stream-Kategorie: Settings für PS4 (false) oder PS5 (true).
    pub stream_target_ps5: bool,
    /// Video/Rendering-Fine-Tuning: „Erweitert“-Sektionen ausgeklappt.
    pub advanced_open: bool,
    /// Manueller Host (Consoles): Eingaben.
    pub manual_ip: String,
    pub manual_mac: String,
    pub manual_focus: FocusHandle,
    /// Registrierten Host umbenennen: MAC (hex) des Hosts + gepufferter Name.
    pub renaming: Option<String>,
    pub rename_pending: String,
    pub rename_focus: FocusHandle,
    /// Eigene Fokus-Handles der Text-Rows (keine geteilten Handles — sonst
    /// bekämen zwei Felder dieselben Tastenereignisse).
    pub custom_width_focus: FocusHandle,
    pub custom_height_focus: FocusHandle,
    pub vsr_path_focus: FocusHandle,
    /// Audio-Geräte (Laufzeit-Enumeration chiaki-media; pro Frame gecacht).
    pub audio_out_devices: Vec<String>,
    pub audio_in_devices: Vec<String>,
}

impl Global for SettingsUiState {}

impl SettingsUiState {
    fn new(shell: WeakEntity<AppShell>, cx: &mut App) -> Self {
        Self {
            shell,
            category: 0,
            search: String::new(),
            search_focus: cx.focus_handle(),
            open_select: None,
            key_capture: None,
            capture_focus: cx.focus_handle(),
            stream_target_ps5: true,
            advanced_open: false,
            manual_ip: String::new(),
            manual_mac: String::new(),
            manual_focus: cx.focus_handle(),
            renaming: None,
            rename_pending: String::new(),
            rename_focus: cx.focus_handle(),
            custom_width_focus: cx.focus_handle(),
            custom_height_focus: cx.focus_handle(),
            vsr_path_focus: cx.focus_handle(),
            audio_out_devices: Vec::new(),
            audio_in_devices: Vec::new(),
        }
    }
}

/// Stellt sicher, dass der Seiten-Zustand existiert (erstes `page()`).
fn ensure_state(cx: &mut Context<AppShell>) {
    if !cx.has_global::<SettingsUiState>() {
        let shell = cx.entity().downgrade();
        let state = SettingsUiState::new(shell, cx);
        cx.set_global(state);
    }
}

// ---------------------------------------------------------------------------
// Helfer für Komponenten-Callbacks (bekommen nur `&mut App`)
// ---------------------------------------------------------------------------

/// Führt `f` mit der Shell + Context aus. Für Komponenten-Callbacks
/// (`on_change`/`on_select`/… bekommen nur `&mut App`).
pub(crate) fn update_shell(cx: &mut App, f: impl FnOnce(&mut AppShell, &mut Context<AppShell>)) {
    if let Some(shell) = cx
        .try_global::<SettingsUiState>()
        .and_then(|s| s.shell.upgrade())
    {
        let _ = shell.update(cx, f);
    }
}

/// Ändert UI-Zustand + notifyt die Shell (Re-Render).
pub(crate) fn mutate_state(cx: &mut App, f: impl FnOnce(&mut SettingsUiState)) {
    update_shell(cx, |_, cx| {
        f(cx.global_mut::<SettingsUiState>());
        cx.notify();
    });
}

/// Ändert eine Einstellung (`Settings::update` = ändern + **sofort
/// speichern**) + notifyt. Der kanonische Weg für „Row sofort wirksam“.
pub(crate) fn commit_setting(
    cx: &mut App,
    f: impl FnOnce(&mut chiaki_settings::settings::Settings),
) {
    update_shell(cx, |shell, cx| {
        let settings = shell.backend.settings().clone();
        let result = settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .update(f);
        if let Err(err) = result {
            tracing::warn!("Settings konnte nicht gespeichert werden: {err}");
        }
        cx.notify();
    });
}

/// Ändert Einstellung + UI-Zustand in einem Rutsch + notifyt.
pub(crate) fn commit_setting_and_state(
    cx: &mut App,
    f: impl FnOnce(&mut chiaki_settings::settings::Settings, &mut SettingsUiState),
) {
    update_shell(cx, |shell, cx| {
        let settings = shell.backend.settings().clone();
        let result = settings.lock().unwrap_or_else(|e| e.into_inner()).update(|s| {
            f(s, cx.global_mut::<SettingsUiState>());
        });
        if let Err(err) = result {
            tracing::warn!("Settings konnte nicht gespeichert werden: {err}");
        }
        cx.notify();
    });
}

/// Toast aus einem Komponenten-Callback heraus.
pub(crate) fn push_toast(cx: &mut App, kind: ToastKind, title: &'static str, message: &'static str) {
    update_shell(cx, |shell, cx| {
        shell.push_toast(ToastData::new(kind, title).message(message), cx);
    });
}

// Offenes Select-Popup dieses Frames (Thread-Local-Lese-Cache: die
// Row-Builder bekommen keinen `&App`; `page` spiegelt den Zustand hier
// einmal pro Frame).
thread_local! {
    static OPEN_SELECT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn is_open_cached(id: &str) -> bool {
    OPEN_SELECT.with(|cell| cell.borrow().as_ref().is_some_and(|open| open == id))
}

/// `on_open_change`-Callback für ein Select mit dieser ID (kontrolliertes
/// Popup — nur eines gleichzeitig offen).
pub(crate) fn open_toggler(id: &'static str) -> impl Fn(bool, &mut Window, &mut App) + 'static {
    move |open, _w, cx| {
        mutate_state(cx, |s| {
            if open {
                s.open_select = Some(id.to_string());
            } else if s.open_select.as_deref() == Some(id) {
                s.open_select = None;
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Row-Modell + Builder (Label links, Control rechts, Erklärzeile darunter)
// ---------------------------------------------------------------------------

/// Eine SettingsRow: Suchtext (Label + Erklärtext + Tags, lowercase) +
/// Sichtbarkeit (Abhängigkeit, z. B. „nur wenn VSR aktiv“) + Element +
/// Inaktiv-Markierung (siehe [`SRow::inactive` / `inactive()`]).
pub(crate) struct SRow {
    pub(crate) search: String,
    pub(crate) visible: bool,
    pub(crate) element: gpui::AnyElement,
    /// Nicht-`None` = Row ist im Rust-Build ohne Funktion; der String ist
    /// der kurze Grund (Audit-Flag, geprüft vom Test
    /// `inactive_rows_have_reason_and_audit_md_exists`).
    pub(crate) inactive: Option<&'static str>,
}

impl SRow {
    /// Wie QML `visible: filterMatch && dep`: ohne Suche zählt nur die
    /// Abhängigkeit, mit Suche zusätzlich der Textmatch.
    fn keep(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.visible && (needle.is_empty() || self.search.contains(&needle))
    }
}

/// Eine Sektions-Karte (wie `SSectionCard`): Titel + Rows.
pub(crate) struct Section {
    title: &'static str,
    rows: Vec<SRow>,
}

impl Section {
    pub(crate) fn new(title: &'static str) -> Self {
        Self { title, rows: Vec::new() }
    }

    pub(crate) fn push(&mut self, row: SRow) {
        self.rows.push(row);
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Label-/Erklärtext-Spalte + Suchtext einer Row.
fn row_text(label: &str, subtitle: Option<&str>) -> (gpui::Div, String) {
    let mut search = label.to_string();
    if let Some(sub) = subtitle {
        search.push(' ');
        search.push_str(sub);
    }
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_size(px(theme::SIZE_BODY))
                .text_color(theme::TEXT_PRIMARY)
                .child(label.to_string()),
        );
    if let Some(sub) = subtitle {
        col = col.child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(sub.to_string()),
        );
    }
    (col, search.to_lowercase())
}

fn row(
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    control: gpui::AnyElement,
) -> SRow {
    let (text_col, mut search) = row_text(label, subtitle);
    search.push(' ');
    search.push_str(&tags.to_lowercase());
    SRow {
        search,
        visible,
        element: div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(theme::SP_4))
            .child(text_col)
            .child(control)
            .into_any_element(),
        inactive: None,
    }
}

// ---------------------------------------------------------------------------
// Inaktiv-Markierung (Rust-Build ohne Funktion)
// ---------------------------------------------------------------------------

/// Erklärzeilen-Suffix für im Rust-Build inaktive Settings
/// („— im Rust-Build ohne Funktion (…Grund…)“).
pub(crate) fn inactive_hint(reason: &str) -> String {
    format!(" \u{2014} im Rust-Build ohne Funktion ({reason})")
}

/// Markiert eine Row als im Rust-Build inaktiv: setzt das Audit-Flag
/// (`SRow.inactive`) und hängt unterhalb der Row eine dezente Caption-Zeile
/// „Rust: inaktiv“ (Warn-Farbe) + Hinweistext an (Suchtext wird erweitert).
pub(crate) fn inactive_with(mut row: SRow, hint: String, reason: &'static str) -> SRow {
    row.inactive = Some(reason);
    row.search.push_str(" rust inaktiv ");
    row.search.push_str(&hint.to_lowercase());
    row.element = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(row.element)
        .child(
            div()
                .flex()
                .items_center()
                .flex_wrap()
                .gap_2()
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(theme::WARN)
                        .child("Rust: inaktiv"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_CAPTION))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(hint),
                ),
        )
        .into_any_element();
    row
}

/// Standard-Variante: Hinweis nach dem Schema
/// „Rust: inaktiv — im Rust-Build ohne Funktion (reason)“.
pub(crate) fn inactive(row: SRow, reason: &'static str) -> SRow {
    inactive_with(row, inactive_hint(reason), reason)
}

/// Select-Optionen aus (Wert, Label)-Paaren.
pub(crate) fn opts(pairs: &'static [(&'static str, &'static str)]) -> Vec<SelectOption> {
    pairs
        .iter()
        .map(|(value, label)| SelectOption::new(*value, *label))
        .collect()
}

/// Select-Row (Werte = INI-Strings, Labels = QML-Texte).
pub(crate) fn select_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    options: Vec<SelectOption>,
    selected: impl Into<gpui::SharedString>,
    on_select: impl Fn(&str, &mut chiaki_settings::settings::Settings) + 'static,
) -> SRow {
    let on_select = move |value: &gpui::SharedString, _w: &mut Window, cx: &mut App| {
        let value: &str = &**value;
        let value = value.to_string();
        commit_setting(cx, |s| on_select(&value, s));
    };
    row(
        label,
        subtitle,
        tags,
        visible,
        Select::new(id)
            .options(options)
            .selected(selected)
            .open(is_open_cached(id))
            .on_select(on_select)
            .on_open_change(open_toggler(id))
            .into_any_element(),
    )
}

/// Controller-Button-Namen für Kombo-Selects (Index = Chiaki-Button-Code,
/// wie im QML `buttonNames`-Array: 0 = „Not Used“, 1..16 = Button-Bits).
pub(crate) fn button_combo_names() -> Vec<String> {
    let mut names = vec!["Not Used".to_string()];
    for bit in 0..16u32 {
        names.push(
            chiaki_settings::settings::controller_button_name(1 << bit).to_string(),
        );
    }
    names
}

/// Kombo-Select (Wert = Button-Index 0..16, Label = Button-Name).
pub(crate) fn combo_select(
    id: &'static str,
    label: &str,
    tags: &str,
    visible: bool,
    value: u64,
    on_select: impl Fn(u64, &mut chiaki_settings::settings::Settings) + 'static,
) -> SRow {
    let names = button_combo_names();
    let options: Vec<SelectOption> = names
        .iter()
        .enumerate()
        .map(|(i, name)| SelectOption::new(i.to_string(), name.clone()))
        .collect();
    select_row(
        id,
        label,
        None,
        tags,
        visible,
        options,
        value.to_string(),
        move |v, s| on_select(v.parse().unwrap_or(0), s),
    )
}

/// Select-Row, die nur UI-Zustand ändert (kein Settings-Key), z. B.
/// „Settings for PS4/PS5“ in der Stream-Kategorie.
pub(crate) fn ui_select_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    options: Vec<SelectOption>,
    selected: impl Into<gpui::SharedString>,
    on_select: impl Fn(&str, &mut SettingsUiState) + 'static,
) -> SRow {
    let on_select = move |value: &gpui::SharedString, _w: &mut Window, cx: &mut App| {
        let value: &str = &**value;
        let value = value.to_string();
        mutate_state(cx, |s| on_select(&value, s));
    };
    row(
        label,
        subtitle,
        tags,
        visible,
        Select::new(id)
            .options(options)
            .selected(selected)
            .open(is_open_cached(id))
            .on_select(on_select)
            .on_open_change(open_toggler(id))
            .into_any_element(),
    )
}

/// Toggle-Row mit frei wählbarem Setter (für das Placebo-Fine-Tuning).
pub(crate) fn toggle_row_with(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    checked: bool,
    on_change: impl Fn(bool, &mut chiaki_settings::settings::Settings) + 'static,
) -> SRow {
    row(
        label,
        subtitle,
        tags,
        visible,
        Toggle::new(id, checked)
            .on_change(move |value: bool, _w: &mut Window, cx: &mut App| {
                commit_setting(cx, |s| on_change(value, s));
            })
            .into_any_element(),
    )
}

/// Toggle-Row (ID → Setter-Mapping siehe `set_bool`).
pub(crate) fn toggle_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    checked: bool,
) -> SRow {
    row(
        label,
        subtitle,
        tags,
        visible,
        Toggle::new(id, checked)
            .on_change(move |value: bool, _w: &mut Window, cx: &mut App| {
                commit_setting(cx, |s| set_bool(s, id, value));
            })
            .into_any_element(),
    )
}

/// ID → Settings-Setter (einzige Stelle, die Toggle-IDs auf Keys mappt;
/// die Kategorie-Dateien rufen die Builder mit genau diesen IDs).
fn set_bool(s: &mut chiaki_settings::settings::Settings, id: &str, value: bool) {
    match id {
        "general-streamer-mode" => s.set_streamer_mode(value),
        "general-automatic-connect" => s.set_automatic_connect(value),
        "general-auto-discovery" => s.set_discovery_enabled(value),
        "general-remote-play-ask" => s.set_remote_play_ask(value),
        "general-add-steam-shortcut-ask" => s.set_add_steam_shortcut_ask(value),
        "general-stream-menu-enabled" => s.set_stream_menu_enabled(value),
        "video-fullscreen-doubleclick" => s.set_fullscreen_double_click_enabled(value),
        "video-hide-cursor" => s.set_hide_cursor(value),
        "video-use-zero-copy" => s.set_use_zero_copy(value),
        "video-vsync" => s.set_vsync_enabled(value),
        "video-vulkan-deferred-swap" => s.set_vulkan_deferred_swap(value),
        "video-nv-vsr" => s.set_nv_vsr_enabled(value),
        "video-show-vsr-badge" => s.set_show_vsr_badge(value),
        "video-show-stream-stats" => s.set_show_stream_stats(value),
        "audio-start-mic-unmuted" => s.set_start_mic_unmuted(value),
        "audio-speech-processing" => s.set_speech_processing_enabled(value),
        "audio-idr-on-fec-failure" => s.set_idr_on_fec_failure_enabled(value),
        "controls-keyboard-enabled" => s.set_keyboard_enabled(value),
        "controls-mouse-touch" => s.set_mouse_touch_enabled(value),
        "controls-background-events" => s.set_allow_joystick_background_events(value),
        "controls-buttons-by-pos" => s.set_buttons_by_position(value),
        "controls-dpad-touch-enabled" => s.set_dpad_touch_enabled(value),
        "psn-port-guessing" => s.set_port_guessing_enabled(value),
        "system-log-sanitize" => s.set_log_sanitize(value),
        "system-log-verbose" => s.set_log_verbose(value),
        _ => tracing::warn!("toggle_row: unbekannte ID {id}"),
    }
}

/// Slider-Row mit Werteanzeige rechts (Formatierung im Aufruf).
pub(crate) fn slider_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    value_text: String,
    on_change: impl Fn(f64, &mut chiaki_settings::settings::Settings) + 'static,
) -> SRow {
    row(
        label,
        subtitle,
        tags,
        visible,
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                Slider::new(id, value, min, max).step(step).on_change(
                    move |v: f64, _w: &mut Window, cx: &mut App| {
                        commit_setting(cx, |s| on_change(v, s));
                    },
                ),
            )
            .child(
                div()
                    .min_w(px(150.0))
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(value_text),
            )
            .into_any_element(),
    )
}

/// Text-Row (kontrolliertes TextField; schreibt bei jeder Änderung).
pub(crate) fn text_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    value: String,
    placeholder: &'static str,
    focus: FocusHandle,
    on_change: impl Fn(String, &mut chiaki_settings::settings::Settings) + 'static,
) -> SRow {
    row(
        label,
        subtitle,
        tags,
        visible,
        TextField::new(id)
            .value(value)
            .placeholder(placeholder)
            .width(260.0)
            .focus_handle(focus)
            .on_change(move |v: &gpui::SharedString, _w: &mut Window, cx: &mut App| {
                let v: String = (&**v).to_string();
                commit_setting(cx, |s| on_change(v, s));
            })
            .into_any_element(),
    )
}

/// Action-Row: Label + Button rechts (Danger-Variante für destruktive Aktionen).
pub(crate) fn action_row(
    id: &'static str,
    label: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    button_label: &'static str,
    danger: bool,
    on_click: impl Fn(&mut AppShell, &mut Context<AppShell>) + 'static,
) -> SRow {
    let variant = if danger { ButtonVariant::Danger } else { ButtonVariant::Primary };
    row(
        label,
        subtitle,
        tags,
        visible,
        Button::new(id, button_label)
            .variant(variant)
            .on_click(
                move |_: &gpui::ClickEvent, _w: &mut Window, cx: &mut App| {
                    update_shell(cx, |shell, cx| {
                        on_click(shell, cx);
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
    )
}

/// Info-Row: nur Text (Hinweise, Pfade, Erklärungen) — wie `SInfoRow`.
pub(crate) fn info_row(text: &str, tags: &str) -> SRow {
    SRow {
        search: format!("{text} {tags}").to_lowercase(),
        visible: true,
        element: div()
            .text_size(px(theme::SIZE_CAPTION))
            .text_color(theme::TEXT_SECONDARY)
            .child(text.to_string())
            .into_any_element(),
        inactive: None,
    }
}

/// Individuelle Row (Keymap/Hosts): eigenes Layout, Suchtext hier.
pub(crate) fn custom_row(
    search_text: &str,
    subtitle: Option<&str>,
    tags: &str,
    visible: bool,
    element: gpui::AnyElement,
) -> SRow {
    let mut search = search_text.to_string();
    if let Some(sub) = subtitle {
        search.push(' ');
        search.push_str(sub);
    }
    search.push(' ');
    search.push_str(tags);
    SRow {
        search: search.to_lowercase(),
        visible,
        element,
        inactive: None,
    }
}

/// Ordnet die Control-Spalte rechtsbündig an (für custom_row-Layouts).
pub(crate) fn label_col(label: &str, subtitle: Option<&str>) -> gpui::Div {
    row_text(label, subtitle).0
}

// ---------------------------------------------------------------------------
// page()
// ---------------------------------------------------------------------------

pub fn page(
    shell: &mut AppShell,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> impl IntoElement {
    ensure_state(cx);

    // Zustand lesen (kurze Borrow; Kopien für den Frame-Bau).
    let (category, search, key_capture) = {
        let state = cx.global_mut::<SettingsUiState>();
        (state.category, state.search.clone(), state.key_capture)
    };

    // Offenes Select-Popup in den Builder-Cache spiegeln (1×/Frame).
    let open_select = cx
        .try_global::<SettingsUiState>()
        .and_then(|s| s.open_select.clone());
    OPEN_SELECT.with(|cell| *cell.borrow_mut() = open_select);

    let needle = search.trim().to_lowercase();
    let searching = !needle.is_empty();

    // Bauen + Live-Filter in einem Durchlauf: Rows behalten, die sichtbar
    // sind und (bei Suche) matchen; leere Sektionen kollabieren. Bei Suche
    // werden alle Kategorien durchsucht und pro Treffer-Kategorie ein
    // Kategorien-Kopf gesetzt; gezählt werden die Row-Treffer.
    let mut hits = 0usize;
    let mut rendered: Vec<gpui::AnyElement> = Vec::new();
    for (index, cat) in CATEGORIES.iter().enumerate() {
        let active_or_searching = searching || index == category;
        if !active_or_searching {
            continue;
        }
        let cat_sections: Vec<Section> = match *cat {
            "General" => general::sections(shell, &needle, cx),
            "Video" => video::sections(shell, &needle, cx),
            "Stream" => stream::sections(shell, &needle, cx),
            "Audio & Latency" => audio::sections(shell, &needle, cx),
            "Controls" => controls::sections(shell, &needle, cx),
            "Consoles" => consoles::sections(shell, &needle, cx),
            "PSN & Network" => psn::sections(shell, &needle, cx),
            "System" => system::sections(shell, &needle, cx),
            _ => Vec::new(),
        };

        let mut cards: Vec<gpui::AnyElement> = Vec::new();
        for mut section in cat_sections {
            section.rows.retain(|row| row.keep(&needle));
            if section.is_empty() {
                continue; // leere Sektion kollabiert
            }
            hits += section.rows.len();
            let mut card = Card::new(ElementId::Name(
                format!("section-{}", slug(&section.title)).into(),
            ))
            .child(SectionLabel::new(section.title));
            card.extend(section.rows.into_iter().map(|row| row.element));
            cards.push(card.into_any_element());
        }

        if cards.is_empty() {
            continue;
        }
        if searching {
            rendered.push(
                div()
                    .mt(px(theme::SP_2))
                    .child(SectionLabel::new(*cat))
                    .into_any_element(),
            );
        }
        rendered.extend(cards);
    }

    page_scaffold(build_children(
        shell, category, search, needle, hits, rendered, key_capture, window, cx,
    ))
}

fn slug(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_children(
    shell: &mut AppShell,
    category: usize,
    search: String,
    needle: String,
    hits: usize,
    rendered: Vec<gpui::AnyElement>,
    key_capture: Option<u32>,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> Vec<gpui::AnyElement> {
    let searching = !needle.is_empty();

    let mut children: Vec<gpui::AnyElement> = Vec::new();
    children.push(page_header(shell, "Einstellungen", None, window, cx));

    // Kopfzeile: Suchfeld + Treffer-Zählung bzw. aktive Kategorie.
    let search_focus = cx
        .try_global::<SettingsUiState>()
        .map(|s| s.search_focus.clone());
    // Fokus-Halten: der Shell setzt seinen eigenen Fokus pro Frame per
    // `window.defer` zurück (app.rs) — das Suchfeld queued seinen Refocus
    // danach, solange es fokussiert ist (gleiche Technik wie das
    // Key-Capture-Overlay).
    if let Some(fh) = search_focus.as_ref() {
        if fh.is_focused(window) {
            let fh = fh.clone();
            window.defer(cx, move |window, _cx| fh.focus(window));
        }
    }
    let mut top = div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(theme::SP_4))
        .child(
            TextField::new("settings-search")
                .value(search.clone())
                .placeholder("Search all settings")
                .width(360.0)
                .focus_handle(search_focus.clone().unwrap_or_else(|| cx.focus_handle()))
                .on_change(|v: &gpui::SharedString, _w, cx| {
                    let v: String = (&**v).to_string();
                    mutate_state(cx, |s| s.search = v);
                }),
        );
    if searching {
        top = top
            .child(
                div()
                    .text_size(px(theme::SIZE_CAPTION))
                    .text_color(theme::TEXT_SECONDARY)
                    .child(format!(
                        "{hits} result{} for \u{201C}{}\u{201D}",
                        if hits == 1 { "" } else { "s" },
                        search.trim()
                    )),
            )
            .child(
                Button::new("settings-search-clear", "Clear").on_click(|_, _w, cx| {
                    mutate_state(cx, |s| s.search = String::new());
                }),
            );
    } else {
        top = top.child(
            div()
                .text_size(px(theme::SIZE_CAPTION))
                .text_color(theme::TEXT_SECONDARY)
                .child(CATEGORIES[category].to_string()),
        );
    }
    children.push(top.into_any_element());

    // Zweispaltiger Rahmen: Kategorie-Rail links, Inhalt rechts (Scrollbar).
    let rail: Vec<gpui::AnyElement> = CATEGORIES
        .iter()
        .enumerate()
        .map(|(i, cat)| {
            let active = !searching && i == category;
            div()
                .id(ElementId::Name(format!("cat-{i}").into()))
                .px(px(theme::SP_3))
                .py(px(theme::SP_2 + 2.0))
                .rounded(px(theme::RADIUS_SM))
                .text_size(px(theme::SIZE_BODY))
                .text_color(if active { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY })
                .bg(if active {
                    theme::ACCENT.opacity(0.16)
                } else {
                    gpui::transparent_black()
                })
                .hover(|s| s.bg(theme::SURFACE2))
                .cursor_pointer()
                .child(cat.to_string())
                .on_click(cx.listener(move |_, _ev, _window, cx| {
                    let state = cx.global_mut::<SettingsUiState>();
                    state.category = i;
                    state.search = String::new();
                    state.open_select = None;
                    cx.notify();
                }))
                .into_any_element()
        })
        .collect();

    let content: gpui::AnyElement = if searching && hits == 0 {
        EmptyState::new(
            icons::paths::SEARCH,
            "No settings match",
            format!("\u{201C}{}\u{201D} \u{2014} try another term.", search.trim()),
        )
        .into_any_element()
    } else {
        div()
            .id("settings-scroll")
            .flex()
            .flex_col()
            .gap(px(theme::SP_4))
            // Gebundene Höhe ist Pflicht für overflow_y_scroll: ohne
            // flex_1/min_h_0 wächst der Container unbegrenzt und der
            // Inhalt wird schlicht abgeschnitten (kein Scrollen möglich).
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .children(rendered)
            .into_any_element()
    };

    children.push(
        div()
            .flex()
            .flex_row()
            .gap(px(theme::SP_5))
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .w(px(220.0))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(SectionLabel::new("Settings"))
                    .children(rail),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .border_l_1()
                    .border_color(theme::HAIRLINE)
                    .pl(px(theme::SP_5))
                    .child(content),
            )
            .into_any_element(),
    );

    // Key-Capture-Overlay (Controls → Key Mapping).
    if let Some(button) = key_capture {
        children.push(key_capture_overlay(button, window, cx).into_any_element());
    }

    children
}

// ---------------------------------------------------------------------------
// Key-Capture (Controls → Key Mapping)
// ---------------------------------------------------------------------------

/// Öffnet das Capture-Overlay für einen Controller-Button (nächster
/// Tastendruck wird als Belegung übernommen — wie im QML `openKeyCapture`).
pub(crate) fn start_key_capture(cx: &mut App, window: &mut Window, button: u32) {
    let focus = cx
        .try_global::<SettingsUiState>()
        .map(|s| s.capture_focus.clone());
    mutate_state(cx, |s| s.key_capture = Some(button));
    if let Some(focus) = focus {
        focus.focus(window);
    }
}

fn key_capture_overlay(
    button: u32,
    window: &mut Window,
    cx: &mut Context<AppShell>,
) -> gpui::AnyElement {
    let focus = cx
        .try_global::<SettingsUiState>()
        .map(|s| s.capture_focus.clone())
        .unwrap_or_else(|| cx.focus_handle());
    // Fokus sicherstellen (Overlay ist in diesem Frame gemountet).
    let focus_for_defer = focus.clone();
    window.defer(cx, move |window, _cx| focus_for_defer.focus(window));

    div()
        .id("key-capture-layer")
        .absolute()
        .inset_0()
        .bg(theme::BACKDROP)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_click(|_, _w, cx| mutate_state(cx, |s| s.key_capture = None))
        .track_focus(&focus)
        .on_key_down(cx.listener(
            move |shell: &mut AppShell, event: &KeyDownEvent, _window, cx| {
            let key: &str = &event.keystroke.key;
            let settings = shell.backend.settings().clone();
            if key != "escape" {
                if let Some(k) = controls::key_from_gpui(key) {
                    let qt_name = k.to_qt_name();
                    let result = settings
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .update(|s| s.set_controller_button_mapping(button, &qt_name));
                    if let Err(err) = result {
                        tracing::warn!("Keymap konnte nicht gespeichert werden: {err}");
                    }
                }
            }
                cx.global_mut::<SettingsUiState>().key_capture = None;
                // Shell-Escape-Handler (Route → Home) nicht zusätzlich feuern.
                cx.stop_propagation();
                cx.notify();
            },
        ))
        .child(
            div()
                .w(px(420.0))
                .flex()
                .flex_col()
                .gap(px(theme::SP_2))
                .rounded(px(theme::RADIUS_LG))
                .bg(theme::SURFACE2)
                .border_1()
                .border_color(theme::OUTLINE)
                .p(px(theme::SP_4))
                .child(
                    div()
                        .text_size(px(theme::SIZE_HEADLINE))
                        .font_weight(gpui::FontWeight(theme::WEIGHT_HEADLINE))
                        .text_color(theme::TEXT_PRIMARY)
                        .child("Key Capture"),
                )
                .child(
                    div()
                        .text_size(px(theme::SIZE_BODY))
                        .text_color(theme::TEXT_SECONDARY)
                        .child(
                            "Press any key to configure the button, or click outside to cancel.",
                        ),
                ),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> chiaki_settings::settings::Settings {
        let base = std::env::temp_dir().join(format!(
            "chiaki-ui-settings-page-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        chiaki_settings::settings::Settings::open_at(chiaki_settings::settings::SettingsPaths {
            settings: base.join("settings.ini"),
            default_settings: base.join("settings.default.ini"),
            placebo: base.join("placebo_render_params.ini"),
            base,
        })
        .unwrap()
    }

    /// Die Toggle-ID-Bridge (`set_bool`) muss genau die INI-Keys schreiben,
    /// die das C++ nutzt — Persistenzpfad der Settings-Seite
    /// (`Settings::update` = ändern + sofort speichern).
    #[test]
    fn toggle_ids_persist_cpp_ini_keys() {
        let mut s = test_settings();
        s.update(|s| {
            set_bool(s, "general-streamer-mode", true);
            set_bool(s, "video-vsync", true);
            set_bool(s, "psn-port-guessing", true);
            set_bool(s, "system-log-verbose", true);
        })
        .unwrap();

        let ini = std::fs::read_to_string(s.paths().settings.clone()).unwrap();
        assert!(ini.contains("streamer_mode=true"), "ini: {ini}");
        assert!(ini.contains("vsync=true"), "ini: {ini}");
        assert!(ini.contains("port_guessing_enabled=true"), "ini: {ini}");
        assert!(ini.contains("log_verbose=true"), "ini: {ini}");

        // Rücklesen über die Getter (gleiche Keys wie das C++).
        assert!(s.streamer_mode());
        assert!(s.vsync_enabled());
        assert!(s.port_guessing_enabled());
        assert!(s.log_verbose());
    }

    /// Live-Suche: Suchtext = Label + Erklärtext + Tags (case-insensitive);
    /// `keep` wie QML `visible: filterMatch && dep`.
    #[test]
    fn search_row_matching_wie_qml() {
        let row = toggle_row(
            "general-streamer-mode",
            "Streamer mode",
            Some("Hides sensitive info (MAC addresses, console names, PINs)"),
            "privacy hide twitch",
            true,
            true,
        );
        assert!(row.keep(""));
        assert!(row.keep("STREAMER")); // Label
        assert!(row.keep("mac addresses")); // Erklärtext
        assert!(row.keep("twitch")); // Tags
        assert!(!row.keep("vsr"));

        // Abhängigkeit: unsichtbare Rows bleiben auch bei Match unsichtbar.
        let hidden = info_row("Upscale factor", "scale factor");
        let hidden = SRow {
            search: hidden.search,
            visible: false,
            element: hidden.element,
            inactive: None,
        };
        assert!(!hidden.keep("upscale"));
    }

    /// Audit-Verdrahtung: SETTINGS-AUDIT.md existiert im Workspace-Root und
    /// jede Row mit `inactive`-Flag trägt einen Reason-String — der
    /// `inactive()`-Wrapper setzt ihn immer, Builder-Rows sind per Default
    /// aktiv (`None`).
    #[test]
    fn inactive_rows_have_reason_and_audit_md_exists() {
        // Audit-Dokument vorhanden (Workspace-Root = ../.. vom ui-Crate).
        let audit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../SETTINGS-AUDIT.md");
        assert!(
            audit.exists(),
            "SETTINGS-AUDIT.md fehlt im Workspace-Root: {}",
            audit.display()
        );

        // Aktive Row: kein Flag.
        let active = toggle_row(
            "general-streamer-mode",
            "Streamer mode",
            None,
            "privacy",
            true,
            true,
        );
        assert!(active.inactive.is_none(), "Builder-Rows dürfen kein inactive-Flag haben");

        // Inaktive Row über den Wrapper: Flag + Standard-Hint-Schema.
        let marked = inactive(
            toggle_row("video-use-zero-copy", "Zero-copy presentation", None, "latency", true, true),
            "Zero-Copy-Pfad im Rust-Renderer nicht vorhanden",
        );
        let reason = marked.inactive.expect("inaktive Row braucht einen Reason-String");
        assert!(!reason.trim().is_empty());
        // Der Hint ist im Suchtext (Suche soll inaktive Rows finden).
        assert!(marked.search.contains(&inactive_hint(reason).to_lowercase()));

        // Freier Hinweistext (z. B. vsync) setzt den Reason ebenfalls.
        let custom = inactive_with(
            toggle_row("video-vsync", "Vertical sync", None, "vsync", true, false),
            "im Rust-Renderer immer aus (Present ohne Sync)".to_string(),
            "gpui presentet immer mit SyncInterval 0",
        );
        assert_eq!(
            custom.inactive,
            Some("gpui presentet immer mit SyncInterval 0")
        );
        assert!(custom.search.contains("immer aus"));
    }
}
