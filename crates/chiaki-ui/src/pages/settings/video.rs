//! Settings-Kategorie „Video“ (QML: `SettingsVideo.qml` + v1-Dialoge
//! `DisplaySettingsDialog.qml` / `PlaceboSettingsDialog.qml`): Fenster,
//! Rendering/Decoder, NVIDIA VSR, Stream-Overlay, Display-Feintuning und
//! libplacebo-Renderparameter (`placebo_render_params.ini`, Sektion
//! `placebo_settings` — schreibbar über die chiaki-settings-API).

use chiaki_settings::settings::{
    Decoder, PlaceboColorAdjustmentPreset, PlaceboColorMappingPreset,
    PlaceboDebandPreset, PlaceboDeinterlaceAlgorithm, PlaceboDownscaler,
    PlaceboFrameMixer, PlaceboGamutMappingFunction, PlaceboPeakDetectionPreset,
    PlaceboPreset, PlaceboSigmoidPreset, PlaceboToneMappingFunction,
    PlaceboToneMappingMetadata, PlaceboUpscaler, RenderBackend, Settings, WindowType,
};

use gpui::IntoElement as _;

use crate::app::AppShell;
use crate::components::SelectOption;

use super::{
    inactive, opts, select_row, slider_row, text_row, toggle_row, Section, SRow,
};

pub(crate) fn sections(
    shell: &mut AppShell,
    needle: &str,
    cx: &mut gpui::Context<AppShell>,
) -> Vec<Section> {
    use super::SettingsUiState;
    let advanced_open = cx
        .try_global::<SettingsUiState>()
        .map(|s| s.advanced_open)
        .unwrap_or(false);
    let searching = !needle.is_empty();

    // Arc klonen, damit der Guard nicht an `shell` borrows.
    let settings = shell.backend.settings().clone();
    let s = settings.lock().unwrap_or_else(|e| e.into_inner());

    let window_custom = s.window_type() == WindowType::CustomResolution;

    // Info-Banner (nur wenn die Video-Kategorie aktiv und keine Suche
    // aktiv ist — bei Suche wird die Kategorie-Liste durchsucht, dort
    // wäre der Hinweis nur Lärm): Rust-Renderer rendert ohne libplacebo.
    let mut sections: Vec<Section> = Vec::new();
    if !searching {
        let mut banner = Section::new("Hinweis (Rust-Renderer)");
        banner.push(super::info_row(
            "Rendering-Fine-Tuning-Parameter (libplacebo) sind im Rust-Renderer ohne Funktion \
             und werden nur für die INI-Kompatibilität gespeichert.",
            "rust renderer libplacebo inaktiv hinweis",
        ));
        sections.push(banner);
    }

    let mut window = Section::new("Window");
    window.push(select_row(
        "video-window-type",
        "Window type",
        Some("How the stream window is sized and presented"),
        "fullscreen zoom stretch windowed resizable",
        true,
        opts(&[
            ("Selected Resolution", "Stream Resolution"),
            ("Custom Resolution", "Custom Resolution"),
            ("Adjust Manually", "Adjust Resolution Manually"),
            ("Fullscreen", "Fullscreen"),
            ("Zoom", "Zoom"),
            ("Stretch", "Stretch"),
        ]),
        window_type_value(s.window_type()),
        |v, s: &mut Settings| {
            s.set_window_type(match v {
                "Selected Resolution" => WindowType::SelectedResolution,
                "Custom Resolution" => WindowType::CustomResolution,
                "Adjust Manually" => WindowType::AdjustableResolution,
                "Fullscreen" => WindowType::Fullscreen,
                "Zoom" => WindowType::Zoom,
                _ => WindowType::Stretch,
            });
        },
    ));
    // Custom-Auflösung: nur bei window_type = Custom Resolution wirksam —
    // dann nutzt das Connect-Video-Profil width/height statt des Presets
    // (geklemt 360p..4K, gerade Maße; siehe backend::sessions).
    window.push(text_row(
        "video-custom-width",
        "Custom resolution width",
        Some(
            "Stream-Auflösung bei Window type = Custom Resolution (geklemt 640–3840, \
             gerade Maße) — wirksam beim nächsten Session-Start",
        ),
        "width pixels",
        window_custom,
        s.custom_resolution_width().to_string(),
        "1920",
        focus_for(cx, "video-custom-width"),
        |v, s| {
            let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
            s.set_custom_resolution_width(digits.parse().unwrap_or(0));
        },
    ));
    window.push(text_row(
        "video-custom-height",
        "Custom resolution height",
        Some(
            "Stream-Auflösung bei Window type = Custom Resolution (geklemt 360–2160, \
             gerade Maße) — wirksam beim nächsten Session-Start",
        ),
        "height pixels",
        window_custom,
        s.custom_resolution_height().to_string(),
        "1080",
        focus_for(cx, "video-custom-height"),
        |v, s| {
            let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
            s.set_custom_resolution_height(digits.parse().unwrap_or(0));
        },
    ));
    // Doppelklick auf die Video-Fläche → Vollbild-Toggle (Stream-Seite,
    // ClickEvent.click_count ≥ 2 — wie der C++-Double-Click-Handler).
    window.push(toggle_row(
        "video-fullscreen-doubleclick",
        "Toggle fullscreen on double-click",
        Some("Doppelklick in den Stream wechselt zwischen Fenster und Vollbild"),
        "double click fullscreen",
        true,
        s.fullscreen_double_click_enabled(),
    ));
    // Cursor-Hiding: im Stream wird der Mauszeiger über der Video-Fläche
    // versteckt (Stream-Seite, CursorStyle::None), solange gestreamt wird.
    window.push(toggle_row(
        "video-hide-cursor",
        "Hide cursor during stream",
        Some("Versteckt den Mauszeiger über der Video-Fläche während des Streams"),
        "mouse pointer",
        true,
        s.hide_cursor(),
    ));
    // Benutzerdefinierter Zoom (settings/zoom_factor): > 0 startet den Stream
    // im Zoom-Modus mit Fit-Skala × Faktor; linker Anschlag (−1) = Auto/aus.
    let zoom_factor = s.zoom_factor();
    window.push(slider_row(
        "video-zoom-factor",
        "Zoom factor",
        Some(
            "Benutzerdefinierter Zoom beim Stream-Start (passend × Faktor) — \
             linker Anschlag = Auto/aus",
        ),
        "zoom scale content custom",
        true,
        if zoom_factor > 0.0 { zoom_factor } else { -1.0 },
        -1.0,
        3.0,
        0.25,
        if zoom_factor > 0.0 {
            format!("{} %", (zoom_factor * 100.0).round() as i64)
        } else {
            "Auto/aus".to_string()
        },
        |v, s| {
            // Alles unter 1.0 gilt als Auto/aus (Key wird auf -1 gesetzt).
            s.set_zoom_factor(if v < 1.0 { -1.0 } else { (v * 100.0).round() / 100.0 });
        },
    ));

    let backend_opengl = s.render_backend() == RenderBackend::OpenGL;
    let mut rendering = Section::new("Rendering");
    // Video-Ausgabe (GPU-Pfad, settings/video_output): "gpu" erzwingt das
    // D3D11-Sink-Fenster (Zero-Copy), "cpu" den Kompatibilitäts-Pfad,
    // "auto" nur bei Zero-Copy-Kombination — wirksam beim nächsten
    // Session-Start.
    rendering.push(select_row(
        "video-output",
        "Video-Ausgabe",
        Some(
            "gpu = D3D11-Zero-Copy (empfohlen), cpu = Kompatibilitätspfad, \
             auto = automatisch — wirksam beim nächsten Session-Start",
        ),
        "video output gpu cpu d3d11 zero copy sink",
        true,
        opts(&[
            ("auto", "Auto"),
            ("gpu", "GPU (D3D11-Zero-Copy)"),
            ("cpu", "CPU (Kompatibilität)"),
        ]),
        &s.video_output(),
        |v, s| s.set_video_output(v),
    ));
    rendering.push(select_row(
        "video-hw-decoder",
        "Hardware decoder",
        Some("Auto picks the fastest decoder for this GPU"),
        "nvdec cuda d3d11 vaapi hardware decoder",
        true,
        opts(&[
            ("auto", "Auto"),
            ("cuda", "NVDEC (CUDA)"),
            ("d3d11va", "D3D11VA"),
            ("vulkan", "Vulkan Video"),
            ("none", "None (software)"),
        ]),
        &s.hw_decoder(),
        |v, s| s.set_hardware_decoder(v.to_string()),
    ));
    rendering.push(inactive(
        select_row(
            "video-decoder",
            "Decoder",
            Some("Stream decoder implementation"),
            "decoder ffmpeg pi software",
            true,
            opts(&[("ffmpeg", "FFmpeg"), ("pi", "Pi (stored for compatibility)")]),
            decoder_value(s.decoder()),
            |v, s| {
                s.set_decoder(if v == "pi" { Decoder::Pi } else { Decoder::Ffmpeg });
            },
        ),
        "Pi-Decoder nicht portiert — es läuft immer FFmpeg",
    ));
    // vsync: steuert das Present-Interval des D3D11-Sink-Fensters (GPU-Pfad,
    // wirksam beim Session-Start). Aus = Present(0) ohne Sync (niedrigste
    // Latenz); an = SyncInterval 1 — der Inhaltswechsel rückt auf Vblank-
    // Grenzen (gleichmäßige Kadenz, +bis 1 Refresh-Intervall Latenz).
    rendering.push(toggle_row(
        "video-vsync",
        "Vertical sync",
        Some(
            "An = Bildwechsel am Display-Takt (gleichmäßiger, +etwas Latenz); \
             aus = niedrigste Latenz. Wirksam beim nächsten Session-Start \
             (GPU-Videopfad)",
        ),
        "vsync tearing fluent sync interval",
        true,
        s.vsync_enabled(),
    ));
    rendering.push(inactive(
        select_row(
            "video-render-backend",
            "Renderer backend",
            Some("Changing the backend restarts the application"),
            "vulkan opengl gpu",
            true,
            opts(&[("vulkan", "Vulkan"), ("opengl", "OpenGL")]),
            if backend_opengl { "opengl" } else { "vulkan" },
            |v, s| {
                s.set_render_backend(if v == "opengl" {
                    RenderBackend::OpenGL
                } else {
                    RenderBackend::Vulkan
                });
            },
        ),
        "Rendern läuft immer über GPUI (D3D11) — der Key beeinflusst nur die HDR-Codec-Wahl",
    ));
    rendering.push(inactive(
        toggle_row(
            "video-vulkan-deferred-swap",
            "Vulkan deferred swap",
            None,
            "swapchain",
            !backend_opengl,
            s.vulkan_deferred_swap(),
        ),
        "Kein Vulkan-Swapchain im Rust-Renderer",
    ));
    rendering.push(inactive(
        select_row(
            "video-render-preset",
            "Render preset",
            Some("Quality presets for scaling and sharpness"),
            "quality upscaling preset placebo",
            true,
            opts(&[
                ("fast", "Fast"),
                ("default", "Default"),
                ("high_quality", "High Quality"),
                ("high_quality_spatial", "High Quality + Spatial"),
                ("high_quality_advanced_spatial", "High Quality + Adv Spatial"),
                ("custom", "Custom"),
            ]),
            preset_value(s.placebo_preset()),
            |v, s| {
                s.set_placebo_preset(match v {
                    "fast" => PlaceboPreset::Fast,
                    "default" => PlaceboPreset::Default,
                    "high_quality" => PlaceboPreset::HighQuality,
                    "high_quality_spatial" => PlaceboPreset::HighQualitySpatial,
                    "high_quality_advanced_spatial" => PlaceboPreset::HighQualityAdvancedSpatial,
                    _ => PlaceboPreset::Custom,
                });
            },
        ),
        "libplacebo nicht portiert — steuert nur die Sichtbarkeit der Fine-Tuning-Rows",
    ));
    rendering.push(inactive(
        select_row(
            "video-frame-mixer",
            "Frame mixer",
            Some("Interpolation of consecutive frames (motion smoothness)"),
            "interpolation motion",
            true,
            opts(&[
                ("none", "None"),
                ("oversample", "Oversample"),
                ("hermite", "Hermite"),
                ("linear", "Linear"),
                ("cubic", "Cubic"),
            ]),
            frame_mixer_value(s.placebo_frame_mixer()),
            |v, s| {
                s.set_placebo_frame_mixer(match v {
                    "oversample" => PlaceboFrameMixer::Oversample,
                    "hermite" => PlaceboFrameMixer::Hermite,
                    "linear" => PlaceboFrameMixer::Linear,
                    "cubic" => PlaceboFrameMixer::Cubic,
                    _ => PlaceboFrameMixer::None,
                });
            },
        ),
        "libplacebo nicht portiert — nur INI-Kompatibilität",
    ));

    let vsr = s.nv_vsr_enabled();
    let mut vsr_section = Section::new("NVIDIA Video Super Resolution");
    vsr_section.push(toggle_row(
        "video-nv-vsr",
        "Enable NVIDIA VSR",
        Some("AI-upscaling of the stream \u{2014} sharp picture for viewers (e.g. Discord)"),
        "vsr ai upscale sharp rtx",
        true,
        vsr,
    ));
    vsr_section.push(select_row(
        "video-nv-vsr-scale",
        "Upscale factor",
        Some("Auto targets the display resolution, like browser VSR"),
        "scale factor resolution",
        vsr,
        opts(&[
            ("0", "Auto (Display)"),
            ("150", "1.5x"),
            ("200", "2x"),
            ("300", "3x"),
            ("400", "4x"),
        ]),
        &s.nv_vsr_scale().to_string(),
        |v, s| s.set_nv_vsr_scale(v.parse().unwrap_or(0)),
    ));
    // QualityLevel des VSR-Netzes (SDK: 1 low, 2 medium, 3 high); Auto
    // entspricht dem C++-Verhalten (high ab 3x Scale, sonst medium).
    vsr_section.push(select_row(
        "video-nv-vsr-quality",
        "VSR-Qualität",
        Some(
            "Qualität des VSR-Netzes — Auto = wie im C++-Client \
             (High ab 3x Upscale, sonst Medium)",
        ),
        "vsr quality level ai netz",
        vsr,
        opts(&[
            ("0", "Auto"),
            ("1", "Low"),
            ("2", "Medium"),
            ("3", "High"),
        ]),
        &s.nv_vsr_quality().to_string(),
        |v, s| s.set_nv_vsr_quality(v.parse().unwrap_or(0)),
    ));
    vsr_section.push(text_row(
        "video-nv-vsr-sdk-path",
        "VFX SDK path",
        Some("Optional. Folder with NVVideoEffects.dll \u{2014} empty = auto-detect"),
        "sdk path dll",
        vsr,
        s.nv_vsr_sdk_path(),
        "C:\\Program Files\\NVIDIA Corporation\\NVIDIA Video Effects",
        focus_for(cx, "video-nv-vsr-sdk-path"),
        |v, s| s.set_nv_vsr_sdk_path(v),
    ));
    vsr_section.push(toggle_row(
        "video-show-vsr-badge",
        "Show VSR badge in stream",
        Some("Green status badge in the top-right corner while upscaling is active"),
        "vsr badge indicator overlay hud",
        vsr,
        s.show_vsr_badge(),
    ));
    vsr_section.push(super::info_row(
        "Requires an NVIDIA RTX GPU, the CUDA decoder and the NVIDIA Video Effects SDK.",
        "vsr requirement rtx",
    ));

    // Stream Overlay: Master + pro-Badge-Einzel-Toggles + Debug-Zeile —
    // alle live wirksam (das Stream-HUD liest die Keys 1×/Frame live aus
    // dem Settings-Lock; kein Session-Neustart nötig). Der VSR-Badge-Toggle
    // bleibt bewusst in der VSR-Sektion (Abhängigkeit `visible: vsr`).
    let mut overlay = Section::new("Stream Overlay");
    overlay.push(toggle_row(
        "video-show-stream-stats",
        "Show stream stats during gameplay",
        Some(
            "Master der Stats-Badges — aus blendet die komplette Badge-Reihe im Stream aus \
             (VSR-Badge separat)",
        ),
        "hud overlay stats debug bitrate fps latency master",
        true,
        s.show_stream_stats(),
    ));
    overlay.push(toggle_row(
        "video-overlay-bitrate",
        "Badge: Bitrate",
        Some("Mbit/s-Badge in der Stats-Reihe des Stream-HUDs"),
        "hud overlay badge bitrate mbit",
        true,
        s.overlay_bitrate(),
    ));
    overlay.push(toggle_row(
        "video-overlay-rtt",
        "Badge: RTT",
        Some("Latenz-Badge (Senkusha-RTT) in der Stats-Reihe"),
        "hud overlay badge rtt latency ping",
        true,
        s.overlay_rtt(),
    ));
    overlay.push(toggle_row(
        "video-overlay-loss",
        "Badge: Loss",
        Some("Packet-Loss-Badge in der Stats-Reihe"),
        "hud overlay badge loss packet",
        true,
        s.overlay_loss(),
    ));
    overlay.push(toggle_row(
        "video-overlay-frametime",
        "Badge: Frame-Time",
        Some(
            "Zeit pro angezeigtem Frame — GPU-Pfad: Media-Thread (Decode + VSR + \
             Übergabe), CPU-Pfad: Presenter-Overhead (Alloc + NV12→BGRA + Wrap)",
        ),
        "hud overlay badge frame time",
        true,
        s.overlay_frametime(),
    ));
    overlay.push(toggle_row(
        "video-overlay-fps",
        "Badge: FPS",
        Some("Framerate-Badge in der Stats-Reihe"),
        "hud overlay badge fps framerate",
        true,
        s.overlay_fps(),
    ));
    overlay.push(toggle_row(
        "video-overlay-audio",
        "Badge: Audio",
        Some("Audio-Puffer-Füllstand-Badge in der Stats-Reihe"),
        "hud overlay badge audio buffer",
        true,
        s.overlay_audio(),
    ));
    overlay.push(toggle_row(
        "video-overlay-decoder",
        "Badge: Decoder",
        Some("Decoder-Backend-Badge in der Stats-Reihe"),
        "hud overlay badge decoder backend",
        true,
        s.overlay_decoder(),
    ));
    overlay.push(toggle_row(
        "video-overlay-haptics",
        "Badge: Haptics",
        Some("Haptics-Modus-Badge in der Stats-Reihe"),
        "hud overlay badge haptics rumble",
        true,
        s.overlay_haptics(),
    ));
    overlay.push(toggle_row(
        "video-overlay-debug",
        "Debug-Zeile im Stream",
        Some(
            "Monospaced Zeile unterm HUD: presented fps/drops, media ms (dec/vsr), slot-drops, \
             conv p95, sink gen/uploads/drops",
        ),
        "hud overlay debug line drops p95 sink slot conv",
        true,
        s.overlay_debug(),
    ));

    let ft_display_reason = "libplacebo-Display-Ziel nicht portiert — nur INI-Kompatibilität";
    let mut display = Section::new("Display");
    display.push(inactive(
        select_row(
            "video-display-prim",
            "Target Primaries",
            Some("Color primaries of the stream window (Auto recommended)"),
            "hdr display primaries gamut color",
            true,
            indexed_options(&[
                "Auto",
                "ITU-R Rec. BT.601 NTSC (Standard Gamut)",
                "ITU-R Rec. BT.601 PAL (Standard Gamut)",
                "ITU-R Rec. BT.709 (Standard Gamut)",
                "ITU-R Rec. BT.470 M (Standard Gamut)",
                "EBU Tech. 3213-E (Standard Gamut)",
                "ITU-R Rec. BT.2020 (Wide Gamut)",
                "Apple RGB (Wide Gamut)",
                "Adobe RGB (Wide Gamut)",
                "ProPhoto RGB (Wide Gamut)",
                "CIE 1931 RGB primaries (Wide Gamut)",
                "DCI-P3 (Wide Gamut)",
                "DCI-P3 with D65 white point (Wide Gamut)",
                "Panasonic V-Gamut (Wide Gamut)",
                "Sony S-Gamut (Wide Gamut)",
                "Traditional film primaries with Illuminant C (Wide Gamut)",
                "ACES Primaries #0 (Wide Gamut)",
                "ACES Primaries #1 (Wide Gamut)",
            ]),
            &s.display_target_prim().to_string(),
            |v, s| s.set_display_target_prim(v.parse().unwrap_or(0)),
        ),
        ft_display_reason,
    ));
    display.push(inactive(
        select_row(
            "video-display-trc",
            "Target Transfer Characteristics",
            Some("Transfer function (gamma) of the stream window"),
            "hdr display trc gamma transfer",
            true,
            indexed_options(&[
                "Auto",
                "ITU-R Rec. BT.1886 (SDR)",
                "IEC 61966-2-4 sRGB (SDR)",
                "Linear light content (SDR)",
                "IPure power gamma 1.8 (SDR)",
                "Pure power gamma 2.0 (SDR)",
                "Pure power gamma 2.2 (SDR)",
                "Pure power gamma 2.4 (SDR)",
                "Pure power gamma 2.6 (SDR)",
                "Pure power gamma 2.8 (SDR)",
                "ProPhoto RGB (SDR)",
                "Digital Cinema Distribution Master (SDR)",
                "ITU-R BT.2100 PQ / SMPTE ST2048 (HDR)",
                "ITU-R BT.2100 HLG / ARIB STD-B67 (HDR)",
                "Panasonic V-Log (HDR)",
                "Sony S-Log1 (HDR)",
                "Sony S-Log2 (HDR)",
            ]),
            &s.display_target_trc().to_string(),
            |v, s| s.set_display_target_trc(v.parse().unwrap_or(0)),
        ),
        ft_display_reason,
    ));
    let peak = s.display_target_peak();
    display.push(inactive(
        select_row(
            "video-display-peak-mode",
            "Target Peak",
            Some("Peak luminance of the display"),
            "hdr peak nits sdr",
            true,
            opts(&[("0", "Auto"), ("1000", "Numeric Value")]),
            if peak == 0 { "0" } else { "1000" },
            |v, s| s.set_display_target_peak(if v == "0" { 0 } else { 1000 }),
        ),
        ft_display_reason,
    ));
    display.push(inactive(
        slider_row(
            "video-display-peak-value",
            "Target Peak Value",
            Some("In nits"),
            "hdr peak nits",
            peak != 0,
            peak.max(10) as f64,
            10.0,
            10000.0,
            10.0,
            format!("{peak} nits"),
            |v, s| s.set_display_target_peak(v.round() as i64),
        ),
        ft_display_reason,
    ));
    let contrast = s.display_target_contrast();
    display.push(inactive(
        select_row(
            "video-display-contrast-mode",
            "Target Contrast",
            Some("Contrast of the display"),
            "hdr contrast infinity",
            true,
            opts(&[("0", "Auto"), ("-1", "Infinity"), ("1000", "Numeric Value")]),
            &match contrast {
                -1 => "-1".to_string(),
                0 => "0".to_string(),
                other => other.to_string(),
            },
            |v, s| {
                let value: i64 = v.parse().unwrap_or(0);
                // „Numeric Value“ startet bei 1000 (wie im v1-Dialog).
                s.set_display_target_contrast(if value > 0 { 1000 } else { value });
            },
        ),
        ft_display_reason,
    ));
    display.push(inactive(
        slider_row(
            "video-display-contrast-value",
            "Target Contrast Value",
            None,
            "hdr contrast value",
            contrast > 0,
            contrast.max(10) as f64,
            10.0,
            1000000.0,
            1000.0,
            format!("{contrast}"),
            |v, s| s.set_display_target_contrast(v.round() as i64),
        ),
        ft_display_reason,
    ));

    sections.extend([window, rendering, vsr_section, overlay, display]);

    // Rendering-Fine-Tuning (libplacebo) — nur beim Custom-Preset sichtbar
    // (wie QML `visible: filterMatch && videoPreset === 5`); die
    // „Erweitert“-Sektionen sind eingeklappt und öffnen sich bei der Suche.
    if s.placebo_preset() == PlaceboPreset::Custom {
        sections.push(fine_tuning_upscaling(&s));
        sections.push(fine_tuning_deband(&s));
        sections.push(fine_tuning_sigmoid(&s));
        if advanced_open || searching {
            sections.push(fine_tuning_deinterlace(&s));
            sections.push(fine_tuning_color_adjustment(&s));
            sections.push(fine_tuning_peak_detection(&s));
            sections.push(fine_tuning_color_mapping(&s));
            sections.push(fine_tuning_tone_mapping(&s));
        }
    }
    drop(s);

    // „Erweitert“-Umschalter für die Fine-Tuning-Sektionen.
    let mut advanced = Section::new("Advanced");
    advanced.push(SRow {
        search: "advanced show more fine tuning placebo".to_string(),
        visible: true,
        element: crate::components::Button::new("video-advanced-toggle", if advanced_open { "Hide advanced" } else { "Show advanced" })
            .variant(crate::components::ButtonVariant::Ghost)
            .on_click(|_, _w, cx| {
                super::mutate_state(cx, |s| s.advanced_open = !s.advanced_open);
            })
            .into_any_element(),
        inactive: None,
    });
    sections.push(advanced);

    sections
}

/// Index-basierte Select-Optionen (Wert = Index-String, z. B. Display-Enums).
fn indexed_options(labels: &'static [&'static str]) -> Vec<SelectOption> {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| SelectOption::new(i.to_string(), *label))
        .collect()
}
// ---------------------------------------------------------------------------
// Fine-Tuning-Sektionen (placebo_render_params.ini → placebo_settings/*)
// ---------------------------------------------------------------------------

const UPSCALERS: [(&str, PlaceboUpscaler); 14] = [
    ("none", PlaceboUpscaler::None),
    ("nearest", PlaceboUpscaler::Nearest),
    ("bilinear", PlaceboUpscaler::Bilinear),
    ("oversample", PlaceboUpscaler::Oversample),
    ("bicubic", PlaceboUpscaler::Bicubic),
    ("gaussian", PlaceboUpscaler::Gaussian),
    ("catmull_rom", PlaceboUpscaler::CatmullRom),
    ("lanczos", PlaceboUpscaler::Lanczos),
    ("ewa_lanczos", PlaceboUpscaler::EwaLanczos),
    ("ewa_lanczossharp", PlaceboUpscaler::EwaLanczosSharp),
    ("ewa_lanczos4sharpest", PlaceboUpscaler::EwaLanczos4Sharpest),
    ("fsr", PlaceboUpscaler::Fsr),
    ("fsrcnnx_x2_8_0_4_1", PlaceboUpscaler::Fsrcnnx8),
    ("fsrcnnx_x2_16_0_4_1", PlaceboUpscaler::Fsrcnnx16),
];

const DOWNSCALERS: [(&str, PlaceboDownscaler); 9] = [
    ("none", PlaceboDownscaler::None),
    ("box", PlaceboDownscaler::Box),
    ("hermite", PlaceboDownscaler::Hermite),
    ("bilinear", PlaceboDownscaler::Bilinear),
    ("bicubic", PlaceboDownscaler::Bicubic),
    ("gaussian", PlaceboDownscaler::Gaussian),
    ("catmull_rom", PlaceboDownscaler::CatmullRom),
    ("mitchell", PlaceboDownscaler::Mitchell),
    ("lanczos", PlaceboDownscaler::Lanczos),
];

const GAMUTS: [(&str, PlaceboGamutMappingFunction); 10] = [
    ("clip", PlaceboGamutMappingFunction::Clip),
    ("perceptual", PlaceboGamutMappingFunction::Perceptual),
    ("softclip", PlaceboGamutMappingFunction::SoftClip),
    ("relative", PlaceboGamutMappingFunction::Relative),
    ("saturation", PlaceboGamutMappingFunction::Saturation),
    ("absolute", PlaceboGamutMappingFunction::Absolute),
    ("desaturate", PlaceboGamutMappingFunction::Desaturate),
    ("darken", PlaceboGamutMappingFunction::Darken),
    ("highlight", PlaceboGamutMappingFunction::Highlight),
    ("linear", PlaceboGamutMappingFunction::Linear),
];

const TONES: [(&str, PlaceboToneMappingFunction); 12] = [
    ("clip", PlaceboToneMappingFunction::Clip),
    ("spline", PlaceboToneMappingFunction::Spline),
    ("st2094-40", PlaceboToneMappingFunction::St209440),
    ("st2094-10", PlaceboToneMappingFunction::St209410),
    ("bt2390", PlaceboToneMappingFunction::Bt2390),
    ("bt2446a", PlaceboToneMappingFunction::Bt2446a),
    ("reinhard", PlaceboToneMappingFunction::Reinhard),
    ("mobius", PlaceboToneMappingFunction::Mobius),
    ("hable", PlaceboToneMappingFunction::Hable),
    ("gamma", PlaceboToneMappingFunction::Gamma),
    ("linear", PlaceboToneMappingFunction::Linear),
    ("linearlight", PlaceboToneMappingFunction::LinearLight),
];

const TONE_METADATA: [(&str, PlaceboToneMappingMetadata); 5] = [
    ("any", PlaceboToneMappingMetadata::Any),
    ("none", PlaceboToneMappingMetadata::None),
    ("hdr10", PlaceboToneMappingMetadata::Hdr10),
    ("hdr10plus", PlaceboToneMappingMetadata::Hdr10Plus),
    ("cie_y", PlaceboToneMappingMetadata::CieY),
];

const DEINTERLACE_ALGOS: [(&str, PlaceboDeinterlaceAlgorithm); 4] = [
    ("weave", PlaceboDeinterlaceAlgorithm::Weave),
    ("bob", PlaceboDeinterlaceAlgorithm::Bob),
    ("yadif", PlaceboDeinterlaceAlgorithm::Yadif),
    ("bwdif", PlaceboDeinterlaceAlgorithm::Bwdif),
];



/// Presets/Enums ohne „None = leerer String“-Semantik.
fn no_empty_name<E>(_: E) -> Option<&'static str> {
    None
}

// Die generischen Helfer für Placebo-Rows (direkt über die Settings-API).
// Jede Fine-Tuning-Row ist im Rust-Build inaktiv (kein libplacebo) und wird
// zentral hier mit dem Inaktiv-Hinweis markiert.
mod ft {
    use super::Settings;
    use crate::pages::settings::{inactive, select_row, slider_row, toggle_row_with, SRow};

    const FT_REASON: &str = "libplacebo nicht portiert — nur INI-Kompatibilität";

    pub(super) fn bool_row(
        id: &'static str,
        label: &str,
        subtitle: Option<&str>,
        value: bool,
        set: fn(&mut Settings, bool),
    ) -> SRow {
        inactive(
            toggle_row_with(
                id,
                label,
                subtitle,
                "placebo fine tuning",
                true,
                value,
                move |v, s| set(s, v),
            ),
            FT_REASON,
        )
    }

    pub(super) fn enum_row<E: Copy + PartialEq>(
        id: &'static str,
        label: &str,
        subtitle: Option<&str>,
        pairs: &'static [(&'static str, E)],
        current: E,
        name_of: fn(E) -> Option<&'static str>, // INI-Name ("" = Key entfernt)
        set: fn(&mut Settings, E),
    ) -> SRow {
        let options: Vec<crate::components::SelectOption> =
            pairs.iter().map(|(name, _)| crate::components::SelectOption::new(*name, *name)).collect();
        inactive(
            select_row(
                id,
                label,
                subtitle,
                "placebo fine tuning",
                true,
                options,
                name_of(current).unwrap_or("").to_string(),
                move |v, s| {
                    if let Some((_, e)) = pairs.iter().find(|(name, _)| *name == v) {
                        set(s, *e);
                    }
                },
            ),
            FT_REASON,
        )
    }

    pub(super) fn float_row(
        id: &'static str,
        label: &str,
        value: f64,
        min: f64,
        max: f64,
        step: f64,
        suffix: &str,
        set: fn(&mut Settings, f64),
    ) -> SRow {
        inactive(
            slider_row(
                id,
                label,
                None,
                "placebo fine tuning",
                true,
                value,
                min,
                max,
                step,
                format!("{value:.2}{suffix}"),
                move |v, s| set(s, v),
            ),
            FT_REASON,
        )
    }

    pub(super) fn int_row(
        id: &'static str,
        label: &str,
        value: i64,
        min: i64,
        max: i64,
        step: i64,
        suffix: &str,
        set: fn(&mut Settings, i64),
    ) -> SRow {
        inactive(
            slider_row(
                id,
                label,
                None,
                "placebo fine tuning",
                true,
                value as f64,
                min as f64,
                max as f64,
                step as f64,
                format!("{value}{suffix}"),
                move |v, s| set(s, v.round() as i64),
            ),
            FT_REASON,
        )
    }
}

fn fine_tuning_upscaling(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Upscaling");
    section.push(ft::enum_row(
        "video-ft-upscaler",
        "Upscaler",
        Some("Main image scaler (libplacebo)"),
        &UPSCALERS,
        s.placebo_upscaler(),
        no_empty_name,
        Settings::set_placebo_upscaler,
    ));
    section.push(ft::enum_row(
        "video-ft-plane-upscaler",
        "Plane upscaler",
        None,
        &UPSCALERS,
        s.placebo_plane_upscaler(),
        no_empty_name,
        Settings::set_placebo_plane_upscaler,
    ));
    section.push(ft::enum_row(
        "video-ft-downscaler",
        "Downscaler",
        None,
        &DOWNSCALERS,
        s.placebo_downscaler(),
        no_empty_name,
        Settings::set_placebo_downscaler,
    ));
    section.push(ft::enum_row(
        "video-ft-plane-downscaler",
        "Plane downscaler",
        None,
        &DOWNSCALERS,
        s.placebo_plane_downscaler(),
        no_empty_name,
        Settings::set_placebo_plane_downscaler,
    ));
    section.push(ft::float_row(
        "video-ft-antiringing",
        "Antiringing strength",
        s.placebo_antiringing_strength(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_antiringing_strength,
    ));
    section
}

fn fine_tuning_deband(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Deband");
    section.push(ft::bool_row(
        "video-ft-deband",
        "Deband",
        Some("Remove banding artifacts from the stream"),
        s.placebo_deband_enabled(),
        Settings::set_placebo_deband_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-deband-preset",
        "Deband preset",
        None,
        &[("", PlaceboDebandPreset::None), ("default", PlaceboDebandPreset::Default)],
        s.placebo_deband_preset(),
        deband_preset_value_opt,
        Settings::set_placebo_deband_preset,
    ));
    section.push(ft::int_row(
        "video-ft-deband-iterations",
        "Deband iterations",
        s.placebo_deband_iterations(),
        1,
        4,
        1,
        "",
        Settings::set_placebo_deband_iterations,
    ));
    section.push(ft::float_row(
        "video-ft-deband-threshold",
        "Deband threshold",
        s.placebo_deband_threshold(),
        0.0,
        20.0,
        0.5,
        "",
        Settings::set_placebo_deband_threshold,
    ));
    section.push(ft::float_row(
        "video-ft-deband-radius",
        "Deband radius",
        s.placebo_deband_radius(),
        1.0,
        64.0,
        1.0,
        "",
        Settings::set_placebo_deband_radius,
    ));
    section.push(ft::float_row(
        "video-ft-deband-grain",
        "Deband grain",
        s.placebo_deband_grain(),
        0.0,
        64.0,
        1.0,
        "",
        Settings::set_placebo_deband_grain,
    ));
    section
}

fn fine_tuning_sigmoid(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Sigmoid");
    section.push(ft::bool_row(
        "video-ft-sigmoid",
        "Sigmoid",
        Some("Sigmoidal contrast during upscaling"),
        s.placebo_sigmoid_enabled(),
        Settings::set_placebo_sigmoid_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-sigmoid-preset",
        "Sigmoid preset",
        None,
        &[("", PlaceboSigmoidPreset::None), ("default", PlaceboSigmoidPreset::Default)],
        s.placebo_sigmoid_preset(),
        sigmoid_preset_value_opt,
        Settings::set_placebo_sigmoid_preset,
    ));
    section.push(ft::float_row(
        "video-ft-sigmoid-center",
        "Sigmoid center",
        s.placebo_sigmoid_center(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_sigmoid_center,
    ));
    section.push(ft::float_row(
        "video-ft-sigmoid-slope",
        "Sigmoid slope",
        s.placebo_sigmoid_slope(),
        1.0,
        20.0,
        0.5,
        "",
        Settings::set_placebo_sigmoid_slope,
    ));
    section
}

fn fine_tuning_deinterlace(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Deinterlace");
    section.push(ft::bool_row(
        "video-ft-deinterlace",
        "Deinterlace",
        None,
        s.placebo_deinterlace_enabled(),
        Settings::set_placebo_deinterlace_enabled,
    ));
    section.push(ft::bool_row(
        "video-ft-deinterlace-skip-spatial",
        "Skip spatial deinterlacing",
        None,
        s.placebo_deinterlace_skip_spatial(),
        Settings::set_placebo_deinterlace_skip_spatial,
    ));
    section.push(ft::enum_row(
        "video-ft-deinterlace-algo",
        "Deinterlace algorithm",
        None,
        &DEINTERLACE_ALGOS,
        s.placebo_deinterlace_algorithm(),
        no_empty_name,
        Settings::set_placebo_deinterlace_algorithm,
    ));
    section
}

fn fine_tuning_color_adjustment(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Color Adjustment");
    section.push(ft::bool_row(
        "video-ft-color-adjustment",
        "Color adjustment",
        None,
        s.placebo_color_adjustment_enabled(),
        Settings::set_placebo_color_adjustment_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-color-adjustment-preset",
        "Color adjustment preset",
        None,
        &[("", PlaceboColorAdjustmentPreset::None), ("neutral", PlaceboColorAdjustmentPreset::Neutral)],
        s.placebo_color_adjustment_preset(),
        color_adjustment_preset_value_opt,
        Settings::set_placebo_color_adjustment_preset,
    ));
    section.push(ft::float_row(
        "video-ft-brightness",
        "Brightness",
        s.placebo_color_adjustment_brightness(),
        -1.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_color_adjustment_brightness,
    ));
    section.push(ft::float_row(
        "video-ft-contrast",
        "Contrast",
        s.placebo_color_adjustment_contrast(),
        0.0,
        2.0,
        0.1,
        "",
        Settings::set_placebo_color_adjustment_contrast,
    ));
    section.push(ft::float_row(
        "video-ft-saturation",
        "Saturation",
        s.placebo_color_adjustment_saturation(),
        0.0,
        3.0,
        0.1,
        "",
        Settings::set_placebo_color_adjustment_saturation,
    ));
    section.push(ft::float_row(
        "video-ft-hue",
        "Hue",
        s.placebo_color_adjustment_hue(),
        -3.15,
        3.15,
        0.1,
        "",
        Settings::set_placebo_color_adjustment_hue,
    ));
    section.push(ft::float_row(
        "video-ft-gamma",
        "Gamma",
        s.placebo_color_adjustment_gamma(),
        0.0,
        3.0,
        0.1,
        "",
        Settings::set_placebo_color_adjustment_gamma,
    ));
    section.push(ft::float_row(
        "video-ft-temperature",
        "Temperature",
        s.placebo_color_adjustment_temperature(),
        -1.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_color_adjustment_temperature,
    ));
    section
}

fn fine_tuning_peak_detection(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Peak Detection");
    section.push(ft::bool_row(
        "video-ft-peak-detect",
        "Peak detection",
        Some("Dynamic tone mapping to the display peak"),
        s.placebo_peak_detection_enabled(),
        Settings::set_placebo_peak_detection_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-peak-detect-preset",
        "Peak detection preset",
        None,
        &[
            ("", PlaceboPeakDetectionPreset::None),
            ("default", PlaceboPeakDetectionPreset::Default),
            ("high_quality", PlaceboPeakDetectionPreset::HighQuality),
        ],
        s.placebo_peak_detection_preset(),
        peak_detection_preset_value_opt,
        Settings::set_placebo_peak_detection_preset,
    ));
    section.push(ft::float_row(
        "video-ft-peak-smoothing",
        "Peak smoothing period",
        s.placebo_peak_smoothing_period(),
        0.0,
        100.0,
        1.0,
        "",
        Settings::set_placebo_peak_smoothing_period,
    ));
    section.push(ft::float_row(
        "video-ft-scene-threshold-low",
        "Scene threshold low",
        s.placebo_scene_threshold_low(),
        0.0,
        10.0,
        0.5,
        "",
        Settings::set_placebo_scene_threshold_low,
    ));
    section.push(ft::float_row(
        "video-ft-scene-threshold-high",
        "Scene threshold high",
        s.placebo_scene_threshold_high(),
        0.0,
        10.0,
        0.5,
        "",
        Settings::set_placebo_scene_threshold_high,
    ));
    section.push(ft::float_row(
        "video-ft-peak-percentile",
        "Peak percentile",
        s.placebo_peak_percentile(),
        0.0,
        100.0,
        1.0,
        "",
        Settings::set_placebo_peak_percentile,
    ));
    section.push(ft::float_row(
        "video-ft-black-cutoff",
        "Black cutoff",
        s.placebo_black_cutoff(),
        0.0,
        100.0,
        1.0,
        "",
        Settings::set_placebo_black_cutoff,
    ));
    section.push(ft::bool_row(
        "video-ft-allow-delayed-peak",
        "Allow delayed peak",
        None,
        s.placebo_allow_delayed_peak(),
        Settings::set_placebo_allow_delayed_peak,
    ));
    section
}

fn fine_tuning_color_mapping(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Color Mapping");
    section.push(ft::bool_row(
        "video-ft-color-map",
        "Color mapping",
        Some("Gamut mapping between stream and display color space"),
        s.placebo_color_mapping_enabled(),
        Settings::set_placebo_color_mapping_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-color-map-preset",
        "Color mapping preset",
        None,
        &[
            ("", PlaceboColorMappingPreset::None),
            ("default", PlaceboColorMappingPreset::Default),
            ("high_quality", PlaceboColorMappingPreset::HighQuality),
        ],
        s.placebo_color_mapping_preset(),
        color_mapping_preset_value_opt,
        Settings::set_placebo_color_mapping_preset,
    ));
    section.push(ft::enum_row(
        "video-ft-gamut-mapping",
        "Gamut mapping function",
        None,
        &GAMUTS,
        s.placebo_gamut_mapping_function(),
        no_empty_name,
        Settings::set_placebo_gamut_mapping_function,
    ));
    section.push(ft::float_row(
        "video-ft-perceptual-deadzone",
        "Perceptual deadzone",
        s.placebo_perceptual_deadzone(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_perceptual_deadzone,
    ));
    section.push(ft::float_row(
        "video-ft-perceptual-strength",
        "Perceptual strength",
        s.placebo_perceptual_strength(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_perceptual_strength,
    ));
    section.push(ft::float_row(
        "video-ft-colorimetric-gamma",
        "Colorimetric gamma",
        s.placebo_colorimetric_gamma(),
        0.0,
        4.0,
        0.1,
        "",
        Settings::set_placebo_colorimetric_gamma,
    ));
    section.push(ft::float_row(
        "video-ft-softclip-knee",
        "Softclip knee",
        s.placebo_softclip_knee(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_softclip_knee,
    ));
    section.push(ft::float_row(
        "video-ft-softclip-desat",
        "Softclip desaturation",
        s.placebo_softclip_desat(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_softclip_desat,
    ));
    section.push(ft::int_row(
        "video-ft-lut3d-size-i",
        "3D LUT size (intensity)",
        s.placebo_lut3d_size_i(),
        2,
        64,
        2,
        "",
        Settings::set_placebo_lut3d_size_i,
    ));
    section.push(ft::int_row(
        "video-ft-lut3d-size-c",
        "3D LUT size (chroma)",
        s.placebo_lut3d_size_c(),
        2,
        64,
        2,
        "",
        Settings::set_placebo_lut3d_size_c,
    ));
    section.push(ft::int_row(
        "video-ft-lut3d-size-h",
        "3D LUT size (hue)",
        s.placebo_lut3d_size_h(),
        16,
        512,
        16,
        "",
        Settings::set_placebo_lut3d_size_h,
    ));
    section.push(ft::bool_row(
        "video-ft-lut3d-tricubic",
        "3D LUT tricubic interpolation",
        None,
        s.placebo_lut3d_tricubic_enabled(),
        Settings::set_placebo_lut3d_tricubic_enabled,
    ));
    section.push(ft::bool_row(
        "video-ft-gamut-expansion",
        "Gamut expansion",
        None,
        s.placebo_gamut_expansion_enabled(),
        Settings::set_placebo_gamut_expansion_enabled,
    ));
    section
}

fn fine_tuning_tone_mapping(s: &Settings) -> Section {
    let mut section = Section::new("Fine-Tuning: Tone Mapping");
    section.push(ft::enum_row(
        "video-ft-tone-mapping",
        "Tone mapping function",
        None,
        &TONES,
        s.placebo_tone_mapping_function(),
        no_empty_name,
        Settings::set_placebo_tone_mapping_function,
    ));
    section.push(ft::float_row(
        "video-ft-knee-adaptation",
        "Knee adaptation speed",
        s.placebo_knee_adaptation(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_knee_adaptation,
    ));
    section.push(ft::float_row(
        "video-ft-knee-minimum",
        "Knee minimum",
        s.placebo_knee_minimum(),
        0.0,
        0.5,
        0.025,
        "",
        Settings::set_placebo_knee_minimum,
    ));
    section.push(ft::float_row(
        "video-ft-knee-maximum",
        "Knee maximum",
        s.placebo_knee_maximum(),
        0.5,
        1.0,
        0.025,
        "",
        Settings::set_placebo_knee_maximum,
    ));
    section.push(ft::float_row(
        "video-ft-knee-default",
        "Knee default",
        s.placebo_knee_default(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_knee_default,
    ));
    section.push(ft::float_row(
        "video-ft-knee-offset",
        "Knee offset",
        s.placebo_knee_offset(),
        0.0,
        2.0,
        0.05,
        "",
        Settings::set_placebo_knee_offset,
    ));
    section.push(ft::float_row(
        "video-ft-slope-tuning",
        "Slope tuning",
        s.placebo_slope_tuning(),
        0.0,
        10.0,
        0.1,
        "",
        Settings::set_placebo_slope_tuning,
    ));
    section.push(ft::float_row(
        "video-ft-slope-offset",
        "Slope offset",
        s.placebo_slope_offset(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_slope_offset,
    ));
    section.push(ft::float_row(
        "video-ft-spline-contrast",
        "Spline contrast",
        s.placebo_spline_contrast(),
        0.0,
        1.5,
        0.05,
        "",
        Settings::set_placebo_spline_contrast,
    ));
    section.push(ft::float_row(
        "video-ft-reinhard-contrast",
        "Reinhard contrast",
        s.placebo_reinhard_contrast(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_reinhard_contrast,
    ));
    section.push(ft::float_row(
        "video-ft-linear-knee",
        "Linear knee",
        s.placebo_linear_knee(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_linear_knee,
    ));
    section.push(ft::float_row(
        "video-ft-exposure",
        "Exposure",
        s.placebo_exposure(),
        0.0,
        10.0,
        0.1,
        "",
        Settings::set_placebo_exposure,
    ));
    section.push(ft::bool_row(
        "video-ft-inverse-tone-mapping",
        "Inverse tone mapping",
        Some("Expand SDR content into the HDR range"),
        s.placebo_inverse_tone_mapping_enabled(),
        Settings::set_placebo_inverse_tone_mapping_enabled,
    ));
    section.push(ft::enum_row(
        "video-ft-tone-metadata",
        "Tone mapping metadata",
        None,
        &TONE_METADATA,
        s.placebo_tone_mapping_metadata(),
        no_empty_name,
        Settings::set_placebo_tone_mapping_metadata,
    ));
    section.push(ft::int_row(
        "video-ft-tone-lut-size",
        "Tone mapping LUT size",
        s.placebo_tone_lut_size(),
        16,
        1024,
        16,
        "",
        Settings::set_placebo_tone_lut_size,
    ));
    section.push(ft::float_row(
        "video-ft-contrast-recovery",
        "Contrast recovery",
        s.placebo_contrast_recovery(),
        0.0,
        1.0,
        0.05,
        "",
        Settings::set_placebo_contrast_recovery,
    ));
    section.push(ft::float_row(
        "video-ft-contrast-smoothness",
        "Contrast smoothness",
        s.placebo_contrast_smoothness(),
        0.0,
        20.0,
        0.5,
        "",
        Settings::set_placebo_contrast_smoothness,
    ));
    section
}

fn deband_preset_value_opt(p: PlaceboDebandPreset) -> Option<&'static str> {
    match p {
        PlaceboDebandPreset::None => Some(""), // leerer String = Key entfernt (C++-Semantik)
        PlaceboDebandPreset::Default => Some("default"),
    }
}

fn sigmoid_preset_value_opt(p: PlaceboSigmoidPreset) -> Option<&'static str> {
    match p {
        PlaceboSigmoidPreset::None => Some(""),
        PlaceboSigmoidPreset::Default => Some("default"),
    }
}

fn color_adjustment_preset_value_opt(p: PlaceboColorAdjustmentPreset) -> Option<&'static str> {
    match p {
        PlaceboColorAdjustmentPreset::None => Some(""),
        PlaceboColorAdjustmentPreset::Neutral => Some("neutral"),
    }
}

fn peak_detection_preset_value_opt(p: PlaceboPeakDetectionPreset) -> Option<&'static str> {
    match p {
        PlaceboPeakDetectionPreset::None => Some(""),
        PlaceboPeakDetectionPreset::Default => Some("default"),
        PlaceboPeakDetectionPreset::HighQuality => Some("high_quality"),
    }
}

fn color_mapping_preset_value_opt(p: PlaceboColorMappingPreset) -> Option<&'static str> {
    match p {
        PlaceboColorMappingPreset::None => Some(""),
        PlaceboColorMappingPreset::Default => Some("default"),
        PlaceboColorMappingPreset::HighQuality => Some("high_quality"),
    }
}

// ---------------------------------------------------------------------------
// Mapping-Helfer (Enum → INI-String)
// ---------------------------------------------------------------------------

fn window_type_value(t: WindowType) -> &'static str {
    match t {
        WindowType::SelectedResolution => "Selected Resolution",
        WindowType::CustomResolution => "Custom Resolution",
        WindowType::AdjustableResolution => "Adjust Manually",
        WindowType::Fullscreen => "Fullscreen",
        WindowType::Zoom => "Zoom",
        WindowType::Stretch => "Stretch",
    }
}

fn decoder_value(d: Decoder) -> &'static str {
    match d {
        Decoder::Ffmpeg => "ffmpeg",
        Decoder::Pi => "pi",
    }
}

fn preset_value(p: PlaceboPreset) -> &'static str {
    match p {
        PlaceboPreset::Fast => "fast",
        PlaceboPreset::Default => "default",
        PlaceboPreset::HighQuality => "high_quality",
        PlaceboPreset::HighQualitySpatial => "high_quality_spatial",
        PlaceboPreset::HighQualityAdvancedSpatial => "high_quality_advanced_spatial",
        PlaceboPreset::Custom => "custom",
    }
}

fn frame_mixer_value(m: PlaceboFrameMixer) -> &'static str {
    match m {
        PlaceboFrameMixer::None => "none",
        PlaceboFrameMixer::Oversample => "oversample",
        PlaceboFrameMixer::Hermite => "hermite",
        PlaceboFrameMixer::Linear => "linear",
        PlaceboFrameMixer::Cubic => "cubic",
    }
}

/// Focus-Handle pro Text-Row (aus dem Seiten-Zustand).
fn focus_for(cx: &mut gpui::Context<AppShell>, name: &str) -> gpui::FocusHandle {
    use super::SettingsUiState;
    let state = cx.global_mut::<SettingsUiState>();
    match name {
        "video-custom-width" => state.custom_width_focus.clone(),
        "video-custom-height" => state.custom_height_focus.clone(),
        "video-nv-vsr-sdk-path" => state.vsr_path_focus.clone(),
        _ => state.search_focus.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> Settings {
        let base = std::env::temp_dir().join(format!(
            "chiaki-ui-settings-video-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        Settings::open_at(chiaki_settings::settings::SettingsPaths {
            settings: base.join("settings.ini"),
            default_settings: base.join("settings.default.ini"),
            placebo: base.join("placebo_render_params.ini"),
            base,
        })
        .unwrap()
    }

    /// Die UI-Wertetabellen müssen exakt die INI-Namen der Settings-Setter
    /// schreiben (Roundtrip über `placebo_values()`).
    #[test]
    fn placebo_name_tables_match_settings_ini_names() {
        let mut s = test_settings();

        for (name, value) in UPSCALERS {
            s.set_placebo_upscaler(value);
            assert_eq!(s.placebo_values().get("upscaler"), Some(&name.to_string()));
        }
        for (name, value) in UPSCALERS {
            s.set_placebo_plane_upscaler(value);
            assert_eq!(s.placebo_values().get("plane_upscaler"), Some(&name.to_string()));
        }
        for (name, value) in DOWNSCALERS {
            s.set_placebo_downscaler(value);
            assert_eq!(s.placebo_values().get("downscaler"), Some(&name.to_string()));
        }
        for (name, value) in DOWNSCALERS {
            s.set_placebo_plane_downscaler(value);
            assert_eq!(s.placebo_values().get("plane_downscaler"), Some(&name.to_string()));
        }
        for (name, value) in GAMUTS {
            s.set_placebo_gamut_mapping_function(value);
            assert_eq!(s.placebo_values().get("gamut_mapping"), Some(&name.to_string()));
        }
        for (name, value) in TONES {
            s.set_placebo_tone_mapping_function(value);
            assert_eq!(s.placebo_values().get("tone_mapping"), Some(&name.to_string()));
        }
        for (name, value) in TONE_METADATA {
            s.set_placebo_tone_mapping_metadata(value);
            assert_eq!(s.placebo_values().get("tone_map_metadata"), Some(&name.to_string()));
        }
        for (name, value) in DEINTERLACE_ALGOS {
            s.set_placebo_deinterlace_algorithm(value);
            assert_eq!(s.placebo_values().get("deinterlace_algo"), Some(&name.to_string()));
        }
    }

    /// Presets mit C++-""-Semantik: „None“ entfernt den Key.
    #[test]
    fn empty_preset_semantics() {
        let mut s = test_settings();
        s.set_placebo_deband_preset(PlaceboDebandPreset::Default);
        assert_eq!(s.placebo_values().get("deband_preset"), Some(&"default".to_string()));
        s.set_placebo_deband_preset(PlaceboDebandPreset::None);
        assert!(!s.placebo_values().contains_key("deband_preset"));

        s.set_placebo_sigmoid_preset(PlaceboSigmoidPreset::Default);
        assert_eq!(s.placebo_values().get("sigmoid_preset"), Some(&"default".to_string()));
        s.set_placebo_sigmoid_preset(PlaceboSigmoidPreset::None);
        assert!(!s.placebo_values().contains_key("sigmoid_preset"));

        s.set_placebo_color_adjustment_preset(PlaceboColorAdjustmentPreset::Neutral);
        assert_eq!(
            s.placebo_values().get("color_adjustment_preset"),
            Some(&"neutral".to_string())
        );
        s.set_placebo_color_adjustment_preset(PlaceboColorAdjustmentPreset::None);
        assert!(!s.placebo_values().contains_key("color_adjustment_preset"));

        s.set_placebo_peak_detection_preset(PlaceboPeakDetectionPreset::HighQuality);
        assert_eq!(
            s.placebo_values().get("peak_detect_preset"),
            Some(&"high_quality".to_string())
        );
        s.set_placebo_peak_detection_preset(PlaceboPeakDetectionPreset::None);
        assert!(!s.placebo_values().contains_key("peak_detect_preset"));

        s.set_placebo_color_mapping_preset(PlaceboColorMappingPreset::HighQuality);
        assert_eq!(
            s.placebo_values().get("color_map_preset"),
            Some(&"high_quality".to_string())
        );
        s.set_placebo_color_mapping_preset(PlaceboColorMappingPreset::None);
        assert!(!s.placebo_values().contains_key("color_map_preset"));
    }
}
