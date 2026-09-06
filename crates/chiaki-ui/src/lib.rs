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

use gpui::{px, AppContext as _, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions};

pub use app::{AppShell, Route};

/// Startet die App (tracing + Settings + gpui-Fenster) und blockiert bis
/// zum Beenden. chiaki-app braucht nur diesen Aufruf.
///
/// `profile` entspricht dem `--profile <name>`-Argument der C++-GUI
/// (QCommandLineOption "profile" in gui/src/main.cpp): Nicht-leere Namen
/// laden `profiles/<name>.ini` statt `settings.ini` (Steam-Launch-Options).
pub fn run(profile: Option<String>) -> gpui::Result<()> {
    init_tracing();

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
            |_window, _cx| shell.clone(),
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
// Logging: Datei (log_dir) + Konsole, Level via RUST_LOG (Default: info)
// ---------------------------------------------------------------------------

/// tracing-Subscriber: fmt-Layer auf Logdatei + stdout. Fehler werden
/// ignoriert (App läuft auch ohne Logdatei).
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

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
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
