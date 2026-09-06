// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//! chiaki-ui: GPUI-Oberfläche des chiaki-ng-Ports (Windows-only).
//!
//! Fundament-Architektur:
//! * [`theme`] — Design-System 1:1 aus `docs/ui-v2-spec.md` §3 (bindend).
//! * [`components`] — dünne gpui-Komponenten (neu gebaut, Spec §4;
//!   gpui-component bewusst NICHT verwendet — siehe components/mod.rs).
//! * [`backend`] — gpui-freier Backend-Layer (Settings/Discovery/Controller/
//!   Session) mit Poll-basiertem [`backend::UiEvent`]-Stream.
//! * [`app`] — AppShell-Entity: NavRail, Route-Switching (Fade+Slide 220 ms),
//!   Fokus/Esc-Handling, Toast-/Dialog-Stacks, Backend-Event-Loop.
//! * [`pages`] — Seitengerüste (Home/Konsolen/Einstellungen/Info/Stream +
//!   Registrierungs-Wizard), gefüllt von den Nachfolge-Agents gegen
//!   `CONTRACT-UI.md`.
//!
//! Einstieg für chiaki-app: [`run()`] — blockiert bis zum Fensterschluss
//! (gpui beendet die App mit dem letzten Fenster).

pub mod app;
pub mod backend;
pub mod components;
pub mod icons;
pub mod motion;
pub mod pages;
pub mod psn_login;
pub mod theme;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{px, AppContext as _, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

pub use app::{AppShell, Route};

/// Startet die App (tracing + Settings + gpui-Fenster) und blockiert bis
/// zum Fensterschluss. chiaki-app braucht nur diesen Aufruf.
///
/// `profile` entspricht dem `--profile <name>`-Argument der C++-GUI
/// (QCommandLineOption "profile" in gui/src/main.cpp): Nicht-leere Namen
/// laden `profiles/<name>.ini` statt `settings.ini` (Steam-Launch-Options).
pub fn run(profile: Option<String>) -> gpui::Result<()> {
    // settings/log_verbose wird VOR dem tracing-Init gelesen (Peek, ohne
    // Migration): EnvFilter-Default debug statt info, wenn der Schalter an
    // ist. RUST_LOG überschreibt weiterhin alles.
    init_tracing(chiaki_settings::settings::Settings::peek_log_verbose(
        profile.as_deref(),
    ));
    install_panic_hook();

    let settings = Arc::new(Mutex::new(
        chiaki_settings::settings::Settings::open(profile.as_deref())?,
    ));

    let backend = backend::Backend::start(Arc::clone(&settings))?;

    tracing::info!(
        "chiaki-ui startet (base: {}, profil: {})",
        chiaki_settings::app_paths::base_path().display(),
        profile.as_deref().unwrap_or(""),
    );

    Application::new().with_assets(icons::IconAssets).run(move |cx: &mut gpui::App| {
        let backend_for_quit = backend.clone();
        cx.on_app_quit(move |_cx| {
            let backend = backend_for_quit.clone();
            async move {
                backend.shutdown();
            }
        })
        .detach();

        // AppShell-Entity zuerst (Handle für die Event-Loop), dann Fenster.
        let shell: gpui::Entity<AppShell> = cx.new(|cx| AppShell::new(backend.clone(), cx));

        let backend_for_close = backend.clone();
        let bounds = Bounds::centered(None, gpui::size(px(1280.0), px(800.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Chiaki Remaster".into()),
                    ..Default::default()
                }),
                window_min_size: Some(gpui::Size::new(px(960.0), px(560.0))),
                ..Default::default()
            },
            |window, cx| {
                // Close-Interception: gpui beendet den Prozess, sobald das
                // letzte Fenster zu ist — und `on_app_quit`-Futures bekommen
                // dafür nur 100 ms (gpui SHUTDOWN_TIMEOUT). Läuft beim Schließen
                // noch eine Session, würden Media-/GPU-Sink-Threads hart im
                // Prozess-Teardown sterben (D3D11/CUDA-Aufrufe im Entladen —
                // beobachteter stiller Absturz). Deshalb: WM_CLOSE fängt das
                // Fenster hier ab, fährt das Backend **synchron und bounded**
                // herunter und lässt erst dann das Schließen zu.
                window.on_window_should_close(cx, move |_window, cx| {
                    // Stream-Global zuerst entfernen (Fake-Thread etc.) —
                    // der Zustand gehört zum Fenster, nicht zur Session.
                    if cx.has_global::<crate::pages::stream::state::StreamUiState>() {
                        let mut state =
                            cx.remove_global::<crate::pages::stream::state::StreamUiState>();
                        state.stop_threads();
                    }
                    backend_for_close.shutdown_bounded(Duration::from_secs(4));
                    true
                });
                shell.clone()
            },
        )
        .expect("Hauptfenster konnte nicht geöffnet werden");

        // Backend-Event-Loop: pollt die UiEventQueue und notifyt die Shell.
        spawn_backend_event_loop(shell.downgrade(), cx);
    });

    Ok(())
}

/// Backend-Event-Loop: tickt alle 100 ms, leert die UiEventQueue der Shell
/// und notifyt sie bei Treffern. Endet, wenn die Shell weg ist (App-Ende).
fn spawn_backend_event_loop(shell: gpui::WeakEntity<AppShell>, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            match shell.update(cx, |shell, cx| shell.apply_events(cx)) {
                Ok(()) => {}
                Err(_) => break, // Shell weg → App beendet sich.
            }
        }
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Panic-Hook: Panics landen in chiaki-ui.log UND panics.log — im
// Windows-Subsystem-Build wäre stderr verloren, und ein stiller Absturz
// (z. B. beim Stream-Teardown) war bislang nicht lokalisierbar.
// ---------------------------------------------------------------------------

/// Installiert den globalen Panic-Hook: Message + Thread + Stelle + Backtrace
/// gehen per `tracing::error!` in die normale Logdatei und zusätzlich direkt
/// (append, ohne tracing — der Hook muss auch funktionieren, wenn tracing
/// selbst in den Panic verwickelt ist) nach `panics.log` im Log-Ordner.
/// Der Default-Hook bleibt verkettet (stderr bei Konsolen-Starts).
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unbekannt>".to_string());
        let payload = info.payload();
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "<nicht-String-Panic-Payload>".to_string()
        };
        let thread = std::thread::current();
        let report = format!(
            "PANIC in Thread '{}' an {location}: {message}\n{}",
            thread.name().unwrap_or("<unnamed>"),
            std::backtrace::Backtrace::force_capture(),
        );
        tracing::error!("{report}");
        if std::fs::create_dir_all(chiaki_settings::app_paths::log_dir()).is_ok() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(chiaki_settings::app_paths::log_dir().join("panics.log"))
            {
                use std::io::Write as _;
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let _ = writeln!(file, "[unix {stamp}] {report}");
            }
        }
        default_hook(info);
    }));
}

// ---------------------------------------------------------------------------
// Logging: Datei (log_dir) + Konsole, Level via RUST_LOG (Default: info)
// ---------------------------------------------------------------------------

/// tracing-Subscriber: fmt-Layer auf Logdatei + stdout. `verbose` = der
/// settings/log_verbose-Schalter (Default-Level debug statt info); RUST_LOG
/// überschreibt weiterhin alles. Fehler werden ignoriert (App läuft auch
/// ohne Logdatei).
fn init_tracing(verbose: bool) {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let default_filter = if verbose { "debug" } else { "info" };

    let file_layer = std::fs::create_dir_all(chiaki_settings::app_paths::log_dir())
        .ok()
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(chiaki_settings::app_paths::log_dir().join("chiaki-ui.log"))
                .ok()
        })
        .map(|file| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(FileWriter(Arc::new(file))))
        });

    let console_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stdout);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .with(file_layer)
        .with(console_layer)
        .init();
}

/// `MakeWriter`-Adapter für eine geteilte Logdatei.
struct FileWriter(Arc<std::fs::File>);

impl std::io::Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriter(Arc::clone(&self.0))
    }
}
