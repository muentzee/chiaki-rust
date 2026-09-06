//! Settings-Kategorie „Audio & Latency“ (QML: `SettingsAudio.qml`): Geräte
//! (Laufzeit-Enumeration über chiaki-media `AudioOutput::devices()` /
//! `AudioInput::devices()`), Puffer, Lautstärke, Mikrofon/Speech-Processing,
//! WiFi-/Packet-Loss-Hinweise, Reorder-Timeout.

use chiaki_media::audio::{AudioInput, AudioOutput};

use crate::app::AppShell;

use super::{action_row, inactive, info_row, select_row, slider_row, toggle_row, Section};

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    use super::SettingsUiState;

    // Geräte-Enumeration zur Laufzeit (gecacht; „Refresh devices“ leert den
    // Cache — Enumeration pro Frame wäre zu teuer).
    {
        let needs_scan = cx
            .try_global::<SettingsUiState>()
            .map(|s| s.audio_out_devices.is_empty() && s.audio_in_devices.is_empty())
            .unwrap_or(true);
        if needs_scan {
            let out = AudioOutput::devices();
            let input = AudioInput::devices();
            let state = cx.global_mut::<SettingsUiState>();
            state.audio_out_devices = out;
            state.audio_in_devices = input;
        }
    }
    let (out_devices, in_devices) = cx
        .try_global::<SettingsUiState>()
        .map(|s| (s.audio_out_devices.clone(), s.audio_in_devices.clone()))
        .unwrap_or_default();

    let settings = shell.backend.settings().clone();
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());

    let out_current = s.audio_out_device();
    let in_current = s.audio_in_device();
    let buffer_raw = s.audio_buffer_size_raw();
    let buffer = s.audio_buffer_size();
    let volume = s.audio_volume();
    let mic_unmuted = s.start_mic_unmuted();
    let speech = s.speech_processing_enabled();
    let noise = s.noise_suppress_level();
    let echo = s.echo_suppress_level();
    let wifi = s.wifi_dropped_notif();
    let loss = s.packet_loss_reported_max();
    let reorder = s.reorder_timeout_ms();
    let idr = s.idr_on_fec_failure_enabled();
    drop(s);

    // „Auto“ + gefundene Geräte (Wert = Geräte-Name, „“ = Auto).
    let out_options = device_options(&out_devices);
    let in_options = device_options(&in_devices);

    let mut devices = Section::new("Devices");
    devices.push(select_row(
        "audio-out-device",
        "Output device",
        Some("Where the console audio is played"),
        "speaker headphones output sound",
        true,
        out_options,
        &out_current,
        |v, s| s.set_audio_out_device(v.to_string()),
    ));
    devices.push(select_row(
        "audio-in-device",
        "Input device",
        Some("Microphone sent to the console"),
        "microphone mic input voice",
        true,
        in_options,
        &in_current,
        |v, s| s.set_audio_in_device(v.to_string()),
    ));
    devices.push(action_row(
        "audio-refresh-devices",
        "Refresh devices",
        Some("Re-enumerate the system audio devices"),
        "refresh audio devices scan",
        true,
        "Refresh",
        false,
        |_shell, cx| {
            let state = cx.global_mut::<SettingsUiState>();
            state.audio_out_devices.clear();
            state.audio_in_devices.clear();
        },
    ));

    let mut audio = Section::new("Audio");
    audio.push(slider_row(
        "audio-buffer-size",
        "Audio buffer size",
        Some("Smaller = lower latency, more prone to crackling"),
        "buffer latency delay",
        true,
        (if buffer_raw != 0 { buffer_raw } else { 9600 }) as f64 / 1920.0,
        1.0,
        10.0,
        1.0,
        format!("{} ms", buffer * 10 / 1920),
        |v, s| s.set_audio_buffer_size((v.round() as u64).max(1) * 1920),
    ));
    audio.push(slider_row(
        "audio-volume",
        "Audio volume",
        None,
        "volume loudness",
        true,
        volume as f64,
        0.0,
        128.0,
        1.0,
        format!("{} %", (volume as f64 / 128.0 * 100.0).round() as i64),
        |v, s| s.set_audio_volume(v.round() as i64),
    ));
    audio.push(toggle_row(
        "audio-start-mic-unmuted",
        "Start microphone unmuted",
        None,
        "mic voice chat",
        true,
        mic_unmuted,
    ));
    audio.push(inactive(toggle_row(
        "audio-speech-processing",
        "Speech processing",
        Some("Noise suppression and echo cancellation for the microphone"),
        "speex noise echo cancel",
        true,
        speech,
    ), "Speex-DSP nicht portiert"));
    audio.push(inactive(slider_row(
        "audio-noise-suppress",
        "Noise to suppress",
        None,
        "noise suppression db",
        speech,
        noise as f64,
        0.0,
        60.0,
        1.0,
        format!("{noise} dB (default 6 dB)"),
        |v, s| s.set_noise_suppress_level(v.round() as i64),
    ), "Speex-DSP nicht portiert"));
    audio.push(inactive(slider_row(
        "audio-echo-suppress",
        "Echo to suppress",
        None,
        "echo cancellation db",
        speech,
        echo as f64,
        0.0,
        60.0,
        1.0,
        format!("{echo} dB (default 30 dB)"),
        |v, s| s.set_echo_suppress_level(v.round() as i64),
    ), "Speex-DSP nicht portiert"));

    let mut network = Section::new("Network & Latency");
    network.push(inactive(slider_row(
        "audio-wifi-dropped",
        "Weak Wi-Fi notification",
        Some("Shows an indicator when packet loss exceeds this value"),
        "wifi packet loss indicator warning",
        true,
        wifi as f64,
        0.0,
        100.0,
        1.0,
        format!("\u{2265} {wifi} % dropped (default 3%)"),
        |v, s| s.set_wifi_dropped_notif(v.round() as u64),
    ), "Wi-Fi-Warnung im Stream-Overlay nicht portiert"));
    network.push(slider_row(
        "audio-packet-loss-max",
        "Packet loss reported max",
        None,
        "packet loss report",
        true,
        (loss * 100.0).round(),
        0.0,
        100.0,
        1.0,
        format!("{} % (default 5%)", (loss * 100.0).round() as i64),
        |v, s| s.set_packet_loss_reported_max(v.round() / 100.0),
    ));
    network.push(slider_row(
        "audio-reorder-timeout",
        "Reorder queue timeout",
        Some(
            "How long (ms) to wait for missing network packets. Lower = faster recovery after \
             packet loss, higher = more Wi-Fi jitter tolerance.",
        ),
        "jitter reorder buffer latency",
        true,
        reorder as f64,
        1.0,
        200.0,
        1.0,
        format!("{reorder} ms (default 16 ms)"),
        |v, s| s.set_reorder_timeout_ms(v.round() as i64),
    ));
    network.push(toggle_row(
        "audio-idr-on-fec-failure",
        "Request IDR frame on FEC failure",
        Some("Recovers sharper after corrupted packets, at the cost of a keyframe"),
        "keyframe recovery corruption",
        true,
        idr,
    ));

    let mut notes = Section::new("Notes");
    notes.push(info_row(
        "Audio buffer size 0 (= auto) is managed by the client; the slider always writes an \
         explicit value like the C++ UI.",
        "buffer auto default 9600",
    ));

    vec![devices, audio, network, notes]
}

/// „Auto“ (= leerer Wert) + Geräte-Liste.
fn device_options(devices: &[String]) -> Vec<crate::components::SelectOption> {
    let mut options = vec![crate::components::SelectOption::new("", "Auto")];
    for device in devices {
        options.push(crate::components::SelectOption::new(device.clone(), device.clone()));
    }
    options
}
