// SPDX-License-Identifier: AGPL-3.0-only
//! Headless-Virtualcam-Modus (`chiaki --virtualcam [host]`, HANDOFF §8/V2):
//! Session + Media-Pipeline ohne gpui/Sink-Fenster — das Video landet in der
//! virtuellen Kamera („OBS Virtual Camera“), der Ton bleibt lokal (User hört
//! über den PC). Als Hintergrundprozess startbar (Autostart-Option der
//! Settings schreibt den Run-Key mit genau diesem Modus).
//!
//! Aufbau (bewusst eigenständig, nicht über chiaki-ui — das Media-Modul dort
//! ist `pub(crate)`): Callbacks kopieren die AV-Frames in Channels (gleiche
//! Thread-Disziplin wie sessions.rs: Decoder/AudioOutput/CamFeed besitzt EIN
//! Media-Thread), der Media-Thread dekodiert ALLE Frames in FIFO-Reihenfolge
//! (H.265-Referenzkette) und feedt die Kamera + Audio-Ausgabe. Decode läuft
//! mit CPU-Transfer (DecoderOpts::default) — die Frames kommen als NV12 im
//! Systemspeicher an, kein GPU-Download nötig. Bedienung (Controller) gibt es
//! hier nicht — der Modus ist ein reiner Feed; Beenden per Ctrl+C oder wenn
//! die Konsole die Session beendet.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chiaki_core::session::{Session, SessionCallbacks, SessionEvent};
use chiaki_media::decoder::{DecoderOpts, HwBackend};
use chiaki_media::opus::OpusAudioDecoder;
use chiaki_media::{AudioOutput, Decoder};
use chiaki_settings::hosts::RegisteredHost;
use chiaki_settings::settings::Settings;
use chiaki_virtualcam::{CamFeed, CamFeedConfig, CamResolution};

use chiaki_ui::backend::sessions::{
    build_connect_info, hw_backend_from_setting, ConnectRequest, LinkQuality,
};

/// Mutex-Lock mit Poison-Recovery (Library-Pfad ohne Panic).
fn lock<'a, T>(guard: Result<MutexGuard<'a, T>, PoisonError<MutexGuard<'a, T>>>) -> MutexGuard<'a, T> {
    guard.unwrap_or_else(PoisonError::into_inner)
}

/// Kapazität der Video-FIFO wie in sessions.rs (≈266 ms @60 — der Media-
/// Thread dekodiert schneller als der Netzwerk-Taktfrequenz-Nachschub kommt).
const VIDEO_FIFO_CAP: usize = 16;

enum MediaCmd {
    Audio(Vec<u8>),
    AudioHeader(chiaki_core::audio::AudioHeader),
    Video,
}

struct Shared {
    video: Mutex<VecDeque<(Vec<u8>, i32, bool)>>,
    video_dropped: AtomicU64,
    media_tx: Mutex<Option<Sender<MediaCmd>>>,
    /// `Some((Grund, ist_Fehler))` sobald die Session mit Quit-Event endet.
    quit: Mutex<Option<(String, bool)>>,
    quit_flag: AtomicBool,
    cam_frames: AtomicU64,
    cam_errors: AtomicU64,
}

impl Shared {
    fn push_video(&self, data: Vec<u8>, frames_lost: i32, recovered: bool) {
        let mut queue = lock(self.video.lock());
        while queue.len() >= VIDEO_FIFO_CAP {
            queue.pop_front();
            self.video_dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back((data, frames_lost, recovered));
    }

    fn pop_video(&self) -> Option<(Vec<u8>, i32, bool)> {
        lock(self.video.lock()).pop_front()
    }

    fn send_media(&self, cmd: MediaCmd) {
        if let Some(tx) = lock(self.media_tx.lock()).as_ref() {
            let _ = tx.send(cmd);
        }
    }
}

struct HeadlessCallbacks {
    shared: Arc<Shared>,
}

impl SessionCallbacks for HeadlessCallbacks {
    fn video_frame(&self, frame: chiaki_core::session::VideoFrame<'_>) -> bool {
        self.shared.push_video(frame.data.to_vec(), frame.frames_lost, frame.frame_recovered);
        self.shared.send_media(MediaCmd::Video);
        true
    }

    fn audio_pcm(&self, pcm: &[u8]) {
        self.shared.send_media(MediaCmd::Audio(pcm.to_vec()));
    }

    fn event(&self, ev: SessionEvent) {
        match &ev {
            SessionEvent::AudioStreamInfo(header) => {
                self.shared.send_media(MediaCmd::AudioHeader(*header));
            }
            SessionEvent::Quit { reason, reason_str } => {
                let text = if reason_str.is_empty() {
                    format!("Session quit: {}", chiaki_core::session::quit_reason_string(*reason))
                } else {
                    format!(
                        "Session quit: {} ({reason_str})",
                        chiaki_core::session::quit_reason_string(*reason)
                    )
                };
                *lock(self.shared.quit.lock()) =
                    Some((text, chiaki_core::session::quit_reason_is_error(*reason)));
                self.shared.quit_flag.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

/// Einstieg von main.rs. `host` = Nickname eines registrierten Hosts (None =
/// der erste zugeordnete manuelle Host). Läuft blockierend bis Ctrl+C bzw.
/// Session-Ende; Fehler als Text (main.rs: stderr + Exit 1).
pub fn run(host: Option<String>, profile: Option<String>) -> Result<(), String> {
    init_tracing();
    tracing::info!("Headless-Virtualcam-Modus startet (host-Vorgabe: {host:?})");

    let settings = Settings::open(profile.as_deref()).map_err(|e| format!("Settings: {e}"))?;
    let (registered, host_addr) = resolve_host(&settings, host.as_deref())?;
    let video_profile = {
        // Profil-Auflösung identisch zur GUI (ps5 × local) — der Modus ist
        // ein LAN-Feed; PSN-Holepunch ist hier bewusst nicht verdrahtet.
        let req = ConnectRequest::from_registered(&registered, host_addr.clone(), LinkQuality::Local);
        let info = build_connect_info(&req, &settings).map_err(|e| format!("ConnectInfo: {e}"))?;
        info.video_profile
    };

    // Nur EINE Headless-Instanz (Instanz-Mutex + PID-Datei — die GUI zeigt
    // den Status und schickt das Stop-Event); Drop am Funktionsende räumt auf.
    let instance = chiaki_virtualcam::ipc::InstanceGuard::acquire(
        chiaki_virtualcam::ipc::default_pid_path(),
    )
    .map_err(|err| format!("Headless-Instanz: {err}"))?;

    // GUI-fähiger Stop (Settings-Button): Named Event im Hauptloop.
    let stop_event = chiaki_virtualcam::ipc::StopEvent::create()
        .map_err(|err| format!("Stop-Event: {err}"))?;

    // Kamera VOR der Session öffnen (Fehler → sauber beenden statt still
    // ohne Feed zu laufen — im Headless-Modus ist die Kamera der Zweck).
    // Bei VSR läuft sie im VSR-Output-Format (wie die GUI — User-Vorgabe).
    // Verfügbarkeits-Gate wie im GUI-Session-Start (Rust-Erweiterung): ohne
    // NVIDIA-Treiber/VFX-SDK würde der erzwungene CUDA-Decoder den Feed
    // komplett scheitern lassen — stattdessen läuft der Headless-Feed ohne
    // VSR in Stream-Auflösung.
    let mut nv_vsr = settings.nv_vsr_enabled();
    if nv_vsr {
        let cuda = chiaki_media::vsr::cuda_available();
        let sdk = {
            let path = settings.nv_vsr_sdk_path();
            let path = path.trim();
            chiaki_media::vsr::sdk_dir_resolvable(
                (!path.is_empty()).then_some(std::path::Path::new(path)),
            )
        };
        if !cuda || !sdk {
            tracing::warn!(
                "VSR aktiviert, aber nicht verfügbar (CUDA-Treiber: {cuda}, VFX-SDK: {sdk}) — Headless-Feed läuft ohne VSR"
            );
            nv_vsr = false;
        }
    }
    let nv_vsr_scale = {
        let raw = settings.nv_vsr_scale().clamp(0, i64::from(u32::MAX)) as u32;
        if (100..=400).contains(&raw) { raw } else { 200 }
    };
    let nv_vsr_quality = match settings.nv_vsr_quality() {
        1..=3 => Some(settings.nv_vsr_quality() as u32),
        _ => None,
    };
    let (cam_w, cam_h, cam_resolution) = if nv_vsr {
        let (w, h) = chiaki_media::vsr::output_dims(
            video_profile.width,
            video_profile.height,
            nv_vsr_scale,
        );
        tracing::info!("Kamera im VSR-Output-Format {w}x{h} (virtualcam_resolution ohne Wirkung)");
        (w, h, CamResolution::Stream)
    } else {
        (
            video_profile.width,
            video_profile.height,
            CamResolution::from_ini_value(&settings.virtualcam_resolution()),
        )
    };
    let mut cam = CamFeed::open(CamFeedConfig {
        width: cam_w,
        height: cam_h,
        fps: video_profile.max_fps,
        resolution: cam_resolution,
    })
    .map_err(|err| format!("Virtuelle Kamera: {err}"))?;
    tracing::info!(
        "Kamera aktiv: {}x{} @ {} (Quelle {}x{}, VSR {nv_vsr})",
        cam.dims().0,
        cam.dims().1,
        video_profile.max_fps,
        video_profile.width,
        video_profile.height
    );

    let connect_info = {
        let req =
            ConnectRequest::from_registered(&registered, host_addr.clone(), LinkQuality::Local);
        build_connect_info(&req, &settings).map_err(|e| format!("ConnectInfo: {e}"))?
    };

    let shared = Arc::new(Shared {
        video: Mutex::new(VecDeque::with_capacity(VIDEO_FIFO_CAP + 1)),
        video_dropped: AtomicU64::new(0),
        media_tx: Mutex::new(None),
        quit: Mutex::new(None),
        quit_flag: AtomicBool::new(false),
        cam_frames: AtomicU64::new(0),
        cam_errors: AtomicU64::new(0),
    });

    let mut session = Session::new(connect_info, Arc::new(HeadlessCallbacks { shared: Arc::clone(&shared) }))
        .map_err(|e| format!("Session: {e}"))?;
    session.start().map_err(|e| format!("Session-Start: {e}"))?;

    let (media_tx, media_rx) = std::sync::mpsc::channel::<MediaCmd>();
    *lock(shared.media_tx.lock()) = Some(media_tx);

    // Media-Thread (Decoder + AudioOutput + CamFeed — !Sync-Besitz).
    // VSR erzwingt den CUDA-Decoder wie in der GUI (vsrupscaler-Vertrag).
    let media_settings = MediaSnapshot {
        hw_backend: if nv_vsr {
            chiaki_media::decoder::HwBackend::Cuda
        } else {
            hw_backend_from_setting(&settings.hw_decoder())
        },
        codec: video_profile.codec,
        max_fps: video_profile.max_fps,
        nv_vsr,
        nv_vsr_scale,
        nv_vsr_quality,
        nv_vsr_sdk_path: {
            let path = settings.nv_vsr_sdk_path();
            (!path.trim().is_empty()).then_some(std::path::PathBuf::from(path))
        },
        audio_out_device: {
            let dev = settings.audio_out_device();
            (!dev.trim().is_empty()).then_some(dev)
        },
        audio_volume: settings.audio_volume().clamp(0, 128) as u32,
        audio_buffer_size: settings.audio_buffer_size().min(u32::MAX as u64) as u32,
    };
    let media_shared = Arc::clone(&shared);
    let media_thread = std::thread::Builder::new()
        .name("chiaki-vcam-media".into())
        .spawn(move || media_loop(media_settings, media_rx, media_shared, cam));
    if let Err(err) = media_thread {
        return Err(format!("Media-Thread: {err}"));
    }

    // Ctrl+C → sauberes Beenden (Session stoppen, Media-Kanal schließen).
    let stop_flag = Arc::new(AtomicBool::new(false));
    let _ = ctrlc::set_handler({
        let stop_flag = Arc::clone(&stop_flag);
        move || stop_flag.store(true, Ordering::SeqCst)
    });

    println!(
        "Headless-Virtualcam läuft: {} ({}) {}x{}@{} — Ctrl+C beendet.",
        registered.server_nickname,
        host_addr,
        video_profile.width,
        video_profile.height,
        video_profile.max_fps,
    );

    // Hauptschleife: Quit-Event (Session-Ende), Ctrl+C oder das Stop-Event
    // der GUI abwarten (Wait ersetzt den Sleep — Stop wirkt sofort).
    while !shared.quit_flag.load(Ordering::SeqCst) && !stop_flag.load(Ordering::SeqCst) {
        if stop_event.wait(100) {
            tracing::info!("Stop-Signal (GUI) empfangen — Headless-Feed wird beendet");
            println!("Stop-Signal empfangen — Headless-Feed wird beendet.");
            break;
        }
    }
    // `instance` (Instanz-Mutex + PID-Datei) wird am Funktionsende per RAII
    // freigegeben — nach dem Session-Teardown, damit die GUI den Status
    // „läuft" so lange korrekt zeigt, wie wirklich aufgeräumt wird.

    // Media-Kanal zuerst schließen → der Media-Thread räumt Audio/Kamera auf
    // und endet; dann Session stoppen (Reihenfolge wie sessions.rs-Teardown).
    *lock(shared.media_tx.lock()) = None;
    session.stop();
    let _ = session.join();
    session.fini();

    // Session-Fehler (Konsole aus, Timeout …) als Exit-Fehler signalisieren —
    // Ctrl+C-Beenden gilt als normal.
    let quit = lock(shared.quit.lock()).clone();
    if let Some((reason, is_error)) = &quit {
        if *is_error {
            tracing::error!("{reason}");
            println!("{reason}");
            return Err(reason.clone());
        }
        tracing::info!("{reason}");
        println!("{reason}");
    }
    tracing::info!("Headless-Virtualcam beendet");
    Ok(())
}

/// (registrierter Host, Adresse) für den Modus:
/// 1. `--virtualcam <nickname>` → Host-Registry-Paar (Nickname + zugeordneter
///    manueller Host),
/// 2. `--virtualcam <adresse>` → erster registrierter Host + die Adresse
///    (Skripting; die Registry speichert keine IPs),
/// 3. ohne Argument → erster manueller Host mit zugeordnetem registrierten
///    Host (der Autostart-Fall).
fn resolve_host(
    settings: &Settings,
    host_arg: Option<&str>,
) -> Result<(RegisteredHost, String), String> {
    let registered = settings.registered_hosts();
    let manual = settings.manual_hosts();

    let find_pair = |wanted: &dyn Fn(&RegisteredHost) -> bool| -> Option<(usize, usize)> {
        for (ri, r) in registered.iter().enumerate() {
            if !wanted(r) {
                continue;
            }
            if let Some((mi, _)) = manual
                .iter()
                .enumerate()
                .find(|(_, m)| m.registered && m.registered_mac.mac() == r.server_mac.mac())
            {
                return Some((ri, mi));
            }
        }
        None
    };

    if let Some(arg) = host_arg {
        if let Some((ri, mi)) =
            find_pair(&|r: &RegisteredHost| r.server_nickname.eq_ignore_ascii_case(arg))
        {
            return Ok((registered[ri].clone(), manual[mi].host.clone()));
        }
        // Kein Registry-Treffer → Adresse direkt benutzen (C++-CLI-`--host`-
        // Konvention). Der erste registrierte Host liefert die Credentials.
        let first = registered
            .first()
            .map(|r| (*r).clone())
            .ok_or_else(|| {
                format!(
                    "Keine registrierten Hosts in der Registry — „{arg}“ kann nicht aufgelöst werden"
                )
            })?;
        tracing::info!(
            "„{arg}“ ist kein Registry-Nickname — benutze registrierten Host „{}“ mit Adresse {arg}",
            first.server_nickname
        );
        return Ok((first, arg.to_string()));
    }

    find_pair(&|_: &RegisteredHost| true)
        .map(|(ri, mi)| (registered[ri].clone(), manual[mi].host.clone()))
        .ok_or_else(|| {
            "Kein zugeordneter manueller Host in der Host-Registry — --virtualcam <host>              mit Nickname oder IP angeben oder in der App (Consoles) einen manuellen Host anlegen"
                .to_string()
        })
}

/// Für den Media-Thread eingefrorene Settings (Session-Start).
struct MediaSnapshot {
    hw_backend: HwBackend,
    codec: chiaki_core::Codec,
    max_fps: u32,
    audio_out_device: Option<String>,
    audio_volume: u32,
    audio_buffer_size: u32,
    nv_vsr: bool,
    nv_vsr_scale: u32,
    nv_vsr_quality: Option<u32>,
    nv_vsr_sdk_path: Option<std::path::PathBuf>,
}

/// Media-Loop des Headless-Modus: alle queued Frames dekodieren (CPU-Transfer
/// → NV12 im Systemspeicher) → Kamera; Audio dekodieren → lokale Ausgabe.
fn media_loop(
    settings: MediaSnapshot,
    rx: std::sync::mpsc::Receiver<MediaCmd>,
    shared: Arc<Shared>,
    mut cam: CamFeed,
) {
    let mut decoder = match Decoder::new_opts(settings.codec, settings.hw_backend, settings.max_fps, DecoderOpts::default()) {
        Ok(d) => Some(d),
        Err(err) => {
            tracing::error!("Decoder-Init fehlgeschlagen ({err:?}) — nur Audio + kein Kamera-Feed");
            None
        }
    };
    // VSR (User-Vorgabe: die Kamera bekommt den VSR-Output). CPU-Transfer-
    // Dekode: der Frame liegt als CPU-NV12 vor, `process_frame` liefert den
    // skalierten Frame als kontiguierliches NV12 ([`FrameBuf`]) — derselbe
    // Pfad wie der GUI-CPU-Fallback. Init mit dem ersten Frame.
    let mut vsr = if settings.nv_vsr {
        Some(chiaki_media::VsrUpscaler::new(settings.nv_vsr_sdk_path.clone()))
    } else {
        None
    };
    let mut vsr_inited = false;
    let mut vsr_buf = chiaki_media::FrameBuf::new();
    let mut opus = OpusAudioDecoder::new();
    let mut audio_out: Option<AudioOutput> = None;
    let mut cam_frames_prev: u64 = 0;
    let mut log_tick: u32 = 0;

    loop {
        match rx.recv_timeout(Duration::from_millis(5)) {
            Ok(MediaCmd::AudioHeader(header)) => {
                if let Err(err) = opus.set_header(header) {
                    tracing::error!("OpusDecoder-Init fehlgeschlagen: {err:?}");
                    continue;
                }
                match AudioOutput::new(
                    settings.audio_out_device.as_deref(),
                    u32::from(header.rate),
                    u16::from(header.channels),
                    settings.audio_buffer_size,
                ) {
                    Ok(out) => {
                        out.set_volume(settings.audio_volume as f32 / 128.0);
                        tracing::info!("Audio-Ausgabe '{}' (lokal, Volume {}/128)", out.device_name(), settings.audio_volume);
                        audio_out = Some(out);
                    }
                    Err(err) => tracing::error!("AudioOutput-Init fehlgeschlagen: {err:?}"),
                }
            }
            Ok(MediaCmd::Audio(packet)) => {
                if let Ok(pcm) = opus.decode_frame(&packet) {
                    if let Some(out) = &audio_out {
                        out.push(pcm);
                    }
                }
            }
            Ok(MediaCmd::Video) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        while let Some((data, frames_lost, recovered)) = shared.pop_video() {
            let Some(decoder) = decoder.as_mut() else { break };
            match decoder.decode_sample(&data, frames_lost, recovered) {
                Ok(Some(frame)) => {
                    // CPU-Transfer-Pfad: NV12 liegt im Systemspeicher; GPU-
                    // Frames (raw-Output) gibt es hier bewusst nicht.
                    let Some((y, uv)) = frame.nv12_cpu_planes() else {
                        tracing::warn!("Headless: GPU-Frame im CPU-Pfad übersprungen");
                        continue;
                    };
                    // VSR einmalig mit dem ersten Frame initialisieren (wie
                    // GUI). Schlägt sie fehl, läuft der Feed mit den rohen
                    // Stream-Frames weiter (Kamera-Format passt dann nicht —
                    // der CamFeed zählt/loggt die abgewiesenen Frames).
                    if let (Some(up), false) = (&mut vsr, vsr_inited) {
                        let ctx = decoder
                            .cuda_context()
                            .unwrap_or(std::ptr::null_mut());
                        let stream = decoder.cuda_stream().unwrap_or(std::ptr::null_mut());
                        vsr_inited = up.init(
                            &frame,
                            ctx,
                            stream,
                            settings.nv_vsr_scale,
                            settings.nv_vsr_quality,
                        );
                        tracing::info!(
                            "VSR: {} ({:?})",
                            if vsr_inited { "aktiv" } else { "NICHT aktiv" },
                            up.last_error()
                        );
                    }
                    // POST-VSR-Frame feeden (User-Vorgabe: Kamera MIT VSR).
                    let fed = if let (Some(up), true) = (&mut vsr, vsr_inited) {
                        if up.process_frame(&frame, &mut vsr_buf) {
                            cam.push_nv12(
                                vsr_buf.width(),
                                vsr_buf.height(),
                                vsr_buf.y(),
                                vsr_buf.pitch(),
                                vsr_buf.uv(),
                                vsr_buf.pitch(),
                            );
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !fed {
                        cam.push_nv12(
                            frame.width,
                            frame.height,
                            y,
                            frame.planes[0].stride,
                            uv,
                            frame.planes[1].stride,
                        );
                    }
                    shared.cam_frames.store(cam.frames_sent(), Ordering::Relaxed);
                    shared.cam_errors.store(cam.errors(), Ordering::Relaxed);
                }
                Ok(None) => {}
                Err(err) => tracing::warn!("Decode-Fehler: {err:?}"),
            }
        }

        // 5-s-Statuslog (Frames-Rate + Feed-Bilanz).
        log_tick += 1;
        if log_tick >= 600 {
            log_tick = 0;
            let frames = shared.cam_frames.load(Ordering::Relaxed);
            tracing::info!(
                "Kamera: {} Frames gesendet (Δ+{}), {} Fehler, Slot-Drops {}",
                frames,
                frames.saturating_sub(cam_frames_prev),
                shared.cam_errors.load(Ordering::Relaxed),
                shared.video_dropped.load(Ordering::Relaxed),
            );
            cam_frames_prev = frames;
        }
    }
    // cam dropped hier → Kamera schließt (Mapping frei).
    tracing::info!("Headless-Media-Thread beendet");
}

/// tracing-Init des Headless-Modus: Logdatei `chiaki-virtualcam.log` im
/// üblichen Log-Ordner + stdout (bei Konsolenstart sichtbar). Level via
/// RUST_LOG, Default info.
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let file_layer = std::fs::create_dir_all(chiaki_settings::app_paths::log_dir())
        .ok()
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(chiaki_settings::app_paths::log_dir().join("chiaki-virtualcam.log"))
                .ok()
        })
        .map(|file| {
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(FileWriter(std::sync::Arc::new(file))))
        });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(file_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// `MakeWriter`-Adapter für eine geteilte Logdatei.
struct FileWriter(std::sync::Arc<std::fs::File>);

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
        FileWriter(std::sync::Arc::clone(&self.0))
    }
}
