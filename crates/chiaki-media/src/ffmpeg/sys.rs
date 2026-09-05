// SPDX-License-Identifier: AGPL-3.0-only
//! Schmale, handgeschriebene FFI-Typen für FFmpeg (avutil-59 / avcodec-61 / swscale-8,
//! Referenz: FFmpeg n7.1 win64-gpl-shared).
//!
//! Die Structs sind **Views** (Rust alloziert sie nie selbst — sie werden von
//! `avcodec_alloc_context3` / `av_frame_alloc` / `av_packet_alloc` auf der C-Seite
//! erzeugt) und wurden Feld für Feld aus den Referenz-Headern transkribiert:
//! `libavcodec/avcodec.h`, `libavutil/frame.h`, `libavcodec/packet.h`,
//! `libavutil/rational.h`, `libavutil/buffer.h`, `libavutil/channel_layout.h`.
//! Die DLL-Major-Versionen werden beim Laden geprüft (siehe `ffmpeg::init`), damit
//! dieses Layout-Pinning nicht stillschweigend gegen andere Versionen läuft.
//!
//! `AVCodecContext` ist nach `hw_device_ctx` abgeschnitten (Rust-Seite liest/schreibt
//! nur Felder davor), `AVFrame` ist vollständig bis inkl. `duration` transkribiert,
//! da die Felder erst am Struct-Ende liegen.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

/// C: `#define AV_NUM_DATA_POINTERS 8`
pub const AV_NUM_DATA_POINTERS: usize = 8;

// ---------------------------------------------------------------------------
// Konstanten (Werte aus den Referenz-Headern, n7.1)
// ---------------------------------------------------------------------------

/// `AVERROR(EAGAIN)` — EAGAIN ist 11 unter Windows (MSVCRT/MinGW errno).
pub const AVERROR_EAGAIN: c_int = -11;
/// `AV_NOPTS_VALUE` = INT64_MIN.
pub const AV_NOPTS_VALUE: i64 = i64::MIN;

/// `AV_PIX_FMT_YUV420P` (libavutil/pixfmt.h, Enum-Index 0).
pub const AV_PIX_FMT_YUV420P: c_int = 0;
/// `AV_PIX_FMT_NV12` (Enum-Index 23).
pub const AV_PIX_FMT_NV12: c_int = 23;
/// `AV_PIX_FMT_NONE` = -1.
pub const AV_PIX_FMT_NONE: c_int = -1;

pub const AV_LOG_QUIET: c_int = -8;
pub const AV_LOG_PANIC: c_int = 0;
pub const AV_LOG_ERROR: c_int = 16;
pub const AV_LOG_WARNING: c_int = 24;
pub const AV_LOG_INFO: c_int = 32;
pub const AV_LOG_VERBOSE: c_int = 40;
pub const AV_LOG_DEBUG: c_int = 48;
pub const AV_LOG_TRACE: c_int = 56;

/// `AV_CODEC_ID_H264` (libavcodec/codec_id.h).
pub const AV_CODEC_ID_H264: c_int = 27;
/// `AV_CODEC_ID_HEVC` (= `AV_CODEC_ID_H265`, Makro im Header).
pub const AV_CODEC_ID_HEVC: c_int = 173;

/// `AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX = 0x01` (libavcodec/codec.h).
pub const AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX: c_int = 0x01;

/// `SWS_BILINEAR` (libswscale/swscale.h).
pub const SWS_BILINEAR: c_int = 2;

/// `AV_HWDEVICE_TYPE_NONE` (enum AVHWDeviceType, hwcontext.h).
pub const AV_HWDEVICE_TYPE_NONE: c_int = 0;
/// `AV_HWDEVICE_TYPE_CUDA` (hwcontext.h, Referenz-Header n7.1 — Reihenfolge:
/// NONE=0, VDPAU=1, CUDA=2, VAAPI=3, DXVA2=4, QSV=5, VIDEOTOOLBOX=6,
/// D3D11VA=7, DRM=8, OPENCL=9, MEDIACODEC=10, VULKAN=11, D3D12VA=12).
pub const AV_HWDEVICE_TYPE_CUDA: c_int = 2;

// ---------------------------------------------------------------------------
// Opaque Typen (nur als Pointer im Umlauf)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct AVBuffer {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVClass {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVCodec {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVCodecHWAccel {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVCodecInternal {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVDictionary {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVFrameSideData {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVChannelCustom {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct AVPacketSideData {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct RcOverride {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct SwsContext {
    _opaque: [u8; 0],
}
#[repr(C)]
pub struct SwsFilter {
    _opaque: [u8; 0],
}

// ---------------------------------------------------------------------------
// Kleine Wert-Typen
// ---------------------------------------------------------------------------

/// C: `typedef struct AVRational { int num; int den; }` (rational.h).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

impl AVRational {
    pub const fn new(num: c_int, den: c_int) -> Self {
        AVRational { num, den }
    }

    /// Port von `av_q2d()`.
    pub fn q2d(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// `time_base.num <= 0 || time_base.den <= 0` (C-Prüfung in
    /// `chiaki_ffmpeg_frame_get_timing`).
    pub fn is_valid(self) -> bool {
        self.num > 0 && self.den > 0
    }
}

/// C: `typedef struct AVBufferRef { AVBuffer *buffer; uint8_t *data; size_t size; }`.
#[repr(C)]
pub struct AVBufferRef {
    pub buffer: *mut AVBuffer,
    pub data: *mut u8,
    pub size: usize,
}

/// C: `typedef struct AVHWDeviceContext` (libavutil/hwcontext.h, avutil 59).
/// Wird von `av_hwdevice_ctx_create` alloziert — auch hier nur eine View.
#[repr(C)]
pub struct AVHWDeviceContext {
    pub av_class: *const AVClass,
    /// enum AVHWDeviceType
    pub type_: c_int,
    /// `AVCUDADeviceContext*` bei CUDA ( siehe unten), sonst hwcontext-spezifisch.
    pub hwctx: *mut c_void,
    pub internal: *mut AVBufferRef,
}

/// C: `typedef struct AVCUDADeviceContext` (libavutil/hwcontext_cuda.h).
/// Die ersten beiden Felder sind über alle FFmpeg-Versionen stabil (gleicher
/// Kommentar wie im C++-Original `vsrupscaler.cpp`); CUDA-Typen CUcontext/
/// CUstream sind opake Pointer.
#[repr(C)]
pub struct AVCUDADeviceContext {
    /// CUcontext
    pub cuda_ctx: *mut c_void,
    /// CUstream
    pub cuda_stream: *mut c_void,
    /// AVCUDADeviceContextInternal*
    pub internal: *mut c_void,
}

/// C: `typedef struct AVChannelLayout` (channel_layout.h) — Union u64/Pointer,
/// auf x64 8 Bytes, Struct gesamt 24 Bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AVChannelLayout {
    pub order: c_int,
    pub nb_channels: c_int,
    pub u: AVChannelLayoutU,
    pub opaque: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union AVChannelLayoutU {
    pub mask: u64,
    pub map: *mut AVChannelCustom,
}

/// C: `typedef struct AVCodecHWConfig` (codec.h) — vollständig.
#[repr(C)]
pub struct AVCodecHWConfig {
    pub pix_fmt: c_int,
    pub methods: c_int,
    pub device_type: c_int,
}

// ---------------------------------------------------------------------------
// AVFrame — vollständig bis inkl. `duration` (liegt am Struct-Ende)
// ---------------------------------------------------------------------------

/// C: `typedef struct AVFrame` (libavutil/frame.h, avutil 59 / n7.1).
/// Reihenfolge 1:1 aus dem Header; `FF_API_*`-Felder sind in 59 noch aktiv.
#[repr(C)]
pub struct AVFrame {
    pub data: [*mut u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    pub extended_data: *mut *mut u8,

    pub width: c_int,
    pub height: c_int,

    pub nb_samples: c_int,

    pub format: c_int,

    /// `FF_API_FRAME_KEY` (in 59 noch aktiv).
    pub key_frame: c_int,

    pub pict_type: c_int, // enum AVPictureType

    pub sample_aspect_ratio: AVRational,

    pub pts: i64,
    pub pkt_dts: i64,
    pub time_base: AVRational,

    pub quality: c_int,

    pub opaque: *mut c_void,

    pub repeat_pict: c_int,

    /// `FF_API_INTERLACED_FRAME` (in 59 noch aktiv).
    pub interlaced_frame: c_int,
    pub top_field_first: c_int,

    /// `FF_API_PALETTE_HAS_CHANGED` (in 59 noch aktiv).
    pub palette_has_changed: c_int,

    pub sample_rate: c_int,

    pub buf: [*mut AVBufferRef; AV_NUM_DATA_POINTERS],

    pub extended_buf: *mut *mut AVBufferRef,
    pub nb_extended_buf: c_int,

    pub side_data: *mut *mut AVFrameSideData,
    pub nb_side_data: c_int,

    pub flags: c_int,

    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
    pub chroma_location: c_int,

    pub best_effort_timestamp: i64,

    /// `FF_API_FRAME_PKT` (in 59 noch aktiv).
    pub pkt_pos: i64,

    pub metadata: *mut AVDictionary,

    pub decode_error_flags: c_int,

    /// `FF_API_FRAME_PKT` (in 59 noch aktiv).
    pub pkt_size: c_int,

    pub hw_frames_ctx: *mut AVBufferRef,

    pub opaque_ref: *mut AVBufferRef,

    pub crop_top: usize,
    pub crop_bottom: usize,
    pub crop_left: usize,
    pub crop_right: usize,

    pub private_ref: *mut AVBufferRef,

    pub ch_layout: AVChannelLayout,

    pub duration: i64,
}

// ---------------------------------------------------------------------------
// AVCodecContext — bis inkl. `hw_device_ctx` transkribiert (View, siehe oben)
// ---------------------------------------------------------------------------

/// C: `typedef struct AVCodecContext` (libavcodec/avcodec.h, avcodec 61 / n7.1).
#[repr(C)]
pub struct AVCodecContext {
    pub av_class: *const AVClass,
    pub log_level_offset: c_int,

    pub codec_type: c_int, // enum AVMediaType
    pub codec: *const AVCodec,
    pub codec_id: c_int, // enum AVCodecID

    pub codec_tag: c_uint,

    pub priv_data: *mut c_void,

    pub internal: *mut AVCodecInternal,

    pub opaque: *mut c_void,

    pub bit_rate: i64,

    pub flags: c_int,

    pub flags2: c_int,

    pub extradata: *mut u8,
    pub extradata_size: c_int,

    pub time_base: AVRational,

    pub pkt_timebase: AVRational,

    pub framerate: AVRational,

    pub ticks_per_frame: c_int,

    pub delay: c_int,

    pub width: c_int,
    pub height: c_int,

    pub coded_width: c_int,
    pub coded_height: c_int,

    pub sample_aspect_ratio: AVRational,

    pub pix_fmt: c_int, // enum AVPixelFormat

    pub sw_pix_fmt: c_int, // enum AVPixelFormat

    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
    pub color_range: c_int,
    pub chroma_sample_location: c_int,

    pub field_order: c_int,

    pub refs: c_int,

    pub has_b_frames: c_int,

    pub slice_flags: c_int,

    pub draw_horiz_band: Option<
        unsafe extern "system" fn(
            s: *mut AVCodecContext,
            src: *const AVFrame,
            offset: *mut c_int,
            y: c_int,
            ty: c_int,
            height: c_int,
        ),
    >,

    pub get_format:
        Option<unsafe extern "system" fn(s: *mut AVCodecContext, fmt: *const c_int) -> c_int>,

    pub max_b_frames: c_int,

    pub b_quant_factor: f32,
    pub b_quant_offset: f32,

    pub i_quant_factor: f32,
    pub i_quant_offset: f32,

    pub lumi_masking: f32,
    pub temporal_cplx_masking: f32,
    pub spatial_cplx_masking: f32,
    pub p_masking: f32,
    pub dark_masking: f32,

    pub nsse_weight: c_int,

    pub me_cmp: c_int,
    pub me_sub_cmp: c_int,
    pub mb_cmp: c_int,
    pub ildct_cmp: c_int,

    pub dia_size: c_int,

    pub last_predictor_count: c_int,

    pub me_pre_cmp: c_int,

    pub pre_dia_size: c_int,

    pub me_subpel_quality: c_int,

    pub me_range: c_int,

    pub mb_decision: c_int,

    pub intra_matrix: *mut u16,
    pub inter_matrix: *mut u16,
    pub chroma_intra_matrix: *mut u16,

    pub intra_dc_precision: c_int,

    pub mb_lmin: c_int,
    pub mb_lmax: c_int,

    pub bidir_refine: c_int,

    pub keyint_min: c_int,

    pub gop_size: c_int,

    pub mv0_threshold: c_int,

    pub slices: c_int,

    pub sample_rate: c_int,

    pub sample_fmt: c_int, // enum AVSampleFormat

    pub ch_layout: AVChannelLayout,

    pub frame_size: c_int,

    pub block_align: c_int,

    pub cutoff: c_int,

    pub audio_service_type: c_int, // enum AVAudioServiceType

    pub request_sample_fmt: c_int, // enum AVSampleFormat

    pub initial_padding: c_int,
    pub trailing_padding: c_int,

    pub seek_preroll: c_int,

    pub get_buffer2: Option<
        unsafe extern "system" fn(
            s: *mut AVCodecContext,
            frame: *mut AVFrame,
            flags: c_int,
        ) -> c_int,
    >,

    pub bit_rate_tolerance: c_int,

    pub global_quality: c_int,

    pub compression_level: c_int,

    pub qcompress: f32,
    pub qblur: f32,

    pub qmin: c_int,
    pub qmax: c_int,

    pub max_qdiff: c_int,

    pub rc_buffer_size: c_int,
    pub rc_override_count: c_int,
    pub rc_override: *mut RcOverride,

    pub rc_max_rate: i64,
    pub rc_min_rate: i64,

    pub rc_max_available_vbv_use: f32,
    pub rc_min_vbv_overflow_use: f32,

    pub rc_initial_buffer_occupancy: c_int,

    pub trellis: c_int,

    pub stats_out: *mut c_char,
    pub stats_in: *mut c_char,

    pub workaround_bugs: c_int,

    pub strict_std_compliance: c_int,

    pub error_concealment: c_int,

    pub debug: c_int,

    pub err_recognition: c_int,

    pub hwaccel: *const AVCodecHWAccel,

    pub hwaccel_context: *mut c_void,

    pub hw_frames_ctx: *mut AVBufferRef,

    pub hw_device_ctx: *mut AVBufferRef,
}

// ---------------------------------------------------------------------------
// AVPacket — vollständig (packet.h, avcodec 61)
// ---------------------------------------------------------------------------

/// C: `typedef struct AVPacket` (libavcodec/packet.h).
#[repr(C)]
pub struct AVPacket {
    pub buf: *mut AVBufferRef,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    pub stream_index: c_int,
    pub flags: c_int,
    pub side_data: *mut AVPacketSideData,
    pub side_data_elems: c_int,
    pub duration: i64,
    pub pos: i64,
    pub opaque: *mut c_void,
    pub opaque_ref: *mut AVBufferRef,
    /// Seit avcodec 59.8.100 (C setzt es in chiaki_ffmpeg_decoder_video_sample_cb).
    pub time_base: AVRational,
}

// ---------------------------------------------------------------------------
// Funktionszeiger-Typen (via libloading aufgelöst)
// ---------------------------------------------------------------------------

/// va_list ist auf Win-x64 (MS-ABI, auch MinGW) ein `char*` — hier opak als
/// `*mut c_void` durchgereicht, nur an `av_log_format_line` weitergegeben.
pub type VaList = *mut c_void;

pub type FnAvcodecVersion = unsafe extern "system" fn() -> c_uint;
pub type FnAvcodecFindDecoder = unsafe extern "system" fn(id: c_int) -> *const AVCodec;
pub type FnAvcodecAllocContext3 =
    unsafe extern "system" fn(codec: *const AVCodec) -> *mut AVCodecContext;
pub type FnAvcodecFreeContext = unsafe extern "system" fn(ctx: *mut *mut AVCodecContext);
pub type FnAvcodecOpen2 = unsafe extern "system" fn(
    ctx: *mut AVCodecContext,
    codec: *const AVCodec,
    options: *mut *mut AVDictionary,
) -> c_int;
pub type FnAvcodecSendPacket =
    unsafe extern "system" fn(ctx: *mut AVCodecContext, packet: *const AVPacket) -> c_int;
pub type FnAvcodecReceiveFrame =
    unsafe extern "system" fn(ctx: *mut AVCodecContext, frame: *mut AVFrame) -> c_int;
pub type FnAvcodecGetHwConfig =
    unsafe extern "system" fn(codec: *const AVCodec, index: c_int) -> *const AVCodecHWConfig;

pub type FnAvutilVersion = unsafe extern "system" fn() -> c_uint;
pub type FnAvLogSetCallback = unsafe extern "system" fn(
    cb: Option<unsafe extern "system" fn(*mut c_void, c_int, *const c_char, VaList)>,
);
pub type FnAvLogSetLevel = unsafe extern "system" fn(level: c_int);
pub type FnAvLogFormatLine = unsafe extern "system" fn(
    avcl: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    vl: VaList,
    line: *mut c_char,
    line_size: c_int,
    print_prefix: *mut c_int,
);
pub type FnAvHwdeviceFindTypeByName = unsafe extern "system" fn(name: *const c_char) -> c_int;
pub type FnAvHwdeviceCtxCreate = unsafe extern "system" fn(
    device_ctx: *mut *mut AVBufferRef,
    ty: c_int,
    device: *const c_char,
    options: *mut *mut AVDictionary,
    flags: c_int,
) -> c_int;
pub type FnAvBufferRefFn = unsafe extern "system" fn(buf: *mut AVBufferRef) -> *mut AVBufferRef;
pub type FnAvBufferUnref = unsafe extern "system" fn(buf: *mut *mut AVBufferRef);
/// Signatur wie in vsrupscaler.cpp genutzt (NV12-Contiguity-Buffer).
pub type FnAvBufferCreate = unsafe extern "system" fn(
    data: *mut u8,
    size: usize,
    free: Option<unsafe extern "system" fn(opaque: *mut c_void, data: *mut u8)>,
    opaque: *mut c_void,
    flags: c_int,
) -> *mut AVBufferRef;
pub type FnAvMalloc = unsafe extern "system" fn(size: usize) -> *mut c_void;
pub type FnAvFree = unsafe extern "system" fn(ptr: *mut c_void);

pub type FnAvPacketAlloc = unsafe extern "system" fn() -> *mut AVPacket;
pub type FnAvPacketFree = unsafe extern "system" fn(packet: *mut *mut AVPacket);

pub type FnAvFrameAlloc = unsafe extern "system" fn() -> *mut AVFrame;
pub type FnAvFrameFree = unsafe extern "system" fn(frame: *mut *mut AVFrame);
pub type FnAvFrameUnref = unsafe extern "system" fn(frame: *mut AVFrame);
pub type FnAvFrameGetBuffer = unsafe extern "system" fn(frame: *mut AVFrame, align: c_int) -> c_int;
pub type FnAvFrameCopyProps =
    unsafe extern "system" fn(dst: *mut AVFrame, src: *const AVFrame) -> c_int;
pub type FnAvHwframeTransferData =
    unsafe extern "system" fn(dst: *mut AVFrame, src: *const AVFrame, flags: c_int) -> c_int;
pub type FnAvGetPixFmtName = unsafe extern "system" fn(pix_fmt: c_int) -> *const c_char;
pub type FnAvStrerror =
    unsafe extern "system" fn(errnum: c_int, errbuf: *mut c_char, errbuf_size: usize) -> c_int;

pub type FnSwsVersion = unsafe extern "system" fn() -> c_uint;
pub type FnSwsGetContext = unsafe extern "system" fn(
    src_w: c_int,
    src_h: c_int,
    src_format: c_int,
    dst_w: c_int,
    dst_h: c_int,
    dst_format: c_int,
    flags: c_int,
    src_filter: *mut SwsFilter,
    dst_filter: *mut SwsFilter,
    param: *const f64,
) -> *mut SwsContext;
#[allow(clippy::too_many_arguments)]
pub type FnSwsScale = unsafe extern "system" fn(
    ctx: *mut SwsContext,
    src_slice: *const *const u8,
    src_stride: *const c_int,
    src_slice_y: c_int,
    src_slice_h: c_int,
    dst: *const *mut u8,
    dst_stride: *const c_int,
) -> c_int;
pub type FnSwsFreeContext = unsafe extern "system" fn(ctx: *mut SwsContext);

/// Alle aufgelösten FFmpeg-Funktionen (avcodec + avutil + swscale).
///
/// Mit `libloading` aufgelöst; die Structs hier sind `unsafe extern "system" fn`
/// (Win-x64 hat nur eine Calling Convention — `extern "system"` == cdecl).
pub struct Api {
    // avcodec
    pub avcodec_version: FnAvcodecVersion,
    pub avcodec_find_decoder: FnAvcodecFindDecoder,
    pub avcodec_alloc_context3: FnAvcodecAllocContext3,
    pub avcodec_free_context: FnAvcodecFreeContext,
    pub avcodec_open2: FnAvcodecOpen2,
    pub avcodec_send_packet: FnAvcodecSendPacket,
    pub avcodec_receive_frame: FnAvcodecReceiveFrame,
    pub avcodec_get_hw_config: FnAvcodecGetHwConfig,
    // avutil
    pub avutil_version: FnAvutilVersion,
    pub av_log_set_callback: FnAvLogSetCallback,
    pub av_log_set_level: FnAvLogSetLevel,
    pub av_log_format_line: FnAvLogFormatLine,
    pub av_hwdevice_find_type_by_name: FnAvHwdeviceFindTypeByName,
    pub av_hwdevice_ctx_create: FnAvHwdeviceCtxCreate,
    pub av_buffer_ref: FnAvBufferRefFn,
    pub av_buffer_unref: FnAvBufferUnref,
    pub av_buffer_create: FnAvBufferCreate,
    pub av_malloc: FnAvMalloc,
    pub av_free: FnAvFree,
    pub av_packet_alloc: FnAvPacketAlloc,
    pub av_packet_free: FnAvPacketFree,
    pub av_frame_alloc: FnAvFrameAlloc,
    pub av_frame_free: FnAvFrameFree,
    pub av_frame_unref: FnAvFrameUnref,
    pub av_frame_get_buffer: FnAvFrameGetBuffer,
    pub av_frame_copy_props: FnAvFrameCopyProps,
    pub av_hwframe_transfer_data: FnAvHwframeTransferData,
    pub av_get_pix_fmt_name: FnAvGetPixFmtName,
    pub av_strerror: FnAvStrerror,
    // swscale
    pub swscale_version: FnSwsVersion,
    pub sws_get_context: FnSwsGetContext,
    pub sws_scale: FnSwsScale,
    pub sws_free_context: FnSwsFreeContext,
}
