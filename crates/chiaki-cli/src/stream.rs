//! `stream`-Subcommand (M1-Gate): baut eine Session über
//! `chiaki_core::session::Session` auf, loggt die Session-Events und schreibt
//! die ersten 100 decodierbaren H.264/H.265-Einheiten (komplett über den
//! FrameProcessor rekonstruierte Frames, Annexb, inkl. Profil-Header)
//! als `stream.h264`/`stream.h265` ins `--out-dir`.
//!
//! ConnectInfo-Mapping (wie der C++-Client in gui/src/main.cpp und wie es
//! die session.c intern beim PSN-Pfad macht):
//! - `regist_key` = `RegisteredHost.rp_regist_key` (16 Bytes, `\0`-gefüllt)
//! - `morning`    = `RegisteredHost.rp_key`       (16 Bytes)

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use chiaki_core::error::Target;
use chiaki_core::regist::{RegistInfo, RegisteredHost};
use chiaki_core::session::{
    connect_video_profile_preset_session, quit_reason_is_error, quit_reason_string,
    ConnectInfo, Session, SessionCallbacks, SessionEvent, VideoFpsPresetSession,
    VideoResolutionPresetSession,
};
use chiaki_core::takion::DisableAudioVideo;
use chiaki_core::Codec;
use chiaki_media::{Decoder, HwBackend};

use crate::regist;
use crate::util;

/// Anzahl der in die Datei geschriebenen Einheiten für das M1-Gate
/// („schreibt die ersten 100 decodierbaren H.264/H.265-Einheiten in Datei").
const M1_GATE_UNITS: u32 = 100;

/// Dateiname der Video-Ausgabe je Codec (Annexb-Rohdaten).
fn frame_file_name(codec: CodecArg) -> &'static str {
    match codec {
        CodecArg::H264 => "stream.h264",
        CodecArg::H265 => "stream.h265",
    }
}

/// Argumente von `chiaki-cli stream`.
#[derive(Debug, Clone, clap::Args)]
pub struct StreamArgs {
    /// Console-IP bzw. Hostname
    #[arg(long)]
    pub host: String,
    /// Regist-Key des registrierten Hosts (bis 16 Zeichen; =
    /// `rp_regist_key` aus der Host-Registry)
    #[arg(long, value_parser = util::parse_regist_key)]
    pub regist_key: Option<[u8; 16]>,
    /// Morning-Key des registrierten Hosts (32 Hex-Zeichen = 16 Bytes; =
    /// `rp_key` aus der Host-Registry)
    #[arg(long, value_parser = util::parse_morning_hex)]
    pub morning: Option<[u8; 16]>,
    /// Mit `--regist-key`+`--morning`: Console-Login-PIN, die bei
    /// LoginPinRequest gesendet wird. Ohne Keys: es wird zuerst der
    /// Regist-Flow mit dieser PIN ausgeführt (Auto-Regist).
    #[arg(long, value_parser = util::validate_pin_str)]
    pub pin: Option<String>,
    /// Regist-/Morning-Key statt direkt aus der settings.ini-Host-Registry
    /// lesen (Nickname des registrierten Hosts)
    #[arg(long)]
    pub registered: Option<String>,
    /// Video-Codec
    #[arg(long, value_enum, default_value = "h264")]
    pub codec: CodecArg,
    /// Auflösung
    #[arg(long, value_enum, default_value = "720")]
    pub resolution: ResolutionArg,
    /// Framerate
    #[arg(long, value_enum, default_value = "60")]
    pub fps: FpsArg,
    /// Bitrate in kbps (0 = Auto, Preset-Default der Auflösung)
    #[arg(long, default_value_t = 0)]
    pub bitrate: u32,
    /// Nach n empfangenen Video-Einheiten sauber beenden (Default: laufen,
    /// bis Ctrl+C oder die Session endet)
    #[arg(long)]
    pub frames: Option<u32>,
    /// Verzeichnis für stream.h264/stream.h265
    #[arg(long, default_value = ".")]
    pub out_dir: PathBuf,
    /// Console ist eine PS5 (Default: PS4)
    #[arg(long)]
    pub ps5: bool,
    /// Frames zusätzlich tatsächlich dekodieren (chiaki-media) und
    /// Frame-Metadaten loggen (benötigt die FFmpeg-DLLs)
    #[arg(long)]
    pub decode_test: bool,
    /// Konsole vor dem Trennen in den Ruhemodus versetzen (goto_bed über
    /// Ctrl, wie der Disconnect-Dialog der GUI)
    #[arg(long)]
    pub standby_on_exit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum CodecArg {
    H264,
    H265,
}

impl From<CodecArg> for Codec {
    fn from(v: CodecArg) -> Self {
        match v {
            CodecArg::H264 => Codec::H264,
            CodecArg::H265 => Codec::H265,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ResolutionArg {
    #[value(name = "720")]
    P720,
    #[value(name = "1080")]
    P1080,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FpsArg {
    #[value(name = "30")]
    Fps30,
    #[value(name = "60")]
    Fps60,
}

// ---------------------------------------------------------------------------
// Geteilter Zustand zwischen Callbacks (Session-Threads) und Hauptschleife
// ---------------------------------------------------------------------------

struct StreamState {
    quit: bool,
    quit_reason: Option<(chiaki_core::session::QuitReason, String)>,
    units_received: u32,
    units_written: u32,
    bytes_written: u64,
    file: Option<std::fs::File>,
    /// Nach so vielen Einheiten beenden (--frames), None = bis Ctrl+C/Quit.
    stop_after: Option<u32>,
    decoded_frames: u64,
}

impl StreamState {
    fn new(file: Option<std::fs::File>, stop_after: Option<u32>) -> Self {
        StreamState {
            quit: false,
            quit_reason: None,
            units_received: 0,
            units_written: 0,
            bytes_written: 0,
            file,
            stop_after,
            decoded_frames: 0,
        }
    }
}

struct StreamShared {
    state: Mutex<StreamState>,
    cond: Condvar,
    /// LoginPinRequest empfangen (Hauptschleife ruft dann set_login_pin).
    pin_requested: AtomicBool,
}

impl StreamShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, StreamState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn notify(&self) {
        self.cond.notify_all();
    }
}

// ---------------------------------------------------------------------------
// SessionCallbacks-Implementierung
// ---------------------------------------------------------------------------

struct StreamCallbacks {
    shared: Arc<StreamShared>,
    out_path: PathBuf,
    /// chiaki-media-Decoder für --decode-test (nicht Sync -> Mutex).
    decoder: Mutex<Option<Decoder>>,
}

impl StreamCallbacks {
    fn lock_decoder(&self) -> Option<std::sync::MutexGuard<'_, Option<Decoder>>> {
        Some(
            self.decoder
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}

impl SessionCallbacks for StreamCallbacks {
    /// C: video_sample_cb — `frame.data` ist der vom FrameProcessor
    /// rekonstruierte Frame (Annexb-Einheiten inkl. Profil-Header beim
    /// ersten Aufruf bzw. nach Profile-Wechsel), direkt in die Datei
    /// schreibbar und von FFmpeg dekodierbar (wie im C-ffmpegdecoder).
    fn video_frame(&self, frame: chiaki_core::session::VideoFrame<'_>) -> bool {
        // --decode-test: Frame tatsächlich dekodieren und Metadaten loggen.
        let mut decode_ok = true;
        if let Some(mut guard) = self.lock_decoder() {
            if let Some(decoder) = guard.as_mut() {
                match decoder.decode_packet(frame.data) {
                    Ok(Some(f)) => {
                        let first;
                        {
                            let mut st = self.shared.lock();
                            st.decoded_frames += 1;
                            first = st.decoded_frames == 1;
                        }
                        if first {
                            tracing::info!(
                                "Decode test: first frame {}x{}, hw backend {:?}, lost {}, recovered {}",
                                f.width,
                                f.height,
                                decoder.used_hw_backend(),
                                frame.frames_lost,
                                frame.frame_recovered
                            );
                        } else {
                            tracing::debug!(
                                "Decode test: frame {}x{}, lost {}, recovered {}",
                                f.width,
                                f.height,
                                frame.frames_lost,
                                frame.frame_recovered
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("Decode test: failed to decode frame: {e}");
                        decode_ok = false;
                    }
                }
            }
        }

        let mut st = self.shared.lock();
        st.units_received += 1;

        // M1-Gate: die ersten 100 decodierbaren Einheiten in Datei schreiben
        // (der Profil-Header SPS/PPS/VPS ist die erste davon).
        if st.units_written < M1_GATE_UNITS {
            if let Some(file) = st.file.as_mut() {
                match file.write_all(frame.data) {
                    Ok(()) => {
                        st.units_written += 1;
                        st.bytes_written += frame.data.len() as u64;
                        if st.units_written == 1 {
                            tracing::info!(
                                "Writing video units to {}",
                                self.out_path.display()
                            );
                        } else if st.units_written == M1_GATE_UNITS {
                            tracing::info!(
                                "M1 gate reached: wrote first {M1_GATE_UNITS} decodable units \
                                 ({} bytes) to {}",
                                st.bytes_written,
                                self.out_path.display()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to write video unit: {e}");
                    }
                }
            }
        }

        tracing::debug!(
            "video unit #{} received ({} bytes, lost {}, recovered {})",
            st.units_received,
            frame.data.len(),
            frame.frames_lost,
            frame.frame_recovered
        );

        if st.stop_after.is_some_and(|n| st.units_received >= n) {
            st.quit = true;
            self.shared.notify();
        }

        // Rückgabe wie im C: false = Frame verworfen -> corrupt-frame-Report
        // (Console sendet dann einen neuen Keyframe).
        decode_ok
    }

    fn audio_pcm(&self, pcm: &[u8]) {
        if pcm.is_empty() {
            tracing::trace!("audio concealment frame");
        } else {
            tracing::trace!("audio pcm: {} bytes", pcm.len());
        }
    }

    fn event(&self, ev: SessionEvent) {
        match ev {
            SessionEvent::Connected => {
                tracing::info!("Session connected");
            }
            SessionEvent::LoginPinRequest(pin_incorrect) => {
                if pin_incorrect {
                    tracing::warn!("The console reports the PIN entered before was incorrect");
                }
                tracing::info!("Console requests login PIN");
                self.shared.pin_requested.store(true, Ordering::SeqCst);
                self.shared.notify();
            }
            SessionEvent::Regist(host) => {
                tracing::info!(
                    "Auto registered with \"{}\" (regist_key \"{}\", morning {})",
                    host.server_nickname,
                    util::regist_key_string(&host.rp_regist_key),
                    hex::encode(host.rp_key)
                );
            }
            SessionEvent::NicknameReceived(nickname) => {
                tracing::info!("Server nickname: {nickname}");
            }
            SessionEvent::Quit { reason, reason_str } => {
                if quit_reason_is_error(reason) {
                    tracing::error!(
                        "Session quit: {}{}",
                        quit_reason_string(reason),
                        if reason_str.is_empty() {
                            String::new()
                        } else {
                            format!(" ({reason_str})")
                        }
                    );
                } else {
                    tracing::info!("Session quit: {}", quit_reason_string(reason));
                }
                let mut st = self.shared.lock();
                st.quit = true;
                st.quit_reason = Some((reason, reason_str));
                self.shared.notify();
            }
            SessionEvent::AudioStreamInfo(header) => {
                tracing::info!(
                    "Audio stream: {} Hz, {} channels, {} bits, frame size {}",
                    header.rate,
                    header.channels,
                    header.bits,
                    header.frame_size
                );
            }
            SessionEvent::VideoFecFailure {
                frame_index,
                idr_request_sent,
            } => {
                tracing::warn!(
                    "Video FEC failure for frame {frame_index} (idr request sent: {idr_request_sent})"
                );
            }
            SessionEvent::CantDisplay { cant } => {
                tracing::info!(
                    "Remote says the stream can {} be displayed",
                    if cant { "not" } else { "now" }
                );
            }
            // Rumble/Trigger-Effekte, LED, Player-Index, Motion-Reset,
            // Keyboard-Events etc. sind für den Headless-Test nur Debug.
            other => tracing::debug!("Session event: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Key-Beschaffung (direkt / Auto-Regist / Host-Registry)
// ---------------------------------------------------------------------------

/// (regist_key, morning) aus einem `RegisteredHost` — genau das Mapping, das
/// auch die App-Ebene beim Session-Aufbau verwenden wird (siehe Modul-Doku).
pub fn connect_keys_from_registered_host(
    host: &RegisteredHost,
) -> ([u8; 16], [u8; 16]) {
    (host.rp_regist_key, host.rp_key)
}

/// Liest (regist_key, morning) + PS5-Flag aus der settings.ini-Host-Registry
/// (so wie der C++-Client in gui/src/main.cpp `stream` ohne explizite Keys
/// bedient: morning = GetRPKey(), regist_key = GetRPRegistKey()).
fn keys_from_settings(nickname: &str) -> Result<([u8; 16], [u8; 16], bool), String> {
    let settings = chiaki_settings::Settings::open(None)
        .map_err(|e| format!("failed to open settings: {e}"))?;
    let host = settings
        .registered_hosts()
        .into_iter()
        .find(|h| h.server_nickname == nickname)
        .ok_or_else(|| format!("no registered host with nickname \"{nickname}\" found"))?;
    // chiaki_settings::RegisteredHost spiegelt die rp_*-Felder der
    // Host-Registry 1:1 (gleiche Semantik wie chiaki_core::regist::RegisteredHost).
    let keys = (host.rp_regist_key, host.rp_key);
    Ok((keys.0, keys.1, host.target.is_ps5()))
}

fn build_video_profile(args: &StreamArgs) -> chiaki_core::video::ConnectVideoProfile {
    let resolution = match args.resolution {
        ResolutionArg::P720 => VideoResolutionPresetSession::P720,
        ResolutionArg::P1080 => VideoResolutionPresetSession::P1080,
    };
    let fps = match args.fps {
        FpsArg::Fps30 => VideoFpsPresetSession::Fps30,
        FpsArg::Fps60 => VideoFpsPresetSession::Fps60,
    };
    let mut profile = connect_video_profile_preset_session(resolution, fps);
    profile.codec = args.codec.into();
    if args.bitrate > 0 {
        profile.bitrate = args.bitrate;
    }
    profile
}

// ---------------------------------------------------------------------------
// Kommando
// ---------------------------------------------------------------------------

pub fn run(args: StreamArgs) -> Result<(), String> {
    // --- Credentials besorgen (siehe Modul-Doku für das Mapping) ---
    let mut ps5 = args.ps5;
    let (regist_key, morning) = if args.regist_key.is_some() || args.morning.is_some() {
        let regist_key = args.regist_key.ok_or(
            "--morning given without --regist-key: pass both (regist_key = rp_regist_key, morning = rp_key hex)",
        )?;
        let morning = args.morning.ok_or(
            "--regist-key given without --morning: pass both (morning = rp_key as 32 hex chars)",
        )?;
        (regist_key, morning)
    } else if let Some(pin) = &args.pin {
        // M1-Pfad "stream --host <ip> --pin <8-stellig>": erst registrieren,
        // dann die Credentials aus dem Regist-Ergebnis übernehmen.
        let info = RegistInfo {
            target: if ps5 { Target::Ps5_1 } else { Target::Ps4_10 },
            host: args.host.clone(),
            broadcast: false,
            psn_online_id: None,
            psn_account_id: [0; chiaki_core::regist::PSN_ACCOUNT_ID_SIZE],
            pin: util::parse_pin(pin)?,
            console_pin: 0,
        };
        println!("Running regist with {} (PIN {pin}) first ...", args.host);
        let host = regist::run_regist(info)
            .map_err(|e| format!("Regist failed: {e}"))?;
        tracing::info!(
            "Regist succeeded: \"{}\" (rp-key {})",
            host.server_nickname,
            hex::encode(host.rp_key)
        );
        connect_keys_from_registered_host(&host)
    } else if let Some(nickname) = &args.registered {
        let (regist_key, morning, registry_ps5) = keys_from_settings(nickname)?;
        ps5 |= registry_ps5;
        (regist_key, morning)
    } else {
        return Err(
            "no credentials: pass --regist-key + --morning, --pin (auto-regist) or --registered <nickname>".to_owned(),
        );
    };

    // --- Ausgabedatei ---
    std::fs::create_dir_all(&args.out_dir)
        .map_err(|e| format!("failed to create out dir {}: {e}", args.out_dir.display()))?;
    let out_path = args.out_dir.join(frame_file_name(args.codec));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&out_path)
        .map_err(|e| format!("failed to open {}: {e}", out_path.display()))?;

    // --- Optionaler Decoder (--decode-test) ---
    let video_profile = build_video_profile(&args);
    let decoder = if args.decode_test {
        let dec = Decoder::new(args.codec.into(), HwBackend::Auto, video_profile.max_fps)
            .map_err(|e| format!("failed to create decoder for --decode-test: {e}"))?;
        Some(dec)
    } else {
        None
    };

    // --- Session aufbauen ---
    let shared = Arc::new(StreamShared {
        state: Mutex::new(StreamState::new(Some(file), args.frames)),
        cond: Condvar::new(),
        pin_requested: AtomicBool::new(false),
    });

    let callbacks = StreamCallbacks {
        shared: Arc::clone(&shared),
        out_path: out_path.clone(),
        decoder: Mutex::new(decoder),
    };

    // Login-PIN nur im Direkt-Pfad senden (nach Auto-Regist ist die
    // Regist-PIN verbraucht; der Console-Login-Passcode ist eine andere PIN).
    let login_pin = if args.regist_key.is_some() || args.registered.is_some() {
        args.pin.as_deref().map(str::as_bytes).map(<[u8]>::to_vec)
    } else {
        None
    };

    let connect_info = ConnectInfo {
        ps5,
        host: args.host.clone(),
        regist_key,
        morning,
        video_profile,
        video_profile_auto_downgrade: true,
        enable_keyboard: false,
        enable_dualsense: true,
        audio_video_disabled: DisableAudioVideo::NoneDisabled,
        // Regist wurde ggf. oben separat ausgeführt — die Session startet
        // mit fertigen Keys (C: connect_info.auto_regist nur im PSN-Pfad).
        auto_regist: false,
        holepunch_session: None,
        rudp_sock: None,
        psn_account_id: [0; chiaki_core::regist::PSN_ACCOUNT_ID_SIZE],
        packet_loss_max: 0.0,
        enable_idr_on_fec_failure: true,
        av_reorder_timeout_us: 0,
    };

    println!(
        "Connecting to {} ({}, {}x{}@{}, {}) ...",
        args.host,
        if ps5 { "PS5" } else { "PS4" },
        video_profile.width,
        video_profile.height,
        video_profile.max_fps,
        match args.codec {
            CodecArg::H264 => "H264",
            CodecArg::H265 => "H265",
        }
    );

    let mut session = Session::new(connect_info, Arc::new(callbacks))
        .map_err(|e| format!("failed to create session: {e}"))?;
    session
        .start()
        .map_err(|e| format!("failed to start session: {e}"))?;

    // --- Hauptschleife: Events/Ctrl+C abwarten, Login-PIN nachliefern ---
    let mut pin_sent = false;
    let mut fatal: Option<String> = None;
    let quit_reason = loop {
        if util::ctrl_c_received() {
            tracing::info!("Ctrl+C, stopping session");
            break None;
        }

        // LoginPinRequest -> PIN an die Session übergeben (C:
        // chiaki_session_set_login_pin weckt den session_thread).
        if shared.pin_requested.swap(false, Ordering::SeqCst) && !pin_sent {
            pin_sent = true;
            match &login_pin {
                Some(pin) => {
                    if let Err(e) = session.set_login_pin(pin) {
                        fatal = Some(format!("failed to send login pin: {e}"));
                        break None;
                    }
                }
                None => tracing::error!(
                    "The console requests a login PIN, but none is available. \
                     Pass --pin together with --regist-key/--morning (or --registered)."
                ),
            }
        }

        let st = shared.lock();
        if st.quit {
            break st.quit_reason.clone();
        }
        let (st, _) = shared
            .cond
            .wait_timeout(st, Duration::from_millis(100))
            .unwrap_or_else(PoisonError::into_inner);
        if st.quit {
            break st.quit_reason.clone();
        }
    };

    // --- Sauberes Herunterfahren (Ctrl+C-Regel: stop + join) ---
    if args.standby_on_exit && fatal.is_none() && quit_reason.is_none() {
        // goto_bed läuft über den laufenden Ctrl-Kanal; kurz warten, damit
        // die Nachricht raus ist, bevor die Session gestoppt wird.
        match session.goto_bed() {
            Ok(()) => {
                tracing::info!("goto_bed sent, waiting 1 s before disconnect");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => tracing::warn!("goto_bed failed: {e}"),
        }
    }
    session.stop();
    session
        .join()
        .map_err(|e| format!("failed to join session: {e}"))?;
    session.fini();

    if let Some(e) = fatal {
        return Err(e);
    }

    let st = shared.lock();
    println!();
    println!(
        "Session ended. reason: {}",
        match &quit_reason {
            Some((reason, reason_str)) => {
                let s = quit_reason_string(*reason);
                if reason_str.is_empty() {
                    s.to_owned()
                } else {
                    format!("{s} ({reason_str})")
                }
            }
            None => "Stopped (Ctrl+C or --frames)".to_owned(),
        }
    );
    println!(
        "Video units received: {}, written: {} ({} bytes) to {}",
        st.units_received,
        st.units_written,
        st.bytes_written,
        out_path.display()
    );
    if st.decoded_frames > 0 {
        println!("Frames decoded (--decode-test): {}", st.decoded_frames);
    }

    match quit_reason {
        Some((reason, _)) if quit_reason_is_error(reason) => {
            Err(format!("session ended with error: {}", quit_reason_string(reason)))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chiaki_core::session::{build_session_request, format_regist_key_hex, session_request_path};

    /// ConnectInfo-Mapping aus RegisteredHost (App-Ebene!):
    /// regist_key = rp_regist_key, morning = rp_key.
    #[test]
    fn keys_from_registered_host_mapping() {
        let mut host = RegisteredHost::default();
        host.rp_regist_key = *b"0123456789abcdef";
        host.rp_key = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f];
        let (regist_key, morning) = connect_keys_from_registered_host(&host);
        assert_eq!(regist_key, host.rp_regist_key);
        assert_eq!(morning, host.rp_key);
        assert_eq!(regist_key.len(), chiaki_core::regist::SESSION_AUTH_SIZE);
        assert_eq!(morning.len(), 16);
    }

    /// session_request-Header-Builder via chiaki-core: golden gegen das
    /// C-Format `session_request_fmt` (session.c).
    #[test]
    fn session_request_golden() {
        assert_eq!(session_request_path(Target::Ps5_1), "/sie/ps5/rp/sess/init");
        assert_eq!(session_request_path(Target::Ps4_10), "/sie/ps4/rp/sess/init");
        assert_eq!(session_request_path(Target::Ps4_8), "/sce/rp/session");

        let req = build_session_request(
            "/sie/ps5/rp/sess/init",
            "192.168.0.10",
            9295,
            "00112233445566778899aabbccddeeff",
            "1.0",
        );
        assert_eq!(
            req,
            "GET /sie/ps5/rp/sess/init HTTP/1.1\r\n\
             Host: 192.168.0.10:9295\r\n\
             User-Agent: remoteplay Windows\r\n\
             Connection: close\r\n\
             Content-Length: 0\r\n\
             RP-Registkey: 00112233445566778899aabbccddeeff\r\n\
             Rp-Version: 1.0\r\n\
             \r\n"
        );
    }

    /// format_regist_key_hex (C: format_hex) — lowercase, ohne Trenner.
    #[test]
    fn regist_key_hex_format() {
        assert_eq!(format_regist_key_hex(&[0x0a, 0x00, 0xff]), "0a00ff");
        assert_eq!(format_regist_key_hex(&[]), "");
    }

    /// Video-Profil-Bau: Preset-Tabelle + Codec/Bitrate-Override.
    #[test]
    fn video_profile_preset_and_overrides() {
        let mk = |codec: CodecArg, res: ResolutionArg, fps: FpsArg, bitrate: u32| {
            build_video_profile(&StreamArgs {
                host: String::new(),
                regist_key: None,
                morning: None,
                pin: None,
                registered: None,
                codec,
                resolution: res,
                fps,
                bitrate,
                frames: None,
                out_dir: PathBuf::from("."),
                ps5: false,
                decode_test: false,
                standby_on_exit: false,
            })
        };

        let p = mk(CodecArg::H264, ResolutionArg::P720, FpsArg::Fps30, 0);
        assert_eq!((p.width, p.height, p.max_fps, p.bitrate), (1280, 720, 30, 10000));
        assert_eq!(p.codec, Codec::H264);

        let p = mk(CodecArg::H265, ResolutionArg::P1080, FpsArg::Fps60, 20000);
        assert_eq!((p.width, p.height, p.max_fps, p.bitrate), (1920, 1080, 60, 20000));
        assert_eq!(p.codec, Codec::H265);

        // 0 = Auto -> Preset-Bitrate der Auflösung.
        let p = mk(CodecArg::H264, ResolutionArg::P1080, FpsArg::Fps60, 0);
        assert_eq!(p.bitrate, 15000);
    }

    /// Frame-Dateiname je Codec.
    #[test]
    fn frame_file_name_by_codec() {
        assert_eq!(super::frame_file_name(CodecArg::H265), "stream.h265");
        assert_eq!(super::frame_file_name(CodecArg::H264), "stream.h264");
    }
}
