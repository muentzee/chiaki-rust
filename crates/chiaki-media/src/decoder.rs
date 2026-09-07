// SPDX-License-Identifier: AGPL-3.0-only
//! Port von `lib/src/ffmpegdecoder.c` (chiaki-ng) auf dynamisch geladenes FFmpeg.
//!
//! Unterschiede zum C-Decoder (dokumentiert, Verhalten identisch):
//! - Der C-Client rendert Zero-Copy direkt aus dem HW-Frame (libplacebo/CUDA);
//!   diese Crate überträgt HW-Frames mit `av_hwframe_transfer_data` in
//!   Systemspeicher (NV12), damit chiaki-render eine einfache CPU-NV12-Textur
//!   hochladen kann. Nur der Software-Pfad liefert YUV420P und wird per
//!   swscale nach NV12 konvertiert — der DecodedFrame-Vertrag ist damit
//!   **immer NV12 mit 2 Planes**.
//! - `decode_sample()` fasst C `chiaki_ffmpeg_decoder_video_sample_cb` +
//!   `chiaki_ffmpeg_decoder_pull_frame` zusammen (push + drain bis zum letzten
//!   Frame, "only the very last frame" wie im C).
//!
//! ## NVDEC aligned height (REWORK.md, Root Cause 1)
//! NVDEC-Surfaces sind höher-aligned als das Bild (1080 → 1088). Die UV-Ebene
//! liegt real bei `data[1]` (= `Y + pitch * aligned_height`), NICHT bei
//! `Y + pitch * height`. Deshalb exponiert `DecodedFrame` die **echten**
//! Plane-Pointer und `aligned_height = (data[1] - data[0]) / linesize[0]` —
//! niemals eine aus `height` abgeleitete Annahme. Renderer müssen Chroma über
//! `planes[1].data` lesen, nicht über `planes[0] + height * stride`.
//!
//! ## Lifetime-Vertrag
//! `DecodedFrame.planes` zeigen in FFmpegs Frame-Pool (bzw. in den sws-Ziel-
//! Puffer des Decoders). Der Frame bleibt gültig **bis zum nächsten
//! `decode_sample()`/`decode_packet()`-Aufruf desselben Decoders oder bis zum
//! Decoder-Drop** (C: Frame-Release beim nächsten decode). Ein `DecodedFrame`
//! darf nicht über den nächsten Decode-Aufruf hinaus verwendet werden.

use std::os::raw::c_int;
use std::ptr::{self, NonNull};

use chiaki_core::error::Codec;
use chiaki_core::{ChiakiError, ChiakiResult};
use num_enum::TryFromPrimitive;

use crate::ffmpeg::{self, sys};

/// Hardware-Backend für den Videodekoder.
///
/// `Auto` probiert nacheinander Cuda → D3D11Va → Vulkan und fällt bei
/// Nichtverfügbarkeit auf Software zurück (wie der C-Client, der ohne
/// `hw_decoder_name` software-dekodiert).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HwBackend {
    Auto,
    None,
    Cuda,
    D3D11Va,
    Vulkan,
}

impl HwBackend {
    /// Name für `av_hwdevice_find_type_by_name`.
    fn device_name(self) -> Option<&'static str> {
        match self {
            HwBackend::Cuda => Some("cuda"),
            HwBackend::D3D11Va => Some("d3d11va"),
            HwBackend::Vulkan => Some("vulkan"),
            HwBackend::Auto | HwBackend::None => None,
        }
    }
}

/// Pixelformat des dekodierten Frames (Subset der AVPixelFormat-Werte).
#[derive(Debug, Clone, Copy, PartialEq, Eq, TryFromPrimitive)]
#[repr(i32)]
pub enum FrameFormat {
    /// Software-Pfad vor der NV12-Konvertierung (intern).
    Yuv420P = sys::AV_PIX_FMT_YUV420P,
    /// 4:2:0, Y-Plane + interleaved UV-Plane — der DecodedFrame-Vertrag.
    Nv12 = sys::AV_PIX_FMT_NV12,
}

/// Speicherort der Frame-Daten (GPU-Pfad: [`FrameMemory::CudaDevice`]/
/// [`FrameMemory::D3d11Texture`] — dann NICHT in Systemspeicher transferiert;
/// siehe [`crate::decoder::Decoder::with_raw_hw_output`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMemory {
    /// `planes[0..1]` zeigen in Systemspeicher (klassischer Pfad).
    Cpu,
    /// `planes[0..1]` sind CUDA-Device-Pointer (Y, UV) — NVDEC-CUDA-Raw-Output
    /// (`AV_PIX_FMT_CUDA`). Nur im Decoder-Kontext gültig (VSR/Kopien).
    CudaDevice,
    /// `data[0]` = ID3D11Texture2D* (NV12-Array), `subresource` = Array-Index
    /// (FFmpeg: `data[1] as intptr_t`) — D3D11VA-Raw-Output.
    D3d11Texture {
        texture: *mut std::os::raw::c_void,
        subresource: u32,
    },
}

/// Eine Bild-Ebene: echter Pointer + Zeilenabstand (Bytes).
///
/// Bewusst roh (siehe Moduldokumentation): der Renderer braucht die echten
/// NVDEC-Pointer, um die aligned-height-Falle zu umgehen.
#[derive(Debug, Clone, Copy)]
pub struct Plane {
    /// Echter Plane-Start (`AVFrame.data[i]` bzw. sws-Zielbuffer).
    pub data: NonNull<u8>,
    /// Zeilenabstand in Bytes (`AVFrame.linesize[i]`).
    pub stride: usize,
}

impl Plane {
    /// Plane-Datenpointer (nur lesen; Gültigkeit siehe Lifetime-Vertrag).
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Mutable Plane-Datenpointer (z. B. für Render-Staging).
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.data.as_ptr()
    }
}

impl DecodedFrame {
    /// CPU-Frames: Planes als sichere Slices `(Y, UV)` — `stride*Zeilen`
    /// Bytes je Ebene (UV hat halbe Zeilenzahl). `None` bei GPU-Frames
    /// ([`FrameMemory::CudaDevice`]/[`FrameMemory::D3d11Texture`]).
    ///
    /// SAFETY-Hinweis: Die Slices lesen die echten NVDEC-/FFmpeg-Pool-Pointer
    /// für `stride * Zeilen` Bytes — der Pool gilt nur bis zum nächsten
    /// `decode_*`-Aufruf (Lifetime-Vertrag der Struktur).
    pub fn nv12_cpu_planes(&self) -> Option<(&[u8], &[u8])> {
        if self.memory != FrameMemory::Cpu {
            return None;
        }
        let h = self.height as usize;
        let (ys, uvs) = (self.planes[0].stride, self.planes[1].stride);
        unsafe {
            let y = std::slice::from_raw_parts(self.planes[0].as_ptr(), ys * h);
            let uv = std::slice::from_raw_parts(self.planes[1].as_ptr(), uvs * (h / 2));
            Some((y, uv))
        }
    }
}

/// Dekodierter Frame — CPU-Pfad: immer NV12 (siehe Moduldokumentation);
/// GPU-Pfad ([`FrameMemory::CudaDevice`]/[`FrameMemory::D3d11Texture`]):
/// Daten bleiben auf der GPU (raw HW-Output).
#[derive(Debug, Clone, Copy)]
pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
    /// `[0]` = Y, `[1]` = interleaved UV (NV12). Echte Pointer, siehe
    /// aligned-height-Hinweis oben. Bei [`FrameMemory::CudaDevice`] sind es
    /// CUDA-Device-Pointer; bei [`FrameMemory::D3d11Texture`] tragen sie die
    /// Textur-Pointer (siehe `memory`).
    pub planes: [Plane; 2],
    /// `(data[1] - data[0]) / linesize[0]` — kann > height sein (NVDEC-Alignment,
    /// z. B. 1088 bei 1080p). Chroma-Zeilenhöhe ist `aligned_height / 2`.
    pub aligned_height: u32,
    /// Wall-Clock-PTS in Sekunden (synthetisches Timing wie im C-Decoder).
    pub pts: f64,
    /// Frame-Dauer in Sekunden (synthetisch, wenn FFmpeg keine liefert).
    pub duration: f64,
    /// Verlorene Frames seit dem letzten gelieferten Frame
    /// (C: `chiaki_ffmpeg_decoder_pull_frame(*frames_lost)`).
    pub frames_lost: i32,
    /// Frame nach FEC-Recovery (C: `frame->decode_error_flags |= 1`).
    pub recovered: bool,
    /// Speicherort der Frame-Daten (CPU-Pfad: [`FrameMemory::Cpu`]).
    pub memory: FrameMemory,
}

/// NV12-aligned-height aus den ECHTEN Pointer-Differenzen berechnen
/// (REWORK.md: `alignedHeight = (data[1] - data[0]) / linesize[0]`).
///
/// Pure Funktion, damit die Logik ohne Pointer testbar ist. `None` bei
/// unsinniger Kombination (stride 0, Chroma vor Luma). Die Division ist
/// wie im C GANZZAHLIG (floor): echte NVDEC-Surfaces haben zwischen Y und
/// UV ein Padding, das nicht zeilenbündig sein kann — die überzähligen
/// Bytes werden (wie im C) nicht gelesen.
pub fn nv12_aligned_height(
    luma_addr: usize,
    chroma_addr: usize,
    luma_stride: usize,
) -> Option<u32> {
    if luma_stride == 0 || chroma_addr < luma_addr {
        return None;
    }
    let diff = chroma_addr - luma_addr;
    Some((diff / luma_stride) as u32)
}

/// Synthetisches PTS-/Frametiming — 1:1-Port des adaptiven Durations-Schätzers
/// aus `chiaki_ffmpeg_decoder_video_sample_cb` (chiaki-ng).
///
/// Der C-Decoder baut aus den beobachteten Sample-Abständen eine glatte
/// Frame-Dauer (mit 20 %-Toleranzband und 3-Frame-Kandidaten-Adoption) und
/// vergibt monoton steigende PTS in Mikrosekunden (`time_base` 1/1000000).
#[derive(Debug, Clone)]
pub(crate) struct SyntheticTiming {
    framerate_num: i32,
    frame_duration_us: f64,
    candidate_duration_us: f64,
    last_sample_time_us: u64,
    candidate_count: u8,
    packet_pts: i64,
}

impl SyntheticTiming {
    pub(crate) fn new(max_fps: u32) -> Self {
        let fps = if max_fps > 0 { max_fps as i32 } else { 60 };
        let duration = Self::default_frame_duration_us(fps);
        SyntheticTiming {
            framerate_num: fps,
            frame_duration_us: duration,
            candidate_duration_us: duration,
            last_sample_time_us: 0,
            candidate_count: 0,
            packet_pts: 0,
        }
    }

    /// C: `chiaki_ffmpeg_decoder_default_frame_duration_us`.
    pub(crate) fn default_frame_duration_us(max_fps: i32) -> f64 {
        let fps = if max_fps > 0 { max_fps as f64 } else { 60.0 };
        1_000_000.0 / fps
    }

    fn observed_clamped(&self, observed: f64) -> f64 {
        let default = Self::default_frame_duration_us(self.framerate_num);
        let max = 1_000_000.0 / 15.0;
        observed.max(default).min(max)
    }

    /// Timing-Buchhaltung beim Eingang eines (rekombinierten) Frames.
    /// Port des Schätzer-Blocks; liefert `(packet_pts, duration_pts)` für das
    /// AVPacket. `packet_pts` wird (inkl. verlorener Frames) anschließend vom
    /// Aufrufer via `advance_packet` weitergezählt — zusammen exakt der C-Flow.
    pub(crate) fn packet_timing(&mut self, now_us: u64, frames_lost: i32) -> (i64, i64) {
        if self.last_sample_time_us != 0 {
            let mut observed = (now_us.saturating_sub(self.last_sample_time_us)) as f64;
            let delivered_frames = frames_lost as i64 + 1;
            if delivered_frames > 1 {
                observed /= delivered_frames as f64;
            }
            // C: clamp auf [default_duration, 1/15 s]
            let observed = self.observed_clamped(observed);

            let candidate_diff = if self.candidate_duration_us > 0.0 {
                (observed - self.candidate_duration_us).abs() / self.candidate_duration_us
            } else {
                1.0
            };
            let current_diff = if self.frame_duration_us > 0.0 {
                (observed - self.frame_duration_us).abs() / self.frame_duration_us
            } else {
                1.0
            };
            if current_diff >= 0.20 {
                if candidate_diff <= 0.10 {
                    self.candidate_count += 1;
                } else {
                    self.candidate_duration_us = observed;
                    self.candidate_count = 1;
                }
                if self.candidate_count >= 3 {
                    self.frame_duration_us = self.candidate_duration_us;
                    self.candidate_count = 0;
                }
            } else {
                self.candidate_duration_us = self.frame_duration_us;
                self.candidate_count = 0;
            }
        }
        self.last_sample_time_us = now_us;

        let synthetic_duration_pts = (self.frame_duration_us + 0.5) as i64;
        let synthetic_duration_pts = synthetic_duration_pts.max(1);
        if frames_lost > 0 {
            self.packet_pts += synthetic_duration_pts * frames_lost as i64;
        }
        let pts = self.packet_pts;
        (pts, synthetic_duration_pts)
    }

    /// C: `decoder->synthetic_packet_pts += synthetic_duration_pts;` nach dem Senden.
    pub(crate) fn advance_packet(&mut self, duration_pts: i64) {
        self.packet_pts += duration_pts;
    }

    /// Aktuelle synthetische Frame-Dauer in Sekunden (C: `synthetic_duration`).
    pub(crate) fn duration_secs(&self) -> f64 {
        self.frame_duration_us / 1_000_000.0
    }
}

/// Optionen für den GPU-Pfad ([`Decoder::new_opts`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct DecoderOpts {
    /// HW-Frames NICHT in Systemspeicher transferieren — [`DecodedFrame`]
    /// erhält CUDA-Device-Pointer (Backend Cuda) bzw. die D3D11-NV12-Textur
    /// mit Array-Index (Backend D3D11Va). Software-Pfade liefern weiterhin CPU.
    pub raw_hw_output: bool,
    /// Externes D3D11-Device (ID3D11Device*) + Immediate-Context — D3D11VA
    /// dekodiert dann auf DEMSELBEN Device wie der GPU-Sink (Voraussetzung
    /// für geräteinternes CopySubresourceRegion). Nur mit Backend D3D11Va.
    pub d3d11_device: *mut std::os::raw::c_void,
    pub d3d11_device_context: *mut std::os::raw::c_void,
}

/// FFmpeg-Videodekoder (H264/H265, HW via NVDEC/D3D11VA/Vulkan oder Software).
///
/// Nicht `Sync` — FFmpeg-Codec-Kontexte sind nicht threadsicher; der C-Client
/// schützt den Decoder mit derselben Mutex über send/pull
/// (`chiaki_ffmpeg_decoder_video_sample_cb` vs. `pull_frame`).
pub struct Decoder {
    lib: &'static ffmpeg::FfmpegLib,
    codec_ctx: NonNull<sys::AVCodecContext>,
    hw_device_ctx: *mut sys::AVBufferRef,
    hw_pix_fmt: c_int,
    hw_backend: Option<HwBackend>,
    /// Aufgelöste Pseudoformate (per Name, siehe `new_opts`).
    cuda_pix_fmt: c_int,
    d3d11_pix_fmt: c_int,
    opts: DecoderOpts,
    /// Zwei rotierende AVFrames — Port des frame_last/frame-Ping-Pongs aus
    /// `chiaki_ffmpeg_decoder_pull_frame` (immer nur der letzte Frame lebt).
    frames: [NonNull<sys::AVFrame>; 2],
    scratch_slot: usize,
    transfer_frame: NonNull<sys::AVFrame>,
    /// Tempframe für den EAGAIN-Pfad (C: av_frame_alloc/free pro Vorkommnis).
    drop_frame: NonNull<sys::AVFrame>,
    packet: NonNull<sys::AVPacket>,
    /// sws-Kontext für den Software-Fallback (YUV420P → NV12).
    sws: Option<NonNull<sys::SwsContext>>,
    sws_size: (u32, u32),
    /// Zielbuffer der NV12-Konvertierung (Lifetime-Vertrag wie Frame-Pool).
    nv12_buf: Vec<u8>,
    timing: SyntheticTiming,
    frames_lost_total: i32,
    frame_recovered: bool,
}

// SAFETY: Die rohen FFmpeg-Handles sind besitzend und movable; FFmpeg-Codec-
// Kontexte dürfen zwischen Threads *übergeben* (nicht parallel genutzt) werden —
// wie jeder andere nicht-Sync-Rust-Typ.
unsafe impl Send for Decoder {}

impl Decoder {
    /// Port von `chiaki_ffmpeg_decoder_init`.
    ///
    /// `max_fps` stammt aus dem VideoProfile (C: `connect_info.video_profile.max_fps`)
    /// und treibt das synthetische Timing; 0 wird wie im C als 60 interpretiert.
    pub fn new(codec: Codec, hw_backend: HwBackend, max_fps: u32) -> ChiakiResult<Decoder> {
        Self::new_opts(codec, hw_backend, max_fps, DecoderOpts::default())
    }

    /// Wie [`Decoder::new`] mit GPU-Pfad-Optionen (raw HW-Output, externes
    /// D3D11-Device für D3D11VA).
    pub fn new_opts(
        codec: Codec,
        hw_backend: HwBackend,
        max_fps: u32,
        opts: DecoderOpts,
    ) -> ChiakiResult<Decoder> {
        let lib = ffmpeg::init()?;
        let api = lib.api();

        // HW-Pseudoformate zur Laufzeit per Name auflösen (kein Enum-Pinning).
        let cuda_pix_fmt = unsafe {
            let name = b"cuda\0";
            (api.av_get_pix_fmt)(name.as_ptr() as *const std::os::raw::c_char)
        };
        let d3d11_pix_fmt = unsafe {
            let name = b"d3d11\0";
            (api.av_get_pix_fmt)(name.as_ptr() as *const std::os::raw::c_char)
        };

        if codec.is_hdr() {
            // Der DecodedFrame-Vertrag ist NV12 (8 bit). HDR (H265 + P010LE/
            // YUV420P10LE) bräuchte einen 10-bit-Pfad — bewusst abgelehnt statt
            // farbfalsch zu liefern (offener Punkt, siehe Crate-Doku).
            tracing::error!(
                "Decoder: codec {} requires HDR (P010) support which is not implemented",
                codec
            );
            return Err(ChiakiError::Unknown);
        }

        // C: chiaki_codec_av_codec_id — H265/HDR → AV_CODEC_ID_H265, sonst H264.
        let av_codec_id = if codec.is_h265() {
            sys::AV_CODEC_ID_HEVC
        } else {
            sys::AV_CODEC_ID_H264
        };

        unsafe {
            let av_codec = (api.avcodec_find_decoder)(av_codec_id);
            if av_codec.is_null() {
                tracing::error!("{} Codec not available", codec);
                return Err(ChiakiError::Unknown);
            }

            let codec_ctx =
                NonNull::new((api.avcodec_alloc_context3)(av_codec)).ok_or_else(|| {
                    tracing::error!("Failed to alloc codec context");
                    ChiakiError::Memory
                })?;

            // HW-Setup (C: hw_decoder_name-Block). Abweichung zum C: schlägt ein
            // Wunsch-Backend fehl, wird mit Warnung Software genutzt, statt die
            // Session zu killen ("Software-Fallback wenn HW nicht verfügbar").
            let mut hw_device_ctx: *mut sys::AVBufferRef = ptr::null_mut();
            let mut hw_pix_fmt = sys::AV_PIX_FMT_NONE;
            let mut hw_used: Option<HwBackend> = None;

            let backends: &[HwBackend] = match hw_backend {
                HwBackend::None => &[],
                HwBackend::Auto => &[HwBackend::Cuda, HwBackend::D3D11Va, HwBackend::Vulkan],
                other => &[other],
            };
            for &backend in backends {
                match Self::setup_hw_device(
                    api,
                    av_codec,
                    codec_ctx,
                    backend,
                    opts,
                    &mut hw_device_ctx,
                    &mut hw_pix_fmt,
                ) {
                    Ok(()) => {
                        hw_used = Some(backend);
                        break;
                    }
                    Err(msg) => {
                        tracing::warn!(
                            "Hardware decoder \"{}\" not usable: {}",
                            backend.device_name().unwrap_or("?"),
                            msg
                        );
                        hw_device_ctx = ptr::null_mut();
                        hw_pix_fmt = sys::AV_PIX_FMT_NONE;
                    }
                }
            }

            // Synthetic timing defaults (C: synthetic_framerate/time_base).
            (*codec_ctx.as_ptr()).framerate = sys::AVRational::new(max_fps.max(1) as c_int, 1);
            (*codec_ctx.as_ptr()).pkt_timebase = sys::AVRational::new(1, 1_000_000);
            (*codec_ctx.as_ptr()).time_base = sys::AVRational::new(1, 1_000_000);

            if (api.avcodec_open2)(codec_ctx.as_ptr(), av_codec, ptr::null_mut()) < 0 {
                tracing::error!("Failed to open codec context");
                if !hw_device_ctx.is_null() {
                    (api.av_buffer_unref)(&mut hw_device_ctx);
                }
                (api.avcodec_free_context)(&mut codec_ctx.as_ptr());
                return Err(ChiakiError::Unknown);
            }

            // ABI-Canary: Wir haben pkt_timebase über das transkribierte Struct
            // gesetzt — liest es identisch zurück, stimmen unsere AVCodecContext-
            // Offsets mit der geladenen DLL überein.
            let tb = (*codec_ctx.as_ptr()).pkt_timebase;
            if tb != sys::AVRational::new(1, 1_000_000) {
                tracing::error!(
                    "AVCodecContext layout mismatch (pkt_timebase={:?}) — FFmpeg version not supported",
                    tb
                );
                if !hw_device_ctx.is_null() {
                    (api.av_buffer_unref)(&mut hw_device_ctx);
                }
                (api.avcodec_free_context)(&mut codec_ctx.as_ptr());
                return Err(ChiakiError::VersionMismatch);
            }

            if let Some(backend) = hw_used {
                tracing::info!(
                    "Using hardware decoder \"{}\" with pix_fmt={}",
                    backend.device_name().unwrap_or("?"),
                    ffmpeg::pix_fmt_name(api, hw_pix_fmt)
                );
            } else {
                tracing::info!("Using software decoder for {}", codec);
            }

            let frame0 = NonNull::new((api.av_frame_alloc)());
            let frame1 = NonNull::new((api.av_frame_alloc)());
            let transfer_frame = NonNull::new((api.av_frame_alloc)());
            let drop_frame = NonNull::new((api.av_frame_alloc)());
            let packet = NonNull::new((api.av_packet_alloc)());
            let (frame0, frame1, transfer_frame, drop_frame, packet) =
                match (frame0, frame1, transfer_frame, drop_frame, packet) {
                    (Some(f0), Some(f1), Some(tf), Some(df), Some(pkt)) => (f0, f1, tf, df, pkt),
                    (f0, f1, tf, df, pkt) => {
                        // Teilausgaben freigeben (C: Fehlerkette mit av_frame_free).
                        for f in [f0, f1, tf, df].into_iter().flatten() {
                            (api.av_frame_free)(&mut f.as_ptr());
                        }
                        if let Some(pkt) = pkt {
                            (api.av_packet_free)(&mut pkt.as_ptr());
                        }
                        return Err(ChiakiError::Memory);
                    }
                };

            Ok(Decoder {
                lib,
                codec_ctx,
                hw_device_ctx,
                hw_pix_fmt,
                hw_backend: hw_used,
                cuda_pix_fmt,
                d3d11_pix_fmt,
                opts,
                frames: [frame0, frame1],
                scratch_slot: 0,
                transfer_frame,
                drop_frame,
                packet,
                sws: None,
                sws_size: (0, 0),
                nv12_buf: Vec::new(),
                timing: SyntheticTiming::new(max_fps),
                frames_lost_total: 0,
                frame_recovered: false,
            })
        }
    }

    /// C: HW-Config-Suche + `av_hwdevice_ctx_create` + `ctx->hw_device_ctx = av_buffer_ref(...)`.
    ///
    /// D3D11VA-Sonderweg mit externem Device (GPU-Pfad): `av_hwdevice_ctx_alloc`
    /// → `AVD3D11VADeviceContext` mit Sink-Device/Context füllen →
    /// `av_hwdevice_ctx_init` — FFmpeg dekodiert dann auf DEMSELBEN D3D11-Device
    /// wie der GPU-Sink (Voraussetzung für geräteinternes CopySubresourceRegion).
    fn setup_hw_device(
        api: &sys::Api,
        av_codec: *const sys::AVCodec,
        codec_ctx: NonNull<sys::AVCodecContext>,
        backend: HwBackend,
        opts: DecoderOpts,
        hw_device_ctx: &mut *mut sys::AVBufferRef,
        hw_pix_fmt: &mut c_int,
    ) -> Result<(), String> {
        let name = backend.device_name().unwrap_or_default();
        unsafe {
            let name_c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
            let dev_type = (api.av_hwdevice_find_type_by_name)(name_c.as_ptr());
            if dev_type == sys::AV_HWDEVICE_TYPE_NONE {
                return Err(format!("Hardware decoder \"{name}\" not found"));
            }

            let mut found = false;
            let mut i: c_int = 0;
            while !found {
                let config = (api.avcodec_get_hw_config)(av_codec, i);
                if config.is_null() {
                    return Err("avcodec_get_hw_config failed".to_string());
                }
                let config = &*config;
                if config.methods & sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX != 0
                    && config.device_type == dev_type
                {
                    *hw_pix_fmt = config.pix_fmt;
                    found = true;
                }
                i += 1;
            }

            if hw_device_ctx.is_null() {
                let external_d3d11 = backend == HwBackend::D3D11Va && !opts.d3d11_device.is_null();
                if external_d3d11 {
                    // Externes Device: alloc → hwctx füllen → init.
                    let dev_ref = (api.av_hwdevice_ctx_alloc)(sys::AV_HWDEVICE_TYPE_D3D11VA);
                    if dev_ref.is_null() {
                        return Err("av_hwdevice_ctx_alloc failed".to_string());
                    }
                    let dev = (*dev_ref).data as *mut sys::AVHWDeviceContext;
                    if dev.is_null() {
                        let mut tmp = dev_ref;
                        (api.av_buffer_unref)(&mut tmp);
                        return Err("hwdevice context data is null".to_string());
                    }
                    let hwctx = (*dev).hwctx as *mut sys::AVD3D11VADeviceContext;
                    if hwctx.is_null() {
                        let mut tmp = dev_ref;
                        (api.av_buffer_unref)(&mut tmp);
                        return Err("D3D11VA hwctx is null".to_string());
                    }
                    *hwctx = sys::AVD3D11VADeviceContext {
                        device: opts.d3d11_device,
                        device_context: opts.d3d11_device_context,
                        lock: None,
                        unlock: None,
                        lock_ctx: std::ptr::null_mut(),
                    };
                    if (api.av_hwdevice_ctx_init)(dev_ref) < 0 {
                        let mut tmp = dev_ref;
                        (api.av_buffer_unref)(&mut tmp);
                        return Err("av_hwdevice_ctx_init (external D3D11 device) failed".to_string());
                    }
                    *hw_device_ctx = dev_ref;
                } else if (api.av_hwdevice_ctx_create)(
                    hw_device_ctx,
                    dev_type,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                ) < 0
                {
                    return Err("Failed to create hwdevice context".to_string());
                }
            }
            let ctx_ref = (api.av_buffer_ref)(*hw_device_ctx);
            if ctx_ref.is_null() {
                return Err("av_buffer_ref failed".to_string());
            }
            // Übernahme in den Codec-Kontext (Referenz geht in dessen Obhut).
            (*codec_ctx.as_ptr()).hw_device_ctx = ctx_ref;
            Ok(())
        }
    }

    /// Dekodiert einen kompletten Frame (Access Unit) — Port von
    /// `chiaki_ffmpeg_decoder_video_sample_cb` + `pull_frame` in einem Aufruf.
    ///
    /// `frames_lost`/`frame_recovered` stammen aus dem Videoreceiver
    /// (C: Signaturend-Parameter). Liefert `Ok(None)`, wenn der Decoder noch
    /// keinen Frame ausgibt (z. B. vor dem ersten IDR).
    pub fn decode_sample(
        &mut self,
        data: &[u8],
        frames_lost: i32,
        frame_recovered: bool,
    ) -> ChiakiResult<Option<DecodedFrame>> {
        let api = self.lib.api();
        unsafe {
            // --- C: video_sample_cb — Buchhaltung + Packet bauen + senden ---
            self.frames_lost_total += frames_lost;
            self.frame_recovered |= frame_recovered;

            let now_us = chiaki_core::time::now_us();
            let (pts, duration_pts) = self.timing.packet_timing(now_us, frames_lost);

            let packet = &mut *self.packet.as_ptr();
            packet.data = data.as_ptr() as *mut u8;
            packet.size = data.len() as c_int;
            packet.pts = pts;
            packet.dts = pts;
            packet.duration = duration_pts;
            packet.time_base = sys::AVRational::new(1, 1_000_000);
            self.timing.advance_packet(duration_pts);

            let ctx = self.codec_ctx.as_ptr();
            let mut r = (api.avcodec_send_packet)(ctx, self.packet.as_ptr());
            if r != 0 {
                if r == sys::AVERROR_EAGAIN {
                    tracing::warn!("AVCodec internal buffer is full, dropping a decoded frame to push new packet");
                    // C: einen Frame rausziehen und verwerfen, dann erneut senden.
                    let drop_frame = self.drop_frame.as_ptr();
                    (api.av_frame_unref)(drop_frame);
                    if (api.avcodec_receive_frame)(ctx, drop_frame) != 0 {
                        tracing::error!("Failed to pull frame from full codec buffer");
                        return Err(ChiakiError::Unknown);
                    }
                    self.frames_lost_total += 1;
                    r = (api.avcodec_send_packet)(ctx, self.packet.as_ptr());
                    if r != 0 {
                        tracing::error!("Failed to push frame: {}", ffmpeg::err_str(api, r));
                        return Err(ChiakiError::Unknown);
                    }
                } else {
                    tracing::error!("Failed to push frame: {}", ffmpeg::err_str(api, r));
                    return Err(ChiakiError::Unknown);
                }
            }

            // --- C: pull_frame — bis EAGAIN ziehen, nur der letzte Frame bleibt ---
            let mut filled: Option<usize> = None;
            let mut frame_last_slot: Option<usize> = None;
            let mut frame_slot: Option<usize> = None;
            loop {
                let next_slot = match frame_last_slot {
                    Some(ls) => {
                        (api.av_frame_unref)(self.frames[ls].as_ptr());
                        ls
                    }
                    None => {
                        let s = self.scratch_slot;
                        self.scratch_slot ^= 1;
                        s
                    }
                };
                frame_last_slot = frame_slot;
                frame_slot = Some(next_slot);
                let rr = (api.avcodec_receive_frame)(ctx, self.frames[next_slot].as_ptr());
                if rr != 0 {
                    if rr != sys::AVERROR_EAGAIN {
                        tracing::error!("Decoding with FFMPEG failed");
                        return Err(ChiakiError::Unknown);
                    }
                    break;
                }
                filled = Some(next_slot);
            }
            // Der leere Ziel-Slot des letzten Receive ist der Scratch für den nächsten Aufruf.
            self.scratch_slot = frame_slot.expect("loop runs at least once");

            let Some(slot) = filled else {
                // Noch kein Frame bereit (z. B. vor dem ersten IDR) — wie C: kein Frame.
                return Ok(None);
            };

            let lost = self.frames_lost_total;
            self.frames_lost_total = 0;
            let mut recovered = false;
            if self.frame_recovered {
                recovered = true;
                self.frame_recovered = false;
                (*self.frames[slot].as_ptr()).decode_error_flags |= 1;
            }

            // --- HW-Frame in Systemspeicher übertragen (NVDEC aligned height bleibt
            // über die echten Plane-Pointer/aligned_height sichtbar). Im RAW-Modus
            // (GPU-Pfad) bleibt der Frame auf der GPU: CUDA → Device-Pointer,
            // D3D11VA → NV12-Array-Textur + Index (siehe build_decoded_frame). ---
            let src = &*self.frames[slot].as_ptr();
            let src_is_cuda_raw = self.opts.raw_hw_output
                && src.format == self.cuda_pix_fmt
                && self.cuda_pix_fmt >= 0;
            let src_is_d3d11_raw = self.opts.raw_hw_output
                && src.format == self.d3d11_pix_fmt
                && self.d3d11_pix_fmt >= 0;
            let raw_hw = src_is_cuda_raw || src_is_d3d11_raw;
            if self.hw_backend.is_some() && src.format == self.hw_pix_fmt && !raw_hw {
                let tf = self.transfer_frame.as_ptr();
                (api.av_frame_unref)(tf);
                (*tf).format = sys::AV_PIX_FMT_NV12;
                (*tf).width = src.width;
                (*tf).height = src.height;
                if (api.av_frame_get_buffer)(tf, 0) < 0 {
                    tracing::error!("Failed to allocate transfer buffer for hw frame");
                    return Err(ChiakiError::Unknown);
                }
                let tr = (api.av_hwframe_transfer_data)(tf, self.frames[slot].as_ptr(), 0);
                if tr < 0 {
                    tracing::error!(
                        "av_hwframe_transfer_data failed: {}",
                        ffmpeg::err_str(api, tr)
                    );
                    return Err(ChiakiError::Unknown);
                }
            }

            // Roh-Pointer (kein &self-Borrow), damit build_decoded_frame &mut self
            // für den sws-Puffer nehmen darf.
            let frame_ptr: *const sys::AVFrame = if raw_hw {
                self.frames[slot].as_ptr()
            } else if self.is_hw() && (*self.frames[slot].as_ptr()).format == self.hw_pix_fmt {
                self.transfer_frame.as_ptr()
            } else {
                self.frames[slot].as_ptr()
            };
            let out = self.build_decoded_frame(frame_ptr, lost, recovered)?;
            Ok(Some(out))
        }
    }

    /// Wie `decode_sample`, aber ohne Receiver-Statistik (C-Default 0/false).
    pub fn decode_packet(&mut self, data: &[u8]) -> ChiakiResult<Option<DecodedFrame>> {
        self.decode_sample(data, 0, false)
    }

    /// Backend, das tatsächlich benutzt wird (`None` = Software).
    pub fn used_hw_backend(&self) -> Option<HwBackend> {
        self.hw_backend
    }

    /// CUDA-Kontext des HW-Decoders (`AVCUDADeviceContext.cuda_ctx`) — für
    /// kontextgebundene CUDA-Nutzer wie den VSR-Upscaler
    /// ([`crate::vsr::VsrUpscaler::init`]).
    ///
    /// Im C++ (`vsrupscaler.cpp`) wird derselbe Kontext pro Frame aus
    /// `AVFrame.hw_frames_ctx → AVHWFramesContext.device_ref →
    /// AVHWDeviceContext.hwctx` gelesen; das ist dieselbe `AVHWDeviceContext`,
    /// die dieser Decoder via `av_hwdevice_ctx_create` erzeugt und in
    /// `hw_device_ctx` hält — wir lesen sie direkt und kontextunabhängig vom
    /// ersten Frame. `None` ohne CUDA-HW-Backend (Software/D3D11VA/Vulkan).
    pub fn cuda_context(&self) -> Option<*mut std::os::raw::c_void> {
        self.cuda_hwctx().map(|h| h.cuda_ctx)
    }

    /// CUDA-Stream des HW-Decoders (`AVCUDADeviceContext.cuda_stream`) —
    /// optional (FFmpeg nutzt den Default-Stream = NULL, VSR funktioniert
    /// dann mit `cuda_stream = NULL`).
    pub fn cuda_stream(&self) -> Option<*mut std::os::raw::c_void> {
        self.cuda_hwctx().map(|h| h.cuda_stream)
    }

    /// `AVCUDADeviceContext`-View aus dem eigenen `hw_device_ctx`, nur bei
    /// `AV_HWDEVICE_TYPE_CUDA` (D3D11VA/Vulkan-hwctx haben ein anderes Layout!).
    fn cuda_hwctx(&self) -> Option<&sys::AVCUDADeviceContext> {
        if self.hw_device_ctx.is_null() {
            return None;
        }
        unsafe {
            // SAFETY: hw_device_ctx lebt so lange wie der Decoder (Drop unref't
            // es); data zeigt auf die von FFmpeg allozierte AVHWDeviceContext.
            let dev = (*self.hw_device_ctx).data as *const sys::AVHWDeviceContext;
            if dev.is_null() || (*dev).type_ != sys::AV_HWDEVICE_TYPE_CUDA {
                return None;
            }
            let hwctx = (*dev).hwctx as *const sys::AVCUDADeviceContext;
            if hwctx.is_null() {
                None
            } else {
                Some(&*hwctx)
            }
        }
    }

    /// Aktive HW-Nutzung (entscheidet über den Transfer-Pfad).
    fn is_hw(&self) -> bool {
        self.hw_backend.is_some()
    }

    /// DecodedFrame aus einem NV12- oder YUV420P-Frame bauen (letzteres via
    /// swscale nach NV12 in den eigenen Puffer).
    ///
    /// # Safety
    /// `frame_ptr` muss auf einen von diesem Decoder gehaltenen, gefüllten
    /// AVFrame zeigen (Frame-Pool-Slot oder Transfer-Frame).
    unsafe fn build_decoded_frame(
        &mut self,
        frame_ptr: *const sys::AVFrame,
        frames_lost: i32,
        recovered: bool,
    ) -> ChiakiResult<DecodedFrame> {
        let frame = &*frame_ptr;
        let width = frame.width.max(0) as u32;
        let height = frame.height.max(0) as u32;
        if width == 0 || height == 0 || frame.data[0].is_null() {
            tracing::error!("Decoded frame has no image data ({}x{})", width, height);
            return Err(ChiakiError::Unknown);
        }

        let (planes, aligned_height, memory) = if frame.format == self.cuda_pix_fmt
            && self.cuda_pix_fmt >= 0
            && self.opts.raw_hw_output
        {
            // NVDEC-CUDA-Raw: data[0]/data[1] sind CUDA-Device-Pointer (Y/UV),
            // linesize[0] = Pitch (Referenz: C++ vsrupscaler.cpp, NVCV_MEM_GPU-
            // Views über data[0]/data[1]). aligned height wie bei CPU-Frames
            // aus der Pointer-Differenz (1080 → 1088-Alignment bleibt sichtbar).
            let stride0 = frame.linesize[0].max(0) as usize;
            let stride1 = if frame.linesize[1] != 0 {
                frame.linesize[1].max(0) as usize
            } else {
                stride0
            };
            let p0 = frame.data[0];
            let p1 = frame.data[1];
            if p0.is_null() || p1.is_null() || stride0 == 0 {
                tracing::error!(
                    "CUDA raw frame has unusable plane layout (data[0]={:p}, data[1]={:p}, pitch={})",
                    p0,
                    p1,
                    stride0
                );
                return Err(ChiakiError::Unknown);
            }
            let aligned = nv12_aligned_height(p0 as usize, p1 as usize, stride0)
                .ok_or_else(|| {
                    tracing::error!("CUDA raw plane layout unreadable");
                    ChiakiError::Unknown
                })?;
            (
                [
                    Plane {
                        data: NonNull::new_unchecked(p0),
                        stride: stride0,
                    },
                    Plane {
                        data: NonNull::new_unchecked(p1),
                        stride: stride1,
                    },
                ],
                aligned,
                FrameMemory::CudaDevice,
            )
        } else if frame.format == self.d3d11_pix_fmt
            && self.d3d11_pix_fmt >= 0
            && self.opts.raw_hw_output
        {
            // D3D11VA-Raw: data[0] = ID3D11Texture2D* (NV12-Array), data[1] =
            // Array-Index als intptr_t (FFmpeg-Vertrag). Die planes tragen die
            // Textur-Pointer als Opaque-Werte (Renderer liest `memory`).
            let texture = frame.data[0];
            let subresource = (frame.data[1] as usize) as u32;
            if texture.is_null() {
                tracing::error!("D3D11VA raw frame has null texture pointer");
                return Err(ChiakiError::Unknown);
            }
            let stride0 = frame.linesize[0].max(0) as usize;
            (
                [
                    Plane {
                        data: NonNull::new_unchecked(texture),
                        stride: stride0,
                    },
                    Plane {
                        data: NonNull::new_unchecked(frame.data[1]),
                        stride: stride0,
                    },
                ],
                height,
                FrameMemory::D3d11Texture {
                    texture: texture.cast(),
                    subresource,
                },
            )
        } else {
            let (planes, aligned_height) = self.build_cpu_planes(frame, width, height)?;
            (planes, aligned_height, FrameMemory::Cpu)
        };

        // C: chiaki_ffmpeg_frame_get_timing (Fallback-Kette pkt_timebase →
        // ctx time_base → 1/1e6; best_effort_timestamp → pts → 0). Liegt keine
        // Frame-Dauer vor, greift die synthetische Dauer (C: pull_frame-Override).
        let ctx = &*self.codec_ctx.as_ptr();
        let mut time_base = ctx.pkt_timebase;
        if !time_base.is_valid() {
            time_base = ctx.time_base;
        }
        if !time_base.is_valid() {
            time_base = sys::AVRational::new(1, 1_000_000);
        }
        let mut pts = frame.best_effort_timestamp;
        if pts == sys::AV_NOPTS_VALUE {
            pts = frame.pts;
        }
        if pts == sys::AV_NOPTS_VALUE {
            pts = 0;
        }
        let pts_secs = time_base.q2d() * pts as f64;
        let duration = if frame.duration > 0 {
            time_base.q2d() * frame.duration as f64
        } else {
            self.timing.duration_secs()
        };

        Ok(DecodedFrame {
            width,
            height,
            format: FrameFormat::Nv12,
            planes,
            aligned_height,
            pts: pts_secs,
            duration,
            frames_lost,
            recovered,
            memory,
        })
    }

    /// CPU-Planes: NV12-Frame-Pool-Pointer oder YUV420P→NV12 via swscale
    /// (Software-Pfad, Renderer-Vertrag "immer NV12 mit 2 Planes").
    unsafe fn build_cpu_planes(
        &mut self,
        frame: &sys::AVFrame,
        width: u32,
        height: u32,
    ) -> ChiakiResult<([Plane; 2], u32)> {
        let api = self.lib.api();
        match frame.format {
            sys::AV_PIX_FMT_NV12 => {
                let stride0 = frame.linesize[0].max(0) as usize;
                let stride1 = if frame.linesize[1] != 0 {
                    frame.linesize[1].max(0) as usize
                } else {
                    // REWORK.md: raw CUDA-Frames haben linesize[1] == 0 — UV-Pitch
                    // entspricht dann dem Y-Pitch.
                    stride0
                };
                let p0 = frame.data[0];
                let p1 = frame.data[1];
                let aligned =
                    nv12_aligned_height(p0 as usize, p1 as usize, stride0).ok_or_else(|| {
                        tracing::error!(
                            "NV12 plane layout unreadable (data[0]={:p}, data[1]={:p}, linesize[0]={})",
                            p0,
                            p1,
                            stride0
                        );
                        ChiakiError::Unknown
                    })?;
                Ok((
                    [
                        Plane {
                            data: NonNull::new_unchecked(p0),
                            stride: stride0,
                        },
                        Plane {
                            data: NonNull::new_unchecked(p1),
                            stride: stride1,
                        },
                    ],
                    aligned,
                ))
            }
            sys::AV_PIX_FMT_YUV420P => {
                // Software-Pfad: nach NV12 konvertieren (Renderer-Vertrag).
                let (w, h) = (width, height);
                if self.sws_size != (w, h) {
                    let ctx = (api.sws_get_context)(
                        w as c_int,
                        h as c_int,
                        sys::AV_PIX_FMT_YUV420P,
                        w as c_int,
                        h as c_int,
                        sys::AV_PIX_FMT_NV12,
                        sys::SWS_BILINEAR,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null(),
                    );
                    if ctx.is_null() {
                        tracing::error!("sws_getContext failed ({}x{} YUV420P→NV12)", w, h);
                        return Err(ChiakiError::Unknown);
                    }
                    if let Some(old) = self.sws.take() {
                        (api.sws_free_context)(old.as_ptr());
                    }
                    self.sws = Some(NonNull::new_unchecked(ctx));
                    self.sws_size = (w, h);
                }
                self.nv12_buf.resize(
                    (w as usize) * (h as usize) + (w as usize) * (h as usize).div_ceil(2),
                    0,
                );
                let buf_ptr = self.nv12_buf.as_mut_ptr();
                let uv_ptr = buf_ptr.add((w as usize) * (h as usize));
                let src: [*const u8; 4] = [
                    frame.data[0],
                    frame.data[1],
                    frame.data[2],
                    std::ptr::null(),
                ];
                let src_stride: [c_int; 4] =
                    [frame.linesize[0], frame.linesize[1], frame.linesize[2], 0];
                let dst: [*mut u8; 4] =
                    [buf_ptr, uv_ptr, std::ptr::null_mut(), std::ptr::null_mut()];
                let dst_stride: [c_int; 4] = [w as c_int, w as c_int, 0, 0];
                let r = (api.sws_scale)(
                    self.sws.expect("sws context just created").as_ptr(),
                    src.as_ptr(),
                    src_stride.as_ptr(),
                    0,
                    h as c_int,
                    dst.as_ptr(),
                    dst_stride.as_ptr(),
                );
                if r < 0 {
                    tracing::error!("sws_scale failed: {}", ffmpeg::err_str(api, r));
                    return Err(ChiakiError::Unknown);
                }
                Ok((
                    [
                        Plane {
                            data: NonNull::new_unchecked(buf_ptr),
                            stride: w as usize,
                        },
                        Plane {
                            data: NonNull::new_unchecked(uv_ptr),
                            stride: w as usize,
                        },
                    ],
                    h,
                ))
            }
            other => {
                tracing::error!(
                    "Unsupported pixel format {} (HDR/10-bit needs P010 support)",
                    ffmpeg::pix_fmt_name(api, other)
                );
                Err(ChiakiError::Unknown)
            }
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let api = self.lib.api();
        unsafe {
            let mut ctx = self.codec_ctx.as_ptr();
            (api.avcodec_free_context)(&mut ctx);
            if !self.hw_device_ctx.is_null() {
                (api.av_buffer_unref)(&mut self.hw_device_ctx);
            }
            for f in &mut self.frames {
                (api.av_frame_free)(&mut f.as_ptr());
            }
            let mut tf = self.transfer_frame.as_ptr();
            (api.av_frame_free)(&mut tf);
            let mut df = self.drop_frame.as_ptr();
            (api.av_frame_free)(&mut df);
            let mut pkt = self.packet.as_ptr();
            (api.av_packet_free)(&mut pkt);
            if let Some(sws) = self.sws.take() {
                (api.sws_free_context)(sws.as_ptr());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- nv12_aligned_height (REWORK.md Root Cause 1) -----------------------

    const PITCH_1080P: usize = 1920;
    const PITCH_720P: usize = 1280;

    #[test]
    fn aligned_height_nvdec_1080_is_1088() {
        // NVDEC aligned: UV liegt bei Y + pitch*1088 (nicht pitch*1080) —
        // genau der 1080p-Ghosting/Grün-Balken-Fall.
        let y = 0x1_0000_0000usize;
        let uv = y + PITCH_1080P * 1088;
        assert_eq!(nv12_aligned_height(y, uv, PITCH_1080P), Some(1088));
    }

    #[test]
    fn aligned_height_720_is_already_aligned() {
        // 720 ist 16-aligned — daher war 720p von der Falle "zufällig" immun.
        let y = 0x2000usize;
        let uv = y + PITCH_720P * 720;
        assert_eq!(nv12_aligned_height(y, uv, PITCH_720P), Some(720));
    }

    #[test]
    fn aligned_height_standard_sw_frame_equals_height() {
        // Von FFmpeg allozierte CPU-NV12-Frames: UV exakt hinter der Y-Ebene.
        let h = 540;
        let w = 960;
        let y = 0x10_000usize;
        let uv = y + w * h;
        assert_eq!(nv12_aligned_height(y, uv, w), Some(540));
    }

    #[test]
    fn aligned_height_rejects_nonsense() {
        assert_eq!(nv12_aligned_height(100, 200, 0), None); // stride 0
        assert_eq!(nv12_aligned_height(200, 100, 64), None); // UV vor Luma
    }

    #[test]
    fn aligned_height_nvdec_padding_floors_like_c() {
        // Echte NVDEC-Surface (Live-Messung 1080p): UV liegt 32 Bytes hinter
        // dem zeilenbündigen 1920×1088-Layout (Plane-Ausrichtung). Die alte
        // strenge Teilbarkeitsprüfung hat daran JEDEN Live-Frame verworfen;
        // das C rechnet (data[1]-data[0])/linesize GANZZAHLIG.
        let y = 0x2d720dc3080usize;
        let uv = 0x2d720fc10a0usize;
        let diff = uv - y;
        assert_eq!(diff, 1920 * 1088 + 32); // 32-Byte-Ausrichtung der UV-Plane
        let aligned = nv12_aligned_height(y, uv, 1920).unwrap();
        assert_eq!(aligned, 1088); // floor(diff / 1920) — wie im C
    }

    // --- SyntheticTiming (Port des adaptiven Durations-Schätzers) -----------

    fn timing_60() -> SyntheticTiming {
        SyntheticTiming::new(60)
    }

    #[test]
    fn initial_duration_is_1_over_fps() {
        assert!((SyntheticTiming::default_frame_duration_us(60) - 16666.666).abs() < 0.01);
        assert!((SyntheticTiming::default_frame_duration_us(30) - 33333.333).abs() < 0.01);
        assert!((SyntheticTiming::default_frame_duration_us(0) - 16666.666).abs() < 0.01);
        // → 60
    }

    #[test]
    fn first_packet_gets_pts_0_and_default_duration() {
        let mut t = timing_60();
        let (pts, dur) = t.packet_timing(1_000, 0);
        t.advance_packet(dur);
        assert_eq!(pts, 0);
        assert_eq!(dur, 16_667); // round(16666.67 + 0.5)
        let (pts2, _) = t.packet_timing(1_000 + 16_667, 0);
        assert_eq!(pts2, 16_667); // monoton weitergezählt
    }

    #[test]
    fn lost_frames_advance_pts_accordingly() {
        let mut t = timing_60();
        // reguläres Frame (C-Flow: packet_timing → advance_packet)
        let (_, dur) = t.packet_timing(1_000, 0);
        t.advance_packet(dur);
        // 3 Frames verloren → pts springt um 4*duration (lost*duration vorab + advance)
        let (pts, _) = t.packet_timing(1_000 + dur as u64, 3);
        assert_eq!(pts, 4 * dur);
    }

    #[test]
    fn duration_adopts_stable_30fps_after_three_candidates() {
        let mut t = timing_60();
        let mut now: u64 = 1_000_000;
        let (_, d0) = t.packet_timing(now, 0);
        t.advance_packet(d0);
        assert_eq!(d0, 16_667);
        // 6 Beobachtungen à 33,3 ms (30 fps): 3 bilden den Kandidaten,
        // mit der 3. konsistenten Beobachtung wird die Dauer übernommen.
        for _ in 0..6 {
            now += 33_333;
            let (_, d) = t.packet_timing(now, 0);
            t.advance_packet(d);
        }
        assert_eq!(t.packet_timing(now + 33_333, 0).1, 33_333);
        assert!((t.duration_secs() - 1.0 / 30.0).abs() < 1e-4);
    }

    #[test]
    fn observed_duration_clamped_to_15fps_upper_bound() {
        let mut t = timing_60();
        let mut now: u64 = 0;
        now += 300_000; // 300 ms → weit über 1/15 s Cap
        t.packet_timing(now, 0);
        // Dauer darf den Cap 1e6/15 nicht übernehmen (clamp gilt für observed,
        // der Schätzer sammelt aber nur Kandidaten innerhalb der Toleranz):
        // hier erreichen wir den Cap erst nach Kandidaten-Adoption — prüfe den Cap:
        for _ in 0..8 {
            now += 300_000;
            t.packet_timing(now, 0);
        }
        // 1e6/15 us Cap voll übernommen (nach Kandidaten-Adoption).
        assert!((t.duration_secs() - 1.0 / 15.0).abs() < 1e-4);
    }

    // --- Decoder-Init gegen die Referenz-DLLs (kein echtes Streaming nötig) --

    fn setup() {
        crate::test_setup::reference_dlls();
    }

    #[test]
    fn software_decoder_opens_for_h264_and_hevc() {
        setup();
        let d = Decoder::new(Codec::H264, HwBackend::None, 60).expect("h264 sw decoder");
        assert_eq!(d.used_hw_backend(), None);
        drop(d);
        let d = Decoder::new(Codec::H265, HwBackend::None, 60).expect("h265 sw decoder");
        assert_eq!(d.used_hw_backend(), None);
        drop(d);
    }

    #[test]
    fn auto_backend_falls_back_gracefully() {
        setup();
        // Auf jeder Windows-Maschine muss Auto eine lauffähige Kombination
        // finden (HW falls Treiber da ist, sonst Software) — ohne GPU nicht
        // unterscheidbar, aber der Open muss in beiden Fällen klappen.
        let d = Decoder::new(Codec::H264, HwBackend::Auto, 60).expect("auto decoder");
        tracing::info!("auto selected backend: {:?}", d.used_hw_backend());
    }

    #[test]
    fn hdr_is_rejected_by_nv12_contract() {
        setup();
        assert!(matches!(
            Decoder::new(Codec::H265Hdr, HwBackend::None, 60),
            Err(chiaki_core::ChiakiError::Unknown)
        ));
    }

    #[test]
    fn decode_without_real_stream_yields_no_frame_or_clean_error() {
        // Ein syntetisch zusammengewürfelter H264-Frame dekodiert nicht —
        // wir prüfen nur, dass der Pfad weder hängt noch UB zeigt.
        setup();
        let mut d = Decoder::new(Codec::H264, HwBackend::None, 60).expect("decoder");
        let junk = [0u8; 64];
        let _ = d.decode_packet(&junk);
        let _ = d.decode_sample(&junk, 2, true);
    }
}
