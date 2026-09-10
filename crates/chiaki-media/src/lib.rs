// SPDX-License-Identifier: AGPL-3.0-only
//! chiaki-media — FFmpeg/NVDEC-Videodekodierung + Opus-Audio über FFI.
//!
//! Windows-only. FFmpeg (avutil/avcodec/swscale) und libopus werden zur Laufzeit
//! über `libloading` geladen — keine Linkzeit-Bindings, kein ffmpeg-sys. Alle
//! unsicheren Bindings leben in den jeweiligen `sys`-Modulen; die Oberfläche
//! bleibt safe (einzige Ausnahme: die echten Plane-Pointer in
//! [`decoder::DecodedFrame`], siehe Lifetime-Vertrag dort).
//!
//! Module:
//! - [`ffmpeg`] — DLL-Suche/-Laden, tracing-Log-Bridge, schmale FFI-Bindings
//! - [`decoder`] — Port von `lib/src/ffmpegdecoder.c` (H264/H265, NVDEC/D3D11VA/
//!   Vulkan/Software, immer NV12-Ausgabe, NVDEC-aligned-height-Metadaten)
//! - [`vsr`] — Port von `gui/src/vsrupscaler.cpp` (NVIDIA VFX SDK "VideoSuperRes",
//!   dynamisch geladen, Windows-only, deaktiviert sich sauber ohne SDK)
//! - [`opus`] — Port von `lib/src/opusdecoder.c`/`opusencoder.c` (+ Concealment)
//! - [`audio`] — Port der SDL-Audio-Teile von `gui/src/streamsession.cpp` auf
//!   cpal/WASAPI: Ausgabe ([`audio::AudioOutput`], Ring + Volume + Latenz-Stats)
//!   und Mikrofon ([`audio::AudioInput`], Capture → Opus-40-Byte-Frames)

pub mod audio;
pub mod cuda_d3d11;
pub mod d3d11_copy;
pub mod decoder;
pub mod ffmpeg;
pub mod opus;
pub mod vsr;

pub use audio::{AudioInput, AudioOutput};
pub use decoder::{
    nv12_aligned_height, DecodedFrame, Decoder, FrameFormat, FrameMemory, HwBackend, Plane,
};
pub use vsr::{FrameBuf, VsrUpscaler};

#[cfg(test)]
pub(crate) mod test_setup {
    use std::path::PathBuf;
    use std::sync::Once;

    static SETUP: Once = Once::new();

    /// C++-Referenz-Checkout als Workspace-Geschwister (Konvention wie in
    /// chiaki-core/build.rs): `<workspace>/../chiaki-rust-remaster`.
    fn reference_dir(sub: &str) -> Option<PathBuf> {
        let ws_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)?
            .to_path_buf();
        let dir = ws_root.join("..").join("chiaki-rust-remaster").join(sub);
        dir.is_dir().then_some(dir)
    }

    /// Setzt die Suchpfad-Umgebungsvariablen auf die lokalen Referenz-DLLs und
    /// installiert einen tracing-Subscriber (Tests leben von den Logs), sofern
    /// nichts anderes vorgegeben ist (Task-Vorgabe: Tests laufen gegen die
    /// FFmpeg-Referenz-DLLs, Opus aus dem chiaki-remaster-Win-Ordner). Auf
    /// Maschinen ohne den Referenz-Checkout (z. B. CI) bleiben die Variablen
    /// ungesetzt — dort stellen die Workflows die DLLs über dieselben
    /// Variablen bereit; SDK-abhängige Tests skrippen dann sauber.
    pub fn reference_dlls() {
        SETUP.call_once(|| {
            let filter = tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug"));
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_test_writer()
                .try_init();

            if let Some(dir) = reference_dir(r"ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin") {
                if std::env::var_os("CHIAKI_FFMPEG_DIR").is_none() {
                    std::env::set_var("CHIAKI_FFMPEG_DIR", dir);
                }
            }
            if let Some(dir) = reference_dir(r"chiaki-remaster-Win") {
                if std::env::var_os("CHIAKI_OPUS_DIR").is_none() {
                    std::env::set_var("CHIAKI_OPUS_DIR", dir);
                }
            }
            // VFX-SDK "VideoFX/bin" (NVVideoEffects.dll/NVCVImage.dll) — lädt
            // auch ohne GPU; die GPU-Tests sind zusätzlich #[ignore].
            if let Some(dir) = reference_dir(r"vfx_sdk\sdk\VideoFX\bin") {
                if std::env::var_os("CHIAKI_VSR_SDK_DIR").is_none() {
                    std::env::set_var("CHIAKI_VSR_SDK_DIR", dir);
                }
            }
        });
    }
}
