//! Settings-Kategorie „Stream“ (QML: `SettingsStream.qml`): Auflösung/FPS/
//! Bitrate/Codec je Konsole (PS4/PS5) und Richtung (Lokal = Playback auf
//! diesem PC, Remote = Playback über Internet). Bitrate resettet auf Auto (0),
//! wenn die Auflösung wechselt — wie im Alt-UI.

use chiaki_settings::settings::{Codec, ResolutionPreset};

use crate::app::AppShell;

use super::{info_row, opts, select_row, slider_row, ui_select_row, Section};

/// Preset-Bitrate je Auflösungs-Index (0=360p..3=1080p) in kbit/s.
const AUTO_BITRATES: [u64; 4] = [2000, 6000, 10000, 15000];

fn resolution_value(r: ResolutionPreset) -> &'static str {
    match r {
        ResolutionPreset::P360 => "360p",
        ResolutionPreset::P540 => "540p",
        ResolutionPreset::P720 => "720p",
        ResolutionPreset::P1080 => "1080p",
    }
}

fn resolution_index(value: &str) -> usize {
    match value {
        "360p" => 0,
        "540p" => 1,
        "720p" => 2,
        _ => 3,
    }
}

fn codec_value(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::H265Hdr => "h265_hdr",
    }
}

pub(crate) fn sections(
    shell: &mut AppShell,
    _needle: &str,
    cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    use super::SettingsUiState;
    let is_ps5 = cx
        .try_global::<SettingsUiState>()
        .map(|s| s.stream_target_ps5)
        .unwrap_or(true);

    let s = shell.backend.settings().lock().unwrap_or_else(|e| e.into_inner());
    let (res_local, res_remote) = if is_ps5 {
        (s.resolution_local_ps5(), s.resolution_remote_ps5())
    } else {
        (s.resolution_local_ps4(), s.resolution_remote_ps4())
    };
    let (fps_local, fps_remote) = if is_ps5 {
        (s.fps_local_ps5(), s.fps_remote_ps5())
    } else {
        (s.fps_local_ps4(), s.fps_remote_ps4())
    };
    let (bitrate_local, bitrate_remote) = if is_ps5 {
        (s.bitrate_local_ps5(), s.bitrate_remote_ps5())
    } else {
        (s.bitrate_local_ps4(), s.bitrate_remote_ps4())
    };
    let codec_local = if is_ps5 { codec_value(s.codec_local_ps5()) } else { "" };
    let codec_remote = if is_ps5 { codec_value(s.codec_remote_ps5()) } else { "" };
    let opengl = s.render_backend() == chiaki_settings::settings::RenderBackend::OpenGL;
    drop(s);

    // Auflösungs-Labels wie im QML (mit Default-Hinweis je Richtung/Target).
    let res_options_local: Vec<_> = if is_ps5 {
        opts(&[
            ("360p", "360p"),
            ("540p", "540p"),
            ("720p", "720p"),
            ("1080p", "1080p (Default)"),
        ])
    } else {
        opts(&[
            ("360p", "360p"),
            ("540p", "540p"),
            ("720p", "720p (Default)"),
            ("1080p", "1080p (PS4 Pro)"),
        ])
    };
    let res_options_remote: Vec<_> = if is_ps5 {
        opts(&[("360p", "360p"), ("540p", "540p"), ("720p", "720p (Default)"), ("1080p", "1080p")])
    } else {
        res_options_local.clone()
    };

    let codec_options: Vec<_> = if opengl {
        opts(&[("h264", "H264"), ("h265", "H265 (Default)")])
    } else {
        opts(&[("h264", "H264"), ("h265", "H265 (Default)"), ("h265_hdr", "H265 HDR")])
    };

    let mut target = Section::new("Target");
    target.push(ui_select_row(
        "stream-target",
        "Settings for",
        None,
        "console ps4 ps5",
        true,
        opts(&[("ps4", "PS4"), ("ps5", "PS5")]),
        if is_ps5 { "ps5" } else { "ps4" },
        |v, state| state.stream_target_ps5 = v == "ps5",
    ));

    let mut local = Section::new("Local (LAN)");
    let set_local_res = move |v: &str, s: &mut chiaki_settings::settings::Settings| {
        let r = match v {
            "360p" => ResolutionPreset::P360,
            "540p" => ResolutionPreset::P540,
            "1080p" => ResolutionPreset::P1080,
            _ => ResolutionPreset::P720,
        };
        if is_ps5 {
            s.set_resolution_local_ps5(r);
            s.set_bitrate_local_ps5(0); // Auflösungswechsel → Bitrate auf Auto
        } else {
            s.set_resolution_local_ps4(r);
            s.set_bitrate_local_ps4(0);
        }
    };
    local.push(select_row(
        "stream-res-local",
        "Resolution",
        None,
        "resolution 360 540 720 1080",
        true,
        res_options_local,
        resolution_value(res_local),
        set_local_res,
    ));
    local.push(select_row(
        "stream-fps-local",
        "FPS",
        None,
        "framerate 30 60",
        true,
        opts(&[("30", "30 fps"), ("60", "60 fps (Default)")]),
        if fps_local == chiaki_settings::settings::FpsPreset::Fps30 { "30" } else { "60" },
        move |v, s| {
            let fps = if v == "30" {
                chiaki_settings::settings::FpsPreset::Fps30
            } else {
                chiaki_settings::settings::FpsPreset::Fps60
            };
            if is_ps5 {
                s.set_fps_local_ps5(fps);
            } else {
                s.set_fps_local_ps4(fps);
            }
        },
    ));
    local.push(bitrate_row(
        "stream-bitrate-local",
        bitrate_local,
        AUTO_BITRATES[resolution_index(resolution_value(res_local))],
        is_ps5,
    ));
    local.push(select_row(
        "stream-codec-local",
        "Codec",
        None,
        "codec h264 h265 hdr hevc",
        is_ps5, // Codec-Row nur für PS5 (wie QML)
        codec_options.clone(),
        codec_local,
        |v, s| {
            let codec = match v {
                "h265" => Codec::H265,
                "h265_hdr" => Codec::H265Hdr,
                _ => Codec::H264,
            };
            s.set_codec_local_ps5(codec);
        },
    ));

    let mut remote = Section::new("Remote (Internet / Remote Play)");
    remote.push(select_row(
        "stream-res-remote",
        "Resolution",
        None,
        "resolution 360 540 720 1080 remote",
        true,
        res_options_remote,
        resolution_value(res_remote),
        move |v, s| {
            let r = match v {
                "360p" => ResolutionPreset::P360,
                "540p" => ResolutionPreset::P540,
                "1080p" => ResolutionPreset::P1080,
                _ => ResolutionPreset::P720,
            };
            if is_ps5 {
                s.set_resolution_remote_ps5(r);
                s.set_bitrate_remote_ps5(0);
            } else {
                s.set_resolution_remote_ps4(r);
                s.set_bitrate_remote_ps4(0);
            }
        },
    ));
    remote.push(select_row(
        "stream-fps-remote",
        "FPS",
        None,
        "framerate 30 60 remote",
        true,
        opts(&[("30", "30 fps"), ("60", "60 fps (Default)")]),
        if fps_remote == chiaki_settings::settings::FpsPreset::Fps30 { "30" } else { "60" },
        move |v, s| {
            let fps = if v == "30" {
                chiaki_settings::settings::FpsPreset::Fps30
            } else {
                chiaki_settings::settings::FpsPreset::Fps60
            };
            if is_ps5 {
                s.set_fps_remote_ps5(fps);
            } else {
                s.set_fps_remote_ps4(fps);
            }
        },
    ));
    remote.push(bitrate_row(
        "stream-bitrate-remote",
        bitrate_remote,
        AUTO_BITRATES[resolution_index(resolution_value(res_remote))],
        is_ps5,
    ));
    remote.push(select_row(
        "stream-codec-remote",
        "Codec",
        None,
        "codec h264 h265 hdr hevc remote",
        is_ps5,
        codec_options,
        codec_remote,
        |v, s| {
            let codec = match v {
                "h265" => Codec::H265,
                "h265_hdr" => Codec::H265Hdr,
                _ => Codec::H264,
            };
            s.set_codec_remote_ps5(codec);
        },
    ));

    let mut notes = Section::new("Notes");
    notes.push(info_row(
        "\u{201C}Local\u{201D} applies when streaming inside the home network, \
         \u{201C}Remote\u{201D} when streaming over the internet (PSN remote play). \
         Lower bitrates survive worse connections.",
        "local remote explanation lan internet",
    ));

    vec![target, local, remote, notes]
}

/// Bitrate-Slider (2..100 Mbps, Anzeige „Auto (n Mbps)“ bei 0).
fn bitrate_row(id: &'static str, current_kbps: u64, auto_kbps: u64, is_ps5: bool) -> super::SRow {
    let value = if current_kbps != 0 { current_kbps } else { auto_kbps } as f64 / 1000.0;
    let value_text = if current_kbps != 0 {
        format!("{} Mbps", current_kbps / 1000)
    } else {
        format!("Auto ({} Mbps)", auto_kbps / 1000)
    };
    slider_row(
        id,
        "Bitrate",
        None,
        "bitrate quality mbps auto",
        true,
        value,
        2.0,
        100.0,
        1.0,
        value_text,
        move |v, s| {
            let kbps = (v * 1000.0).round() as u64;
            if is_ps5 {
                s.set_bitrate_local_ps5(kbps);
            } else {
                s.set_bitrate_local_ps4(kbps);
            }
        },
    )
}
