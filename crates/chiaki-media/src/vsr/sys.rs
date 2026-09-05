// SPDX-License-Identifier: AGPL-3.0-only
//! Schmale, handgeschriebene FFI-Typen für das NVIDIA Video Effects SDK
//! ("VFX SDK" 1.2.0.0, Feature `nvvfxvideosuperres`).
//!
//! Struktur-Feld für Feld transkribiert aus den Referenz-Headern:
//! `vfx_sdk/sdk/VideoFX/nvvfx/include/nvCVImage.h` (`struct NvCVImage`),
//! `nvVideoEffects.h` (`NvVFX_*`), `nvCVStatus.h` (`NvCV_Status`) und
//! `features/nvvfxvideosuperres/include/nvVFXVideoSuperRes.h`
//! (`NVVFX_FX_VIDEO_SUPER_RES`, `NVVFX_QUALITY_LEVEL`).
//!
//! Alle Funktionen werden zur Laufzeit über `libloading` aufgelöst — es gibt
//! keine Importbibliotheken und keine SDK-Header zur Build-Zeit (genau wie im
//! C++-Original `gui/src/vsrupscaler.cpp`). Ruft man dieses Modul ohne die
//! DLLs auf, werden sauber Fehler geliefert statt zu crashen.
//!
//! WICSLIG (REWORK.md, Root Causes 2+3): Die DLLs sind kontextgebunden.
//! - `NvCVImage_Transfer` juggelt CUDA-Kontexte selbst und läuft damit
//!   NVDEC-/SDK-/unsere Buffer hinweg — rohe Driver-API tut das nicht.
//! - `NvCVImage_Alloc` (GPU) und `NvVFX_Load` (TensorRT-Engine-Build im
//!   *current context*!) brauchen einen gepushten Decoder-Kontext, sonst
//!   scheitern sie (CUDA 201 → SDK-Fallback -1999 = `NVCV_ERR_CUDA`).

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

/// C: `typedef int NvCV_Status` (nvCVStatus.h — Enum mit negativen Werten).
pub type NvCVStatus = c_int;

/// C: `typedef struct NvVFX_Object* NvVFX_Handle` (nvVideoEffects.h).
pub type NvVFX_Handle = *mut c_void;
/// C: `typedef const char* NvVFX_ParameterSelector` / `NvVFX_EffectSelector`.
pub type NvVFX_ParameterSelector = *const c_char;

// ---------------------------------------------------------------------------
// Konstanten (nvCVImage.h / nvCVStatus.h / nvVFXVideoSuperRes.h)
// ---------------------------------------------------------------------------

// NvCVImage_PixelFormat (RTX_CAMERA_IMAGE=0-Zweig, wie im SDK-Header voreingestellt).
pub const NVCV_FORMAT_UNKNOWN: c_int = 0;
/// Luminance (gray) — wird für die Einzel-Plane-Staging-Views benutzt.
pub const NVCV_Y: c_int = 1;
pub const NVCV_A: c_int = 2;
pub const NVCV_YA: c_int = 3;
pub const NVCV_RGB: c_int = 4;
pub const NVCV_BGR: c_int = 5;
/// Interleaved RGBA — Input-/Output-Format des VSR-Netzes.
pub const NVCV_RGBA: c_int = 6;
pub const NVCV_BGRA: c_int = 7;
pub const NVCV_ARGB: c_int = 8;
pub const NVCV_ABGR: c_int = 9;
/// YUV 4:2:0 — PixelFormat für NV12-Bilder.
pub const NVCV_YUV420: c_int = 10;
pub const NVCV_YUV422: c_int = 11;
pub const NVCV_YUV444: c_int = 12;

// NvCVImage_ComponentType.
pub const NVCV_TYPE_UNKNOWN: c_int = 0;
/// Unsigned 8-bit — der einzige im VSR-Pfad genutzte Komponententyp.
pub const NVCV_U8: c_int = 1;

// Layout-Werte (planar-Feld / layout-Argument).
/// Interleaved/chunky — für RGBA und die Y-Einzelplanen.
pub const NVCV_INTERLEAVED: c_uint = 0;
pub const NVCV_CHUNKY: c_uint = 0;
pub const NVCV_PLANAR: c_uint = 1;
/// `[Y][UV]` Semi-planar 4:2:0 — aka `NVCV_NV12` (FOURCC-Alias im Header).
pub const NVCV_YCUV: c_uint = 7;
/// Alias: `#define NVCV_NV12 NVCV_YCUV`.
pub const NVCV_NV12: c_uint = NVCV_YCUV;

// memSpace (gpuMem-Feld / memSpace-Argument).
/// CPU-Speicher.
pub const NVCV_CPU: c_uint = 0;
/// CUDA/GPU-Speicher (`#define NVCV_CUDA NVCV_GPU`).
pub const NVCV_GPU: c_uint = 1;
pub const NVCV_CUDA: c_uint = 1;
pub const NVCV_CPU_PINNED: c_uint = 2;
pub const NVCV_CUDA_ARRAY: c_uint = 3;

// NvCV_Status-Codes (nvCVStatus.h) — die im VSR-Pfad vorkommenden.
pub const NVCV_SUCCESS: NvCVStatus = 0;
pub const NVCV_ERR_GENERAL: NvCVStatus = -1;
pub const NVCV_ERR_UNIMPLEMENTED: NvCVStatus = -2;
pub const NVCV_ERR_MEMORY: NvCVStatus = -3;
pub const NVCV_ERR_EFFECT: NvCVStatus = -4;
pub const NVCV_ERR_SELECTOR: NvCVStatus = -5;
pub const NVCV_ERR_BUFFER: NvCVStatus = -6;
pub const NVCV_ERR_PARAMETER: NvCVStatus = -7;
pub const NVCV_ERR_MISMATCH: NvCVStatus = -8;
pub const NVCV_ERR_PIXELFORMAT: NvCVStatus = -9;
pub const NVCV_ERR_MODEL: NvCVStatus = -10;
pub const NVCV_ERR_LIBRARY: NvCVStatus = -11;
pub const NVCV_ERR_INITIALIZATION: NvCVStatus = -12;
pub const NVCV_ERR_FEATURENOTFOUND: NvCVStatus = -14;
pub const NVCV_ERR_RESOLUTION: NvCVStatus = -16;
pub const NVCV_ERR_UNSUPPORTEDGPU: NvCVStatus = -17;
pub const NVCV_ERR_CUDA_BASE: NvCVStatus = -1000;
pub const NVCV_ERR_CUDA_MEMORY: NvCVStatus = -1002;
pub const NVCV_ERR_CUDA_INIT: NvCVStatus = -1003;
pub const NVCV_ERR_CUDA: NvCVStatus = -1999;

// Effekt- und Parameter-Selektoren (nvVFXVideoSuperRes.h / nvVideoEffects.h).
/// `NVVFX_FX_VIDEO_SUPER_RES` — Effektname für `NvVFX_CreateEffect`.
pub const NVVFX_FX_VIDEO_SUPER_RES: &str = "VideoSuperRes";
/// `NVVFX_QUALITY_LEVEL` (Feature-Header).
pub const NVVFX_QUALITY_LEVEL: &str = "QualityLevel";
/// `NVVFX_INPUT_IMAGE_0` / `NVVFX_INPUT_IMAGE`.
pub const NVVFX_INPUT_IMAGE_0: &str = "SrcImage0";
/// `NVVFX_OUTPUT_IMAGE_0` / `NVVFX_OUTPUT_IMAGE`.
pub const NVVFX_OUTPUT_IMAGE_0: &str = "DstImage0";
/// `NVVFX_MODEL_DIRECTORY`.
pub const NVVFX_MODEL_DIRECTORY: &str = "ModelDir";
/// `NVVFX_CUDA_STREAM`.
pub const NVVFX_CUDA_STREAM: &str = "CudaStream";
/// `NVVFX_SCALE`.
pub const NVVFX_SCALE: &str = "Scale";
/// `NVVFX_STRENGTH`.
pub const NVVFX_STRENGTH: &str = "Strength";
/// `NVVFX_MODE`.
pub const NVVFX_MODE: &str = "Mode";
/// `NVVFX_GPU`.
pub const NVVFX_GPU: &str = "GPU";
/// `NVVFX_STATE`.
pub const NVVFX_STATE: &str = "State";
/// `NVVFX_STATE_SIZE`.
pub const NVVFX_STATE_SIZE: &str = "StateSize";
/// `NVVFX_STATE_COUNT`.
pub const NVVFX_STATE_COUNT: &str = "NumStateObjects";
/// `NVVFX_INFO`.
pub const NVVFX_INFO: &str = "Info";

// ---------------------------------------------------------------------------
// NvCVImage (nvCVImage.h, SDK 1.2) — exakte C-Repräsentation
// ---------------------------------------------------------------------------

/// C: `struct NvCVImage` — Feld für Feld aus dem Header, 64 Bytes auf Win-x64.
///
/// Das C++-Original (`vsrupscaler.cpp`) repliziert genau dieses Layout; das
/// SDK hängt seine Bilder intern daran auf (u. a. `pitch` = Byte-Stride,
/// `pixels` = Plane-0-Pointer, NV12 wird als kontiguierlich
/// `Y@pixels[0 .. pitch*h) + UV@[pitch*h .. pitch*h*1.5)` adressiert).
#[repr(C)]
pub struct NvCVImage {
    pub width: c_uint,
    pub height: c_uint,
    /// Byte-Stride senkrecht (kann negativ sein — daher signed).
    pub pitch: c_int,
    pub pixelFormat: c_int,     // NvCVImage_PixelFormat
    pub componentType: c_int,   // NvCVImage_ComponentType
    pub pixelBytes: u8,
    pub componentBytes: u8,
    pub numComponents: u8,
    pub planar: u8,             // NVCV_CHUNKY / NVCV_PLANAR / NVCV_YCUV ...
    pub gpuMem: u8,             // NVCV_CPU / NVCV_CPU_PINNED / NVCV_GPU
    pub colorspace: u8,         // OR aus NVCV_601/709/2020, RANGE, CHROMA_*
    pub reserved: [u8; 2],
    pub pixels: *mut c_void,
    pub deletePtr: *mut c_void,
    pub deleteProc: Option<unsafe extern "system" fn(p: *mut c_void)>,
    pub bufferBytes: u64,
}

impl NvCVImage {
    /// C++-Default-Konstruktor: mit 0 füllen.
    pub const fn zeroed() -> Self {
        NvCVImage {
            width: 0,
            height: 0,
            pitch: 0,
            pixelFormat: 0,
            componentType: 0,
            pixelBytes: 0,
            componentBytes: 0,
            numComponents: 0,
            planar: 0,
            gpuMem: 0,
            colorspace: 0,
            reserved: [0; 2],
            pixels: std::ptr::null_mut(),
            deletePtr: std::ptr::null_mut(),
            deleteProc: None,
            bufferBytes: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Funktionszeiger-Typen (via libloading aufgelöst; Win-x64: __cdecl ==
// extern "system" — es gibt nur eine Calling Convention)
// ---------------------------------------------------------------------------

/// `NvCVImage_Init` — View auf bestehenden Buffer (keine Allokation).
pub type FnNvCVImageInit = unsafe extern "system" fn(
    im: *mut NvCVImage,
    width: c_uint,
    height: c_uint,
    pitch: c_int,
    pixels: *mut c_void,
    format: c_int,
    ty: c_int,
    layout: c_uint,
    memSpace: c_uint,
) -> NvCVStatus;

/// `NvCVImage_Alloc` — alloziert (CPU via SDK / GPU im **current context**!).
#[allow(clippy::too_many_arguments)]
pub type FnNvCVImageAlloc = unsafe extern "system" fn(
    im: *mut NvCVImage,
    width: c_uint,
    height: c_uint,
    format: c_int,
    ty: c_int,
    layout: c_uint,
    memSpace: c_uint,
    alignment: c_uint,
) -> NvCVStatus;

/// `NvCVImage_Dealloc` — gibt den von `Alloc` belegten Buffer frei.
pub type FnNvCVImageDealloc = unsafe extern "system" fn(im: *mut NvCVImage);

/// `NvCVImage_Transfer` — Kopie/Konvertierung CPU↔CPU, CPU↔GPU, GPU↔GPU;
/// juggelt CUDA-Kontexte selbst (REWORK.md Root Cause 2). `tmp` darf leer
/// (zeroed) oder NULL sein — bei CPU↔GPU-Transfers wächst der SDK-seitig.
pub type FnNvCVImageTransfer = unsafe extern "system" fn(
    src: *const NvCVImage,
    dst: *mut NvCVImage,
    scale: f32,
    stream: *mut c_void,
    tmp: *mut NvCVImage,
) -> NvCVStatus;

pub type FnNvVFXCreateEffect =
    unsafe extern "system" fn(code: NvVFX_ParameterSelector, effect: *mut NvVFX_Handle) -> NvCVStatus;
pub type FnNvVFXDestroyEffect = unsafe extern "system" fn(effect: NvVFX_Handle);
pub type FnNvVFXSetImage = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    im: *mut NvCVImage,
) -> NvCVStatus;
pub type FnNvVFXSetObject = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    ptr: *mut c_void,
) -> NvCVStatus;
pub type FnNvVFXSetU32 = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    val: c_uint,
) -> NvCVStatus;
pub type FnNvVFXSetF32 = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    val: f32,
) -> NvCVStatus;
pub type FnNvVFXSetString = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    str: *const c_char,
) -> NvCVStatus;
pub type FnNvVFXGetU32 = unsafe extern "system" fn(
    effect: NvVFX_Handle,
    paramName: NvVFX_ParameterSelector,
    val: *mut c_uint,
) -> NvCVStatus;
pub type FnNvVFXLoad = unsafe extern "system" fn(effect: NvVFX_Handle) -> NvCVStatus;
/// `async` != 0 → asynchron (wir nutzen — wie das C++ — immer 0 = synchron).
pub type FnNvVFXRun =
    unsafe extern "system" fn(effect: NvVFX_Handle, async_: c_int) -> NvCVStatus;

// CUDA Driver API (nvcuda.dll) — NUR für pushCtx/popCtx/Sync; jede andere
// rohe Driver-API ist kontextgebunden und wird bewusst NICHT benutzt
// (REWORK.md Root Cause 2: cuMemAlloc/cuMemcpy2DAsync → rc=201).
pub type FnCuCtxPushCurrent = unsafe extern "system" fn(ctx: *mut c_void) -> c_int;
pub type FnCuCtxPopCurrent = unsafe extern "system" fn(ctx: *mut *mut c_void) -> c_int;
pub type FnCuStreamSynchronize = unsafe extern "system" fn(stream: *mut c_void) -> c_int;
/// Synchronisiert ALLE Streams des aktuellen Kontexts — nötig, weil
/// `NvVFX_Run` (SDK 1.2: ohne implementiertes "CudaStream"-SetObject) auf
/// einem SDK-internen Stream läuft, mit dem unsere NULL-Stream-Transfers
/// sonst NICHT geordnet sind.
pub type FnCuCtxSynchronize = unsafe extern "system" fn() -> c_int;

/// Alle aufgelösten VFX-SDK-Funktionen.
///
/// `Copy`, damit Aufrufer eine eigene Kopie ziehen können, ohne `self` zu
/// leihen (die fn-Pointer sind Copy) — z. B. um parallel eigene Felder mutieren
/// zu können.
#[derive(Clone, Copy)]
pub struct VfxApi {
    // NVCVImage.dll
    pub image_init: FnNvCVImageInit,
    pub image_alloc: FnNvCVImageAlloc,
    pub image_dealloc: FnNvCVImageDealloc,
    pub image_transfer: FnNvCVImageTransfer,
    // NVVideoEffects.dll (inkl. NvVFX_* — nvVFXVideoSuperRes.dll ist nur die
    // Feature-Implementierung, die via Preload in den Adressraum kommt)
    pub vfx_create_effect: FnNvVFXCreateEffect,
    pub vfx_destroy_effect: FnNvVFXDestroyEffect,
    pub vfx_set_image: FnNvVFXSetImage,
    pub vfx_set_object: FnNvVFXSetObject,
    pub vfx_set_u32: FnNvVFXSetU32,
    pub vfx_set_f32: FnNvVFXSetF32,
    pub vfx_set_string: FnNvVFXSetString,
    pub vfx_get_u32: FnNvVFXGetU32,
    pub vfx_load: FnNvVFXLoad,
    pub vfx_run: FnNvVFXRun,
    // nvcuda.dll
    pub cu_ctx_push_current: FnCuCtxPushCurrent,
    pub cu_ctx_pop_current: FnCuCtxPopCurrent,
    pub cu_stream_synchronize: FnCuStreamSynchronize,
    pub cu_ctx_synchronize: FnCuCtxSynchronize,
}

/// Sprechbarer Name zu einem `NvCV_Status` (für Logs/`last_error`) — Mapping
/// gemäß nvCVStatus.h; CUDA-Fehler werden dem Bereich zugeordnet, -1999 ist
/// der SDK-Fallback für ungemappte CUDA-Fehler (z. B. 201 INVALID_CONTEXT).
pub fn status_name(code: NvCVStatus) -> &'static str {
    match code {
        NVCV_SUCCESS => "NVCV_SUCCESS",
        NVCV_ERR_GENERAL => "NVCV_ERR_GENERAL",
        NVCV_ERR_UNIMPLEMENTED => "NVCV_ERR_UNIMPLEMENTED",
        NVCV_ERR_MEMORY => "NVCV_ERR_MEMORY",
        NVCV_ERR_EFFECT => "NVCV_ERR_EFFECT",
        NVCV_ERR_SELECTOR => "NVCV_ERR_SELECTOR",
        NVCV_ERR_BUFFER => "NVCV_ERR_BUFFER",
        NVCV_ERR_PARAMETER => "NVCV_ERR_PARAMETER",
        NVCV_ERR_MISMATCH => "NVCV_ERR_MISMATCH",
        NVCV_ERR_PIXELFORMAT => "NVCV_ERR_PIXELFORMAT",
        NVCV_ERR_MODEL => "NVCV_ERR_MODEL",
        NVCV_ERR_LIBRARY => "NVCV_ERR_LIBRARY",
        NVCV_ERR_INITIALIZATION => "NVCV_ERR_INITIALIZATION",
        NVCV_ERR_FEATURENOTFOUND => "NVCV_ERR_FEATURENOTFOUND",
        NVCV_ERR_RESOLUTION => "NVCV_ERR_RESOLUTION",
        NVCV_ERR_UNSUPPORTEDGPU => "NVCV_ERR_UNSUPPORTEDGPU",
        NVCV_ERR_CUDA => "NVCV_ERR_CUDA (-1999: ungemappter CUDA-Fehler, z. B. INVALID_CONTEXT=201)",
        c if (-210..=-200).contains(&c) => "NVCV_ERR_GL/D3D_*",
        c if (-199..=-100).contains(&c) => "NVCV_ERR_TRITON_*",
        c if c <= -1000 => "NVCV_ERR_CUDA_*",
        _ => "NVCV_ERR_<unbekannt>",
    }
}

/// Compile-Zeit-Kontrolle des `NvCVImage`-Layouts: 5×4 Bytes Skalare + 8 Bytes
/// u8-Felder (gepadigt auf 8) + 3 Pointer + u64 = 64 Bytes auf Win-x64.
const _: () = assert!(std::mem::size_of::<NvCVImage>() == 64);

/// NVCV_NV12 muss der YCUV-Wert (7) sein — FOURCC-Alias im Header.
const _: () = assert!(NVCV_NV12 == 7);
/// NVCV_CUDA == NVCV_GPU == 1 (wie im #define).
const _: () = assert!(NVCV_CUDA == 1 && NVCV_GPU == 1);
/// NVCV_YUV420 liegt im RTX_CAMERA_IMAGE=0-Zweig bei 10 (C++ nutzt denselben).
const _: () = assert!(NVCV_YUV420 == 10);
