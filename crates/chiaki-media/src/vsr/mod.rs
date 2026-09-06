// SPDX-License-Identifier: AGPL-3.0-only
//! Port von `gui/src/vsrupscaler.cpp/h` (chiaki-ng Remaster) — NVIDIA Video
//! Effects ("NvVFX") VideoSuperRes-Upscaler, dynamisch geladen zur Laufzeit.
//!
//! Nutzt das VFX-SDK-Feature `nvvfxvideosuperres` (`nvVFXVideoSuperRes.dll` /
//! `nvngx_vsr.dll`) zusammen mit dem SDK-Kern (`NVVideoEffects.dll`,
//! `NVCVImage.dll`) und der CUDA-Driver-API (`nvcuda.dll`, ausschließlich für
//! Kontext-Push/Pop + Stream-Sync). Windows-only; ohne SDK deaktiviert sich
//! VSR sauber (kein Crash) — wie im C++.
//!
//! ## Die drei Root-Causes aus REWORK.md (harte Anforderungen, alle eingehalten)
//! 1. **NVDEC aligned height** (1080→1088): Die UV-Ebene liegt real bei
//!    `planes[1].data`, NICHT bei `Y + pitch*height`. Deshalb wird pro Frame
//!    **pro-Plane** aus den echten `DecodedFrame.planes`-Pointern gestagt
//!    (Y- und UV-Ebene als einzelne `NVCV_Y`-Views), niemals eine NV12-View
//!    über `Y + pitch*height` konstruiert. 720p war im C++ nur zufällig
//!    immun (720 ist schon 16-aligned).
//! 2. **Raw CUDA ist kontextgebunden** (cuMemAlloc/cuMemcpy2DAsync → rc=201
//!    INVALID_CONTEXT): Es wird KEINE rohe Driver-API für Speicher/Kopien
//!    benutzt. Alle Allokationen laufen über `NvCVImage_Alloc` (SDK, im
//!    *current context*) und alle Kopien über `NvCVImage_Transfer` (SDK
//!    juggelt Kontexte selbst). Von `nvcuda.dll` werden nur
//!    `cuCtxPushCurrent`/`cuCtxPopCurrent`/`cuStreamSynchronize` aufgelöst.
//! 3. **`NvVFX_Load` braucht einen aktuellen CUDA-Kontext** (-1999 =
//!    `NVCV_ERR_CUDA`): Der KOMPLETTE Setup-Block (GPU-Allokationen →
//!    `NvVFX_CreateEffect` → `SetImage/SetObject/SetU32` → `Load`) läuft in
//!    EINEM `pushCtx(Decoder-Kontext)…popCtx`; `NvVFX_Run` ebenfalls. Ohne
//!    Kontext: CUDA 201 → SDK-Fallback -1999.
//!
//! ## Abweichungen zum C++ (dokumentiert)
//! - Der C++-Client erhält CUDA-Frames (NVDEC-Device-Pointer) und stagt mit
//!   `NVCV_GPU`-Source-Views. chiaki-medias `Decoder` überträgt HW-Frames
//!   (`av_hwframe_transfer_data`) in **Systemspeicher** — die
//!   `DecodedFrame.planes` sind CPU-Pointer. Die Staging-Source-Views werden
//!   daher mit `NVCV_CPU` initialisiert; `NvCVImage_Transfer` erledigt den
//!   CPU→GPU-Transfer (dafür existiert der SDK-seitig wachsende
//!   `transferTmp`-Buffer — genau der Fix für den beobachteten
//!   `NVCV_ERR_GENERAL` ohne Tmp-Buffer). Der Root-Cause-1-Fix bleibt identisch:
//!   echte `planes[1]`-Pointer statt `Y + pitch*height`.
//! - Der C++-Output ist ein AVFrame mit `av_buffer_create`-Contiguity-Buffer +
//!   `av_frame_copy_props`. Diese Crate übergibt Frames als `DecodedFrame`/
//!   [`FrameBuf`] (keine AVFrames in der öffentlichen API) — `FrameBuf` hält
//!   denselben exakt SDK-kompatiblen kontiguierlichen NV12-Buffer
//!   (`Y@0 + UV@pitch*h`, Fix für grüner Streifen + Heap-Overwrite) und
//!   übernimmt die Metadaten (pts/duration/frames_lost/recovered) manuell.
//! - Im C++ wird der CUDA-Kontext pro Frame aus `AVFrame.hw_frames_ctx →
//!   AVHWFramesContext.device_ref → AVHWDeviceContext.hwctx` gelesen. Der
//!   Rust-Decoder hält dieselbe `AVHWDeviceContext` (`hw_device_ctx`) und
//!   exponiert `cuda_ctx`/`cuda_stream` direkt — `init()` bekommt sie als
//!   Parameter ([`crate::decoder::Decoder::cuda_context`]).

pub mod sys;

use std::ffi::CString;
use std::os::raw::{c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use libloading::Library;

use crate::decoder::{DecodedFrame, Plane};
use sys::{
    NvCVImage, VfxApi, NVCV_CPU, NVCV_ERR_GENERAL, NVCV_GPU, NVCV_INTERLEAVED,
    NVCV_NV12, NVCV_RGBA, NVCV_SUCCESS, NVCV_U8, NVCV_YUV420, NVCV_Y, NVVFX_CUDA_STREAM,
    NVVFX_FX_VIDEO_SUPER_RES, NVVFX_INPUT_IMAGE_0, NVVFX_OUTPUT_IMAGE_0, NVVFX_QUALITY_LEVEL,
};

/// Erwartete SDK-DLLs (VFX SDK "VideoFX/bin").
const DLL_NVCV_IMAGE: &str = "NVCVImage.dll";
const DLL_NV_VIDEO_EFFECTS: &str = "NVVideoEffects.dll";
const DLL_NVCUDA: &str = "nvcuda.dll";

/// Feature-/Runtime-DLLs, die NUR vorgeladen werden (wie C++: LoadLibrary
/// ohne FreeLibrary — die Handles werden absichtlich geleakt), damit
/// "VideoSuperRes" erstellt werden kann und seine Abhängigkeiten
/// (cudart/cublas/npp/nvinfer/ngx runtime) auflösen kann.
const PRELOAD_SDK_RUNTIME: &str = "nvngxruntime.dll";
const PRELOAD_FEATURE_NGONX: &str = "nvngx_vsr.dll";
const PRELOAD_FEATURE_VSR: &str = "nvVFXVideoSuperRes.dll";

/// Nach so vielen fehlgeschlagenen Frames schaltet sich VSR selbst ab
/// (C: `failCount > 30`).
const MAX_CONSECUTIVE_FAILURES: i32 = 30;

const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

/// C: `outW = (width * scalePct / 100) & ~1` — Output muss gerade sein.
/// Pure Funktion, damit die Formel ohne GPU testbar ist.
pub fn output_dims(width: u32, height: u32, scale_pct: u32) -> (u32, u32) {
    let scale_pct = scale_pct.max(1);
    (
        (width.wrapping_mul(scale_pct) / 100) & !1,
        (height.wrapping_mul(scale_pct) / 100) & !1,
    )
}

/// C: `quality = (scalePct >= 300) ? 3 : 2` — Qualität/Methode des VSR-Netzes:
/// 0 bicubic, 1 low, 2 medium, 3 high, 4 ultra (C++-Kommentar zu QualityLevel).
pub fn quality_for_scale(scale_pct: u32) -> u32 {
    if scale_pct >= 300 {
        3
    } else {
        2
    }
}

/// C: `const int pitch = (outW + 63) & ~63;` — Output-Pitch des kontiguier-
/// lichen CPU-NV12-Buffers (64-Byte-Ausrichtung, exakt SDK-adressierbar).
pub fn nv12_output_pitch(out_w: u32) -> usize {
    ((out_w as usize) + 63) & !63
}

/// CPU-Output-Frame: **kontiguierlicher** NV12-Buffer mit exakt
/// SDK-kompatiblem Layout (`Y@0 .. pitch*h`, `UV@pitch*h .. pitch*h*1.5`).
///
/// Das ist der Port des "grüner-Streifen/Heap-Overwrite"-Fixes: FFmpegs
/// eigene CPU-Allokatoren nutzen getrennte Plane-Buffer mit Padding-Lücken,
/// während `NvCVImage` NV12 kontiguierlich adressiert — deren Speicher dem
/// SDK zu geben überschreibt den Heap und lässt die unteren Chroma-Zeilen
/// uninitialisiert (sichtbar als grüner Streifen).
///
/// Lifetime-Vertrag wie [`DecodedFrame`]: die über [`FrameBuf::planes`]
/// gelieferten Plane-Pointer bleiben gültig bis zum nächsten
/// [`VsrUpscaler::process_frame`] auf denselben `FrameBuf` oder bis zum Drop.
#[derive(Debug, Clone)]
pub struct FrameBuf {
    buf: Vec<u8>,
    width: u32,
    height: u32,
    pitch: usize,
    pts: f64,
    duration: f64,
    frames_lost: i32,
    recovered: bool,
}

impl Default for FrameBuf {
    fn default() -> Self {
        FrameBuf::new()
    }
}

impl FrameBuf {
    pub fn new() -> Self {
        FrameBuf {
            buf: Vec::new(),
            width: 0,
            height: 0,
            pitch: 0,
            pts: 0.0,
            duration: 0.0,
            frames_lost: 0,
            recovered: false,
        }
    }

    /// Bildbreite in Pixeln.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Bildhöhe in Pixeln.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Byte-Stride beider Planen.
    /// Gibt den Buffer zur Zero-Copy-Übernahme heraus (Struktur danach leer;
    /// der nächste `process_frame` alloziert neu).
    pub fn into_data(mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// PTS in Sekunden (aus dem Quell-Frame, `av_frame_copy_props`-Äquivalent).
    pub fn pts(&self) -> f64 {
        self.pts
    }

    /// Frame-Dauer in Sekunden.
    pub fn duration(&self) -> f64 {
        self.duration
    }

    /// Verlorene Frames (aus dem Quell-Frame).
    pub fn frames_lost(&self) -> i32 {
        self.frames_lost
    }

    /// Frame nach FEC-Recovery (aus dem Quell-Frame).
    pub fn recovered(&self) -> bool {
        self.recovered
    }

    /// Echte Plane-Pointer (`[0]` = Y, `[1]` = interleaved UV) — gleicher
    /// Vertrag wie `DecodedFrame.planes`; `aligned_height == height` (der
    /// Buffer ist von uns alloziert, UV exakt bei `pitch*height`).
    pub fn planes(&self) -> [Plane; 2] {
        let y = self.buf.as_ptr() as *mut u8;
        let uv = unsafe { y.add(self.pitch * self.height as usize) };
        [
            Plane {
                data: NonNull::new(y).expect("Vec allocation is non-null"),
                stride: self.pitch,
            },
            Plane {
                data: NonNull::new(uv).expect("offset within allocation"),
                stride: self.pitch,
            },
        ]
    }

    /// Luma-Ebene als Slice (nur gültige Bildzeilen, Pitch-Padding inklusive —
    /// Stride-Vertrag wie `AVFrame.linesize`).
    pub fn y(&self) -> &[u8] {
        // SAFETY: buf ist pitch*height*3/2(+Slack) groß, Slice bleibt darin.
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr(), self.pitch * self.height as usize) }
    }

    /// Chroma-Ebene als Slice (interleaved UV, h/2 Zeilen).
    pub fn uv(&self) -> &[u8] {
        // SAFETY: UV beginnt bei pitch*height und umfasst pitch*h/2 Bytes.
        unsafe {
            std::slice::from_raw_parts(
                self.buf.as_ptr().add(self.pitch * self.height as usize),
                self.pitch * (self.height as usize / 2),
            )
        }
    }
}

/// Runtime-geladener NVIDIA Video Super Resolution-Upscaler.
///
/// API-Flow (wie C++ `VsrUpscaler`):
/// 1. [`VsrUpscaler::new`] — SDK-Pfad optional (`None` = Auto-Detect).
/// 2. [`VsrUpscaler::init`] mit dem ersten dekodierten Frame + den CUDA-
///    Kontext-Pointern des Decoders — schlägt fehl (kein SDK, keine CUDA-GPU),
///    ist VSR deaktiviert und `init` liefert `false` (kein Crash).
/// 3. [`VsrUpscaler::process_frame`] pro Frame — `true` = `out` enthält den
///    skalierten Frame, `false` = Original verwenden.
/// 4. Badge/Log über [`VsrUpscaler::is_active`], [`VsrUpscaler::last_error`],
///    [`VsrUpscaler::engine_load_ms`].
pub struct VsrUpscaler {
    /// Explizit gesetzter SDK-Pfad (`None` = Auto-Detect), C: `sdkBinDir`.
    sdk_bin_dir: Option<PathBuf>,

    dlls_loaded: bool,
    disabled: bool,
    fail_count: i32,
    active: bool,

    // SDK-Handles (Lebensdauer = Prozess; wird nie entladen — wie C++).
    _mod_ncvc_image: Option<Library>,
    _mod_nv_video_effects: Option<Library>,
    _mod_cuda: Option<Library>,
    api: Option<VfxApi>,

    effect: Option<NonNull<c_void>>,
    /// GPU RGBA, Input-Konvertierung (C: srcRgba).
    src_rgba: Option<Box<NvCVImage>>,
    /// GPU NV12, SDK-alloziert; die Dekoder-Planen werden pro Frame pro-Plane
    /// hierher gestagt (NVDEC aligned height!, C: srcStaged).
    src_staged: Option<Box<NvCVImage>>,
    /// GPU RGBA, VSR-Output (C: dstRgba).
    dst_rgba: Option<Box<NvCVImage>>,
    /// GPU NV12, Downkonvertierung (C: dstNv12).
    dst_nv12: Option<Box<NvCVImage>>,
    /// Leerer Tmp-Buffer für die konvertierenden Transfers; wächst SDK-seitig
    /// (C: transferTmp — ohne ihn schlagen CPU↔GPU-Transfers fehl).
    transfer_tmp: Box<NvCVImage>,

    /// CUDA-Kontext/-Stream des Decoders (C: AVCUDADeviceContext.hwctx).
    cuda_ctx: *mut c_void,
    cuda_stream: *mut c_void,

    in_size: (u32, u32),
    out_w: u32,
    out_h: u32,

    engine_load_ms: Option<u64>,
    last_error: Option<String>,
}

// SAFETY: VsrUpscaler besitzt seine SDK-Handles und GPU-Bilder exklusiv; die
// SDK-APIs sind pro Effekt nicht threadsicher — Übergabe zwischen Threads
// (Send) ist erlaubt, parallele Nutzung nicht.
unsafe impl Send for VsrUpscaler {}

impl Drop for VsrUpscaler {
    fn drop(&mut self) {
        // C: ~VsrUpscaler — DestroyEffect + Dealloc aller SDK-Bilder.
        let Some(api) = self.api else { return };
        unsafe {
            if let Some(effect) = self.effect {
                (api.vfx_destroy_effect)(effect.as_ptr());
            }
            // GPU-Deallocs ohne gepushten Kontext: das SDK handled das selbst
            // (der C++-Destruktor macht es genauso).
            let images: [*mut NvCVImage; 5] = [
                self.src_rgba.as_deref_mut().map_or(ptr::null_mut(), |b| b as *mut NvCVImage),
                self.src_staged.as_deref_mut().map_or(ptr::null_mut(), |b| b as *mut NvCVImage),
                self.dst_rgba.as_deref_mut().map_or(ptr::null_mut(), |b| b as *mut NvCVImage),
                self.dst_nv12.as_deref_mut().map_or(ptr::null_mut(), |b| b as *mut NvCVImage),
                &mut *self.transfer_tmp,
            ];
            for img in images {
                if !img.is_null() {
                    (api.image_dealloc)(img);
                }
            }
        }
    }
}

impl VsrUpscaler {
    /// C: `VsrUpscaler(ChiakiLog*, const QString &sdkBinDir)`.
    ///
    /// `sdk_bin_dir`: Ordner mit `NVVideoEffects.dll`/`NVCVImage.dll`
    /// (VFX-SDK "VideoFX/bin"); die Feature-DLLs werden in
    /// `<sdk_bin_dir>/../features/nvvfxvideosuperres/bin` erwartet.
    /// Wie im C++ wird ein **expliziter Pfad nicht durch Auto-Detect ersetzt**
    /// — existiert er nicht, bleibt VSR deaktiviert. `None` → Auto-Detect
    /// (`<exe>/../vfx_sdk/sdk/VideoFX/bin`, `<exe>/vfx_sdk/sdk/VideoFX/bin`,
    /// NVIDIA-Standard-Installationspfad; zusätzlich die Umgebungsvariable
    /// `CHIAKI_VSR_SDK_DIR` für Dev/Test — die App-Ebene setzt den Pfad aus
    /// `settings/nv_vsr_sdk_path`).
    pub fn new(sdk_bin_dir: Option<PathBuf>) -> Self {
        VsrUpscaler {
            sdk_bin_dir,
            dlls_loaded: false,
            disabled: false,
            fail_count: 0,
            active: false,
            _mod_ncvc_image: None,
            _mod_nv_video_effects: None,
            _mod_cuda: None,
            api: None,
            effect: None,
            src_rgba: None,
            src_staged: None,
            dst_rgba: None,
            dst_nv12: None,
            transfer_tmp: Box::new(NvCVImage::zeroed()),
            cuda_ctx: ptr::null_mut(),
            cuda_stream: ptr::null_mut(),
            in_size: (0, 0),
            out_w: 0,
            out_h: 0,
            engine_load_ms: None,
            last_error: None,
        }
    }

    /// Läuft VSR gerade? (C: `activeFlag` — Badge-Status.)
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Letzter Fehler (für Log/Badge); `None` solange alles glatt läuft.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Dauer des `NvVFX_Load` (TensorRT-Engine-Build/-Laden) in ms — fürs Log
    /// (C: "engine load NNNN ms").
    pub fn engine_load_ms(&self) -> Option<u64> {
        self.engine_load_ms
    }

    /// Output-Dimensionen nach erfolgreichem `init` (sonst 0x0).
    pub fn output_size(&self) -> (u32, u32) {
        (self.out_w, self.out_h)
    }

    /// VSR braucht zwingend den CUDA-Decoder — Info für die App-Ebene (das
    /// Erzwingen des CUDA-Decoders passiert dort, C: "If VSR is active, the
    /// client forces the CUDA decoder").
    pub fn requires_cuda_decoder() -> bool {
        true
    }

    fn set_error(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::error!("{msg}");
        self.last_error = Some(msg);
    }

    /// SDK-Pfad auflösen (C: `loadSdk`-Präambel). Expliziter Pfad ist
    /// autoritativ; sonst `CHIAKI_VSR_SDK_DIR`, sonst Auto-Detect-Kandidaten.
    fn resolve_sdk_dir(&self) -> Option<PathBuf> {
        // C: nicht-leerer sdkBinDir → nur dieser Ordner zählt.
        if let Some(dir) = &self.sdk_bin_dir {
            return if dir.join(DLL_NV_VIDEO_EFFECTS).exists() {
                Some(dir.clone())
            } else {
                None
            };
        }
        if let Some(env) = std::env::var_os("CHIAKI_VSR_SDK_DIR") {
            let dir = PathBuf::from(env);
            if dir.join(DLL_NV_VIDEO_EFFECTS).exists() {
                return Some(dir);
            }
        }
        // C: Auto-Detect — portable Layout legt das SDK neben den App-Ordner.
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(app_dir) = exe.parent() {
                candidates.push(app_dir.join("../vfx_sdk/sdk/VideoFX/bin"));
                candidates.push(app_dir.join("vfx_sdk/sdk/VideoFX/bin"));
            }
        }
        candidates.push(PathBuf::from(
            "C:/Program Files/NVIDIA Corporation/VFXSDK/VideoFX/bin",
        ));
        candidates
            .into_iter()
            .find(|c| c.join(DLL_NV_VIDEO_EFFECTS).exists())
    }

    fn feature_bin_dir(sdk_bin_dir: &Path) -> PathBuf {
        sdk_bin_dir.join("../features/nvvfxvideosuperres/bin")
    }

    /// DLL mit LOAD_WITH_ALTERED_SEARCH_PATH laden (Dependent-DLLs werden
    /// relativ zum DLL-Ordner gesucht) — Handles werden nie freigegeben
    /// (wie C++ LoadLibraryExW ohne FreeLibrary).
    fn load_library(path: &Path) -> Option<Library> {
        unsafe {
            // SAFETY: Handle bleibt für die Prozess-Laufzeit offen (Leak by
            // design — SDK/TensorRT halten globale Zustände).
            libloading::os::windows::Library::load_with_flags(
                path,
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
            .map(libloading::Library::from)
            .map_err(|e| {
                tracing::error!("VSR: failed to load {} ({e})", path.display());
                e
            })
            .ok()
        }
    }

    /// C: `loadSdk()` — lädt NVCVImage/NVVideoEffects (+Preloads, +nvcuda) und
    /// löst alle Symbole auf. Läuft ohne GPU.
    fn load_sdk(&mut self) -> bool {
        if self.dlls_loaded {
            return true;
        }

        let Some(sdk_bin_dir) = self.resolve_sdk_dir() else {
            // Bewusst nur info (kein Fehler): ohne SDK ist VSR einfach aus.
            tracing::info!(
                "VSR: VFX SDK not found (settings/nv_vsr_sdk_path oder \
                 <app>/vfx_sdk/sdk/VideoFX/bin), upscaler disabled"
            );
            return false;
        };
        let feature_bin_dir = Self::feature_bin_dir(&sdk_bin_dir);

        let Some(ncvc) = Self::load_library(&sdk_bin_dir.join(DLL_NVCV_IMAGE)) else {
            return false; // Meldung kam aus load_library
        };
        let Some(vfx) = Self::load_library(&sdk_bin_dir.join(DLL_NV_VIDEO_EFFECTS)) else {
            return false;
        };

        // Feature + Runtime vorladen, damit "VideoSuperRes" erstellt werden
        // kann und seine Abhängigkeiten auflösen (C: preload-Liste). Fehlende
        // Dateien überspringen, Fehlschläge nur warnen — wie C++.
        //
        // WICHTIG: Die Handles müssen LEAKEN (C++ LoadLibraryExW ohne
        // FreeLibrary)! Ein Drop (FreeLibrary) würde die Module sofort wieder
        // entladen — NvVFX_CreateEffect findet die Feature-DLL dann nicht
        // mehr (NVCV_ERR_UNIMPLEMENTED) und ihre Abhängigkeiten (nvngxruntime)
        // wären weg, bevor das SDK sie per Name auflöst.
        for dll in [
            sdk_bin_dir.join(PRELOAD_SDK_RUNTIME),
            feature_bin_dir.join(PRELOAD_FEATURE_NGONX),
            feature_bin_dir.join(PRELOAD_FEATURE_VSR),
        ] {
            if !dll.exists() {
                continue;
            }
            unsafe {
                // SAFETY: absichtlicher Leak via mem::forget (siehe oben).
                match libloading::os::windows::Library::load_with_flags(
                    &dll,
                    LOAD_WITH_ALTERED_SEARCH_PATH,
                ) {
                    Ok(lib) => std::mem::forget(lib),
                    Err(e) => tracing::warn!("VSR: failed to pre-load {} ({e:?})", dll.display()),
                }
            }
        }

        // nvcuda.dll liegt im System32 (Treiber); C++ versucht erst den
        // SDK-Ordner, dann die nackte Name-Suche (LoadLibraryW).
        let cuda = Self::load_library(&sdk_bin_dir.join(DLL_NVCUDA)).or_else(|| unsafe {
            // SAFETY: siehe oben.
            Library::new(DLL_NVCUDA).ok()
        });
        let Some(cuda) = cuda else {
            tracing::error!("VSR: nvcuda.dll not available");
            return false;
        };

        // --- Symbole auflösen (C: GetProcAddress-Block) ---
        macro_rules! sym {
            ($lib:expr, $name:literal as $ty:ty) => {
                match unsafe { $lib.get::<$ty>(concat!($name, "\0").as_bytes()) } {
                    Ok(f) => *f,
                    Err(_) => {
                        tracing::error!(
                            "VSR: SDK DLLs are missing expected exports (missing {})",
                            $name
                        );
                        return false;
                    }
                }
            };
        }

        let api = VfxApi {
            image_init: sym!(ncvc, "NvCVImage_Init" as sys::FnNvCVImageInit),
            image_alloc: sym!(ncvc, "NvCVImage_Alloc" as sys::FnNvCVImageAlloc),
            image_dealloc: sym!(ncvc, "NvCVImage_Dealloc" as sys::FnNvCVImageDealloc),
            image_transfer: sym!(ncvc, "NvCVImage_Transfer" as sys::FnNvCVImageTransfer),
            vfx_create_effect: sym!(vfx, "NvVFX_CreateEffect" as sys::FnNvVFXCreateEffect),
            vfx_destroy_effect: sym!(vfx, "NvVFX_DestroyEffect" as sys::FnNvVFXDestroyEffect),
            vfx_set_image: sym!(vfx, "NvVFX_SetImage" as sys::FnNvVFXSetImage),
            vfx_set_object: sym!(vfx, "NvVFX_SetObject" as sys::FnNvVFXSetObject),
            vfx_set_u32: sym!(vfx, "NvVFX_SetU32" as sys::FnNvVFXSetU32),
            vfx_set_f32: sym!(vfx, "NvVFX_SetF32" as sys::FnNvVFXSetF32),
            vfx_set_string: sym!(vfx, "NvVFX_SetString" as sys::FnNvVFXSetString),
            vfx_get_u32: sym!(vfx, "NvVFX_GetU32" as sys::FnNvVFXGetU32),
            vfx_load: sym!(vfx, "NvVFX_Load" as sys::FnNvVFXLoad),
            vfx_run: sym!(vfx, "NvVFX_Run" as sys::FnNvVFXRun),
            cu_ctx_push_current: sym!(cuda, "cuCtxPushCurrent" as sys::FnCuCtxPushCurrent),
            cu_ctx_pop_current: sym!(cuda, "cuCtxPopCurrent" as sys::FnCuCtxPopCurrent),
            cu_stream_synchronize: sym!(cuda, "cuStreamSynchronize" as sys::FnCuStreamSynchronize),
            cu_ctx_synchronize: sym!(cuda, "cuCtxSynchronize" as sys::FnCuCtxSynchronize),
        };

        tracing::info!("VSR: VFX SDK loaded from \"{}\"", sdk_bin_dir.display());
        self._mod_ncvc_image = Some(ncvc);
        self._mod_nv_video_effects = Some(vfx);
        self._mod_cuda = Some(cuda);
        self.api = Some(api);
        self.dlls_loaded = true;
        true
    }

    /// C: `init(AVFrame *firstFrame, int scalePct)`.
    ///
    /// Mit dem ersten dekodierten Frame aufrufen. `cuda_ctx`/`cuda_stream`
    /// stammen vom Decoder ([`crate::decoder::Decoder::cuda_context`]/
    /// [`crate::decoder::Decoder::cuda_stream`]) — im C++ werden sie aus
    /// `AVFrame.hw_frames_ctx → AVHWFramesContext.device_ref →
    /// AVHWDeviceContext.hwctx` gelesen; der Rust-Decoder exponiert dieselben
    /// Werte direkt.
    ///
    /// Liefert `true`, wenn VSR ab jetzt aktiv ist; bei `false` bleibt VSR
    /// deaktiviert (Frames laufen unskaliert weiter).
    // Die rohen CUDA-Handles werden 1:1 an die SDK-FFI durchgereicht (opaque
    // Pointer, nie in Rust dereferenziert) — daher der Lint-allow.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn init(
        &mut self,
        first_frame: &DecodedFrame,
        cuda_ctx: *mut c_void,
        cuda_stream: *mut c_void,
        scale_pct: u32,
    ) -> bool {
        if self.active || self.disabled {
            return self.active;
        }

        if !self.dlls_loaded && !self.load_sdk() {
            self.disabled = true;
            return false;
        }

        if first_frame.width == 0 || first_frame.height == 0 {
            self.set_error("VSR: first frame has no dimensions, disabling");
            self.disabled = true;
            return false;
        }
        // C: "first frame is not a CUDA hardware frame, disabling" — hier
        // äquivalent: ohne Decoder-CUDA-Kontext gäbe es keinen Kontext, in dem
        // GPU-Buffer und TensorRT-Engine leben könnten (Root Cause 3).
        if cuda_ctx.is_null() {
            self.set_error(
                "VSR: first frame is not a CUDA hardware frame (no decoder cuda_ctx), disabling",
            );
            self.disabled = true;
            return false;
        }
        // Parameter-Validation (App-Settings erlauben 100–400 %).
        if !(100..=400).contains(&scale_pct) {
            self.set_error(format!("VSR: invalid scale {scale_pct}% (allowed: 100..=400), disabling"));
            self.disabled = true;
            return false;
        }

        let api = self.api.expect("load_sdk succeeded");

        let (in_w, in_h) = (first_frame.width, first_frame.height);
        let (out_w, out_h) = output_dims(in_w, in_h, scale_pct);
        self.in_size = (in_w, in_h);
        self.out_w = out_w;
        self.out_h = out_h;
        self.cuda_ctx = cuda_ctx;
        self.cuda_stream = cuda_stream;

        // --- KOMPLETTER Setup-Block im Decoder-Kontext (REWORK.md Root Cause
        // 3): NvCVImage_Alloc alloziert GPU-Buffer im *current context* und
        // NvVFX_Load baut die TensorRT-Engine im *current context*. Ohne
        // Kontext: CUDA 201 → SDK-Fallback -1999 (NVCV_ERR_CUDA). ---
        let push_rc = unsafe { (api.cu_ctx_push_current)(cuda_ctx) };
        if push_rc != 0 {
            self.set_error(format!(
                "VSR: cuCtxPushCurrent(decoder) failed rc={push_rc}"
            ));
            self.disabled = true;
            return false;
        }

        // Der VSR-Filter konsumiert/produziert interleaved 8-bit RGBA
        // GPU-Buffer; der NV12-Staging-Buffer nimmt die Dekoder-Planen auf.
        let alloc_gpu_image = |w: u32,
                               h: u32,
                               fmt: c_int,
                               layout: u32|
         -> Option<Box<NvCVImage>> {
            let mut img = Box::new(NvCVImage::zeroed());
            let st = unsafe {
                (api.image_alloc)(&mut *img, w, h, fmt, NVCV_U8, layout, NVCV_GPU, 0)
            };
            if st != NVCV_SUCCESS {
                tracing::error!(
                    "VSR: failed to allocate GPU image {}x{} fmt={} (status {})",
                    w,
                    h,
                    fmt,
                    st
                );
                None
            } else {
                Some(img)
            }
        };

        let mut allocs_ok = true;
        if let Some(img) = alloc_gpu_image(in_w, in_h, NVCV_RGBA, NVCV_INTERLEAVED) {
            self.src_rgba = Some(img);
        } else {
            allocs_ok = false;
        }
        if let Some(img) = alloc_gpu_image(in_w, in_h, NVCV_YUV420, NVCV_NV12) {
            self.src_staged = Some(img);
        } else {
            allocs_ok = false;
        }
        if let Some(img) = alloc_gpu_image(out_w, out_h, NVCV_RGBA, NVCV_INTERLEAVED) {
            self.dst_rgba = Some(img);
        } else {
            allocs_ok = false;
        }
        if let Some(img) = alloc_gpu_image(out_w, out_h, NVCV_YUV420, NVCV_NV12) {
            self.dst_nv12 = Some(img);
        } else {
            allocs_ok = false;
        }

        if !allocs_ok {
            unsafe { self.pop_ctx() };
            self.disabled = true;
            return false;
        }

        // Leerer Tmp-Buffer für die konvertierenden Transfers; der SDK wächst
        // ihn bedarfsgesteuert (C: transferTmp).
        *self.transfer_tmp = NvCVImage::zeroed();

        let effect_name = CString::new(NVVFX_FX_VIDEO_SUPER_RES).expect("no NUL in effect name");
        let mut effect: *mut c_void = ptr::null_mut();
        let st = unsafe { (api.vfx_create_effect)(effect_name.as_ptr(), &mut effect) };
        if st != NVCV_SUCCESS || effect.is_null() {
            self.set_error(format!(
                "VSR: NvVFX_CreateEffect(\"{NVVFX_FX_VIDEO_SUPER_RES}\") failed (status {}) - \
                 is the nvvfxvideosuperres feature installed?",
                sys::status_name(st)
            ));
            unsafe { self.pop_ctx() };
            self.disabled = true;
            return false;
        }
        let effect = NonNull::new(effect).expect("checked non-null above");
        self.effect = Some(effect);

        // WICHTIG: NvVFX-Selektoren sind C-Strings und brauchen ein NUL-Byte —
        // `&str::as_ptr()` liefert KEINES (klassischer Rust-FFI-Stolperstein;
        // das C++-Original nutzt string-literals, die terminiert sind). Ein
        // fehlendes NUL macht aus "SrcImage0" einen Müll-Selektor →
        // NVCV_ERR_SELECTOR → "vsr-run -7". Deshalb überall CString.
        let sel_src = CString::new(NVVFX_INPUT_IMAGE_0).expect("no NUL");
        let sel_dst = CString::new(NVVFX_OUTPUT_IMAGE_0).expect("no NUL");
        let sel_stream = CString::new(NVVFX_CUDA_STREAM).expect("no NUL");
        let sel_quality = CString::new(NVVFX_QUALITY_LEVEL).expect("no NUL");

        let src_img: *mut NvCVImage = self.src_rgba.as_deref_mut().expect("alloced");
        let dst_img: *mut NvCVImage = self.dst_rgba.as_deref_mut().expect("alloced");
        unsafe {
            let rc_src = (api.vfx_set_image)(effect.as_ptr(), sel_src.as_ptr(), src_img);
            let rc_dst = (api.vfx_set_image)(effect.as_ptr(), sel_dst.as_ptr(), dst_img);
            // Abweichung zum C++ (das die rc-Werte ignoriert): Ein
            // NVCV_ERR_SELECTOR hier bedeutet einen kaputten/entarteten
            // Effekt (z. B. nicht geladene Feature-DLL). Dann hilft nur
            // sauberes Deaktivieren; stummes Weiterlaufen würde
            // 30 x "vsr-run -7" produzieren.
            if rc_src != NVCV_SUCCESS || rc_dst != NVCV_SUCCESS {
                self.set_error(format!(
                    "VSR: NvVFX_SetImage failed (src {}, dst {}) - effect has no image parameters, \
                     disabling",
                    sys::status_name(rc_src),
                    sys::status_name(rc_dst)
                ));
                self.pop_ctx();
                self.disabled = true;
                return false;
            }
            if !self.cuda_stream.is_null() {
                (api.vfx_set_object)(effect.as_ptr(), sel_stream.as_ptr(), self.cuda_stream);
            }
            // Qualität/Methode des VSR-Netzes (C: "QualityLevel" — 0 bicubic,
            // 1 low, 2 medium, 3 high, 4 ultra).
            let rc_q = (api.vfx_set_u32)(
                effect.as_ptr(),
                sel_quality.as_ptr(),
                quality_for_scale(scale_pct),
            );
            if rc_q != NVCV_SUCCESS {
                tracing::warn!(
                    "VSR: NvVFX_SetU32(QualityLevel) failed (status {}), continuing with default",
                    sys::status_name(rc_q)
                );
            }

            // Load baut/lädt die TensorRT-Engine; transiente Fehler wurden
            // beobachtet (Engine-Cache-Konflikt nach hartem Prozess-Kill) —
            // daher ein Retry (C: Sleep(200) + zweiter Load).
            let load_timer = Instant::now();
            let mut load_st = (api.vfx_load)(effect.as_ptr());
            if load_st != NVCV_SUCCESS {
                tracing::error!(
                    "VSR: NvVFX_Load failed (status {}), retrying once",
                    sys::status_name(load_st)
                );
                std::thread::sleep(Duration::from_millis(200));
                load_st = (api.vfx_load)(effect.as_ptr());
            }
            let elapsed_ms = load_timer.elapsed().as_millis() as u64;

            if load_st != NVCV_SUCCESS {
                tracing::error!(
                    "VSR: NvVFX_Load failed again (status {}) - check nvvfxvideosuperres \
                     feature install and TensorRT engine cache",
                    sys::status_name(load_st)
                );
                self.last_error = Some(format!("NvVFX_Load failed ({})", sys::status_name(load_st)));
                // popCtx VOR dem Fehler-Ausgang (C: popCtx direkt nach Load).
                self.pop_ctx();
                self.disabled = true;
                return false;
            }

            self.engine_load_ms = Some(elapsed_ms);
        }

        unsafe { self.pop_ctx() };

        self.active = true;
        self.fail_count = 0;
        tracing::info!(
            "VSR: NVIDIA Video Super Resolution active ({}x{} -> {}x{}, quality {}, \
             engine load {} ms)",
            in_w,
            in_h,
            out_w,
            out_h,
            quality_for_scale(scale_pct),
            self.engine_load_ms.expect("just set")
        );
        true
    }

    /// C: `process(AVFrame *in)` — Upscale-Pfad pro Frame.
    ///
    /// Liefert `true`, wenn `out` mit dem skalierten CPU-NV12-Frame gefüllt
    /// wurde; `false` = Original-Frame verwenden (VSR inaktiv, Fehler-Pass-
    /// through, Dimensionswechsel). Deaktiviert sich nach wiederholten
    /// Fehlern selbst (> [`MAX_CONSECUTIVE_FAILURES`], C: failCount > 30).
    ///
    /// Pipeline (exakt wie C++, GPU-Teile im Decoder-CUDA-Kontext):
    /// 1. pro-Plane-Staging der Dekoder-Planen in den SDK-NV12-Staging-Buffer
    ///    (echte `planes[0]`/`planes[1]`-Pointer — **nie** `Y + pitch*height`,
    ///    REWORK.md Root Cause 1),
    /// 2. NV12 → RGBA (GPU, `NvCVImage_Transfer`),
    /// 3. `NvVFX_Run` (VSR-Netz, "vsr-run"),
    /// 4. RGBA → NV12 (GPU),
    /// 5. GPU-NV12 → kontiguierlicher CPU-NV12-[`FrameBuf`]
    ///    (`Y@0 + UV@pitch*h` — grüner-Streifen/Heap-Overwrite-Fix).
    pub fn process_frame(&mut self, frame: &DecodedFrame, out: &mut FrameBuf) -> bool {
        if !self.active || self.disabled {
            return false;
        }
        // Verteidigung (das C++ trustet die init-Größe): ein Dimensionswechsel
        // im Stream würde Staging-/Output-Buffer überlaufen — Pass-through
        // statt UB. Bei PS Remote Play bleibt die Größe session-stabil.
        if (frame.width, frame.height) != self.in_size {
            if self.fail_count == 0 {
                tracing::error!(
                    "VSR: frame size {}x{} != init size {}x{}, passing frame through",
                    frame.width,
                    frame.height,
                    self.in_size.0,
                    self.in_size.1
                );
            }
            return false;
        }
        self.process_planes(
            frame.planes[0].as_mut_ptr().cast(),
            frame.planes[0].stride,
            frame.planes[1].as_mut_ptr().cast(),
            frame.planes[1].stride,
            frame.pts,
            frame.duration,
            frame.frames_lost,
            frame.recovered,
            NVCV_CPU,
            true,
            out,
        )
    }

    /// GPU-Pfad (Zero-Copy): VSR aus CUDA-Device-Pointern (NVDEC-CUDA-Raw-
    /// Output — `AV_PIX_FMT_CUDA`, `data[0]`/`data[1]`, Pitch `linesize[0]`;
    /// Referenz: C++ `vsrupscaler.cpp` stagt dieselben Pointer mit
    /// `NVCV_MEM_GPU`-Views). Läuft komplett auf der GPU und lässt den Output
    /// als GPU-RGBA liegen ([`Self::gpu_rgba_output`]) — für den
    /// CUDA-D3D11-Interop-Schreibvorgang in die Video-Textur. KEIN CPU-
    /// Roundtrip; `out` bleibt unbenutzt.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn process_frame_gpu(
        &mut self,
        y_dev: *const c_void,
        y_pitch: usize,
        uv_dev: *const c_void,
        uv_pitch: usize,
        width: u32,
        height: u32,
        pts: f64,
        duration: f64,
        frames_lost: i32,
        recovered: bool,
    ) -> bool {
        if !self.active || self.disabled {
            return false;
        }
        if (width, height) != self.in_size {
            if self.fail_count == 0 {
                tracing::error!(
                    "VSR: frame size {}x{} != init size {}x{}, passing frame through",
                    width,
                    height,
                    self.in_size.0,
                    self.in_size.1
                );
            }
            return false;
        }
        let mut unused = FrameBuf::new();
        self.process_planes(
            y_dev as *mut u8,
            y_pitch,
            uv_dev as *mut u8,
            uv_pitch,
            pts,
            duration,
            frames_lost,
            recovered,
            NVCV_GPU,
            false,
            &mut unused,
        )
    }

    /// GPU-RGBA-Output des letzten erfolgreichen [`Self::process_frame_gpu`]:
    /// `(Device-Pointer, Pitch, Breite, Höhe)`. Gültig bis zum nächsten
    /// `process_frame_gpu`/Drop (SDK-Buffer im Decoder-CUDA-Kontext — der
    /// Interop-Schreibvorgang muss denselben Kontext pushen).
    pub fn gpu_rgba_output(&self) -> Option<(*const c_void, usize, u32, u32)> {
        if !self.active {
            return None;
        }
        let dst = self.dst_rgba.as_deref()?;
        Some((
            dst.pixels as *const c_void,
            dst.pitch.max(0) as usize,
            self.out_w,
            self.out_h,
        ))
    }

    /// Rohzeiger auf das GPU-RGBA-NvCVImage des letzten erfolgreichen
    /// [`Self::process_frame_gpu`] — Quelle für das SDK-D3D11-Interop
    /// ([`crate::cuda_d3d11::CudaD3d11Interop::write_from`]). Gültig bis zum
    /// nächsten `process_frame_gpu`/Drop.
    pub fn gpu_rgba_image(&self) -> Option<*const NvCVImage> {
        if !self.active {
            return None;
        }
        self.dst_rgba.as_deref().map(|img| img as *const NvCVImage)
    }

    /// VSR aus einem KOPIERTEN NV12-Frame (besessene Planes, z. B. der
    /// Media-Thread hält nur den neuesten Frame als NV12Frame). Layout:
    /// `y = data[0..y_stride*h]`, `uv = data[y_stride*h ..]`.
    #[allow(clippy::too_many_arguments)]
    pub fn process_frame_nv12(
        &mut self,
        y: &[u8],
        uv: &[u8],
        y_stride: usize,
        uv_stride: usize,
        width: u32,
        height: u32,
        pts: f64,
        duration: f64,
        frames_lost: i32,
        recovered: bool,
        out: &mut FrameBuf,
    ) -> bool {
        if !self.active || self.disabled {
            return false;
        }
        if (width, height) != self.in_size {
            if self.fail_count == 0 {
                tracing::error!(
                    "VSR: frame size {}x{} != init size {}x{}, passing frame through",
                    width,
                    height,
                    self.in_size.0,
                    self.in_size.1
                );
            }
            return false;
        }
        let y_ptr = y.as_ptr() as *mut u8;
        let uv_ptr = uv.as_ptr() as *mut u8;
        self.process_planes(
            y_ptr,
            y_stride,
            uv_ptr,
            uv_stride,
            pts,
            duration,
            frames_lost,
            recovered,
            NVCV_CPU,
            true,
            out,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn process_planes(
        &mut self,
        y_ptr: *mut u8,
        y_stride: usize,
        uv_ptr: *mut u8,
        uv_stride: usize,
        pts: f64,
        duration: f64,
        frames_lost: i32,
        recovered: bool,
        src_memory: c_uint,
        convert_out_to_cpu: bool,
        out: &mut FrameBuf,
    ) -> bool {
        let api = self.api.expect("active implies api");
        let effect = self.effect.expect("active implies effect");
        let (in_w, in_h) = self.in_size;
        let (out_w, out_h) = (self.out_w, self.out_h);
        let staged: &NvCVImage = self.src_staged.as_deref().expect("alloced");

        unsafe { (api.cu_ctx_push_current)(self.cuda_ctx) };

        // --- 1) pro-Plane-Staging (NVDEC aligned height!) ---
        // Wir dürfen dem SDK KEINE NV12-View über die Dekoder-Planes geben:
        // der UV-Start liegt NICHT bei Y + pitch*height (aligned height!).
        // Deshalb werden Y und UV separat als einfache NVCV_Y-Views
        // transferiert — Pitch/Pointer/Kontexte handhabt der SDK-Transfer
        // selbst. Quelle: CPU-Planes des Decoders (NVCV_CPU, siehe Modul-
        // dokumentation); Ziel: GPU-Staging (NVCV_GPU, SDK-NV12-Layout).
        let mut stage_failed = false;
        {
            let mut src_y = NvCVImage::zeroed();
            let mut src_uv = NvCVImage::zeroed();
            let mut dst_y = NvCVImage::zeroed();
            let mut dst_uv = NvCVImage::zeroed();

            let (rc_y, rc_uv) = unsafe {
                (api.image_init)(
                    &mut src_y,
                    in_w,
                    in_h,
                    y_stride as c_int,
                    y_ptr.cast(),
                    NVCV_Y,
                    NVCV_U8,
                    NVCV_INTERLEAVED,
                    src_memory,
                );
                (api.image_init)(
                    &mut src_uv,
                    in_w,
                    in_h / 2,
                    uv_stride as c_int,
                    uv_ptr.cast(),
                    NVCV_Y,
                    NVCV_U8,
                    NVCV_INTERLEAVED,
                    src_memory,
                );
                (api.image_init)(
                    &mut dst_y,
                    in_w,
                    in_h,
                    staged.pitch,
                    staged.pixels,
                    NVCV_Y,
                    NVCV_U8,
                    NVCV_INTERLEAVED,
                    NVCV_GPU,
                );
                // SDK-NV12-Layout: UV liegt bei Y + pitch*h (Bildhöhe — der
                // Staging-Buffer ist von uns alloziert, kein Alignment-Polster).
                (api.image_init)(
                    &mut dst_uv,
                    in_w,
                    in_h / 2,
                    staged.pitch,
                    (staged.pixels as *mut u8)
                        .add(staged.pitch as usize * in_h as usize)
                        .cast(),
                    NVCV_Y,
                    NVCV_U8,
                    NVCV_INTERLEAVED,
                    NVCV_GPU,
                );

                let rc_y = (api.image_transfer)(
                    &src_y,
                    &mut dst_y,
                    1.0,
                    self.cuda_stream,
                    &mut *self.transfer_tmp,
                );
                let rc_uv = if rc_y == NVCV_SUCCESS {
                    (api.image_transfer)(
                        &src_uv,
                        &mut dst_uv,
                        1.0,
                        self.cuda_stream,
                        &mut *self.transfer_tmp,
                    )
                } else {
                    NVCV_SUCCESS
                };
                (rc_y, rc_uv)
            };
            if rc_y != NVCV_SUCCESS || rc_uv != NVCV_SUCCESS {
                stage_failed = true;
                if self.fail_count == 0 {
                    tracing::error!(
                        "VSR: plane staging failed (Y rc={} ({}), UV rc={} ({}))",
                        rc_y,
                        sys::status_name(rc_y),
                        rc_uv,
                        sys::status_name(rc_uv)
                    );
                }
            }
        }

        // --- 2) Staging-NV12 → RGBA, 3) VSR-Netz, 4) RGBA → NV12 ---
        let src_rgba: *mut NvCVImage = self.src_rgba.as_deref_mut().expect("alloced");
        let dst_rgba: *mut NvCVImage = self.dst_rgba.as_deref_mut().expect("alloced");
        let dst_nv12: *mut NvCVImage = self.dst_nv12.as_deref_mut().expect("alloced");
        let tmp: *mut NvCVImage = &mut *self.transfer_tmp;

        let mut failed_stage: &str = if stage_failed {
            "stage (NVDEC->NV12 planes)"
        } else {
            ""
        };
        let mut st = if stage_failed {
            NVCV_ERR_GENERAL
        } else {
            // C: NvCVImage_Transfer(staged, srcRgba, ...) — Ziel ist die
            // VSR-INPUT-Struktur (srcRgba), NICHT der Output-Buffer!
            let rc = unsafe {
                (api.image_transfer)(staged, src_rgba, 1.0, self.cuda_stream, tmp)
            };
            if rc != NVCV_SUCCESS {
                failed_stage = "convert-in (NV12->RGBA)";
            }
            rc
        };
        if st == NVCV_SUCCESS {
            // NvVFX_Run liest SrcImage0 auf seinem SDK-internen Stream (das
            // "CudaStream"-SetObject ist im 1.2er-Wrapper nicht implementiert,
            // -2); ohne Kontext-Sync liest es srcRgba, BEVOR convert-in
            // fertig ist → Schwarz-Bild. Unsere Transfers laufen auf dem
            // Default-Stream, der mit fremden Streams nicht geordnet ist.
            unsafe { (api.cu_ctx_synchronize)() };
        }
        if st == NVCV_SUCCESS {
            st = unsafe { (api.vfx_run)(effect.as_ptr(), 0) };
            if st != NVCV_SUCCESS {
                failed_stage = "vsr-run";
                // Log-Zeile analog zur Task-Vorgabe („vsr-run status …“).
                tracing::error!("vsr-run status {st} ({})", sys::status_name(st));
            } else {
                // Run liefert asynchron auf dem SDK-internen Stream — erst
                // der Kontext-Sync garantiert, dass convert-out das fertige
                // dstRgba liest (ohne: schwarze Frames).
                unsafe { (api.cu_ctx_synchronize)() };
            }
        }
        if st == NVCV_SUCCESS && convert_out_to_cpu {
            let rc = unsafe {
                (api.image_transfer)(
                    dst_rgba as *const NvCVImage,
                    dst_nv12,
                    1.0,
                    self.cuda_stream,
                    tmp,
                )
            };
            st = rc;
            if st != NVCV_SUCCESS {
                failed_stage = "convert-out (RGBA->NV12)";
            }
        }
        if st == NVCV_SUCCESS {
            // Nach dem VSR-Run stets synchronisieren — im GPU-Modus liest der
            // nachfolgende Interop-Kopiervorgang (Default-Stream) dstRgba, das
            // das VSR-Netz auf seinem SDK-internen Stream geschrieben hat.
            unsafe { (api.cu_ctx_synchronize)() };
            unsafe { (api.cu_stream_synchronize)(self.cuda_stream) };
        }
        unsafe { self.pop_ctx() };

        if !convert_out_to_cpu {
            // GPU-Modus: der Output bleibt auf der GPU (siehe
            // gpu_rgba_output); der CPU-Download entfällt komplett.
            if st != NVCV_SUCCESS {
                if self.fail_count == 0 {
                    tracing::error!(
                        "VSR: GPU pipeline failed in stage '{}' (status {} ({}))",
                        failed_stage,
                        st,
                        sys::status_name(st)
                    );
                    self.last_error = Some(format!(
                        "GPU pipeline failed in stage '{failed_stage}' (status {st})"
                    ));
                }
                self.fail_count += 1;
                if self.fail_count > MAX_CONSECUTIVE_FAILURES {
                    tracing::error!(
                        "VSR: GPU pipeline keeps failing (status {}), disabling upscaler",
                        st
                    );
                    self.disabled = true;
                    self.active = false;
                }
                return false;
            }
            self.fail_count = 0;
            return true;
        }

        if st != NVCV_SUCCESS {
            if self.fail_count == 0 {
                tracing::error!(
                    "VSR: pipeline failed in stage '{}' (status {} ({}))",
                    failed_stage,
                    st,
                    sys::status_name(st)
                );
                self.last_error = Some(format!(
                    "pipeline failed in stage '{failed_stage}' (status {st})"
                ));
            }
            self.fail_count += 1;
            if self.fail_count > MAX_CONSECUTIVE_FAILURES {
                tracing::error!(
                    "VSR: pipeline keeps failing (status {}), disabling upscaler",
                    st
                );
                self.disabled = true;
                self.active = false;
            }
            return false;
        }

        // --- 5) kontiguierlicher CPU-Output (grüner-Streifen/Heap-Fix) ---
        // FFmpegs Allokatoren nutzen getrennte Plane-Buffer mit Padding-Lücken,
        // NvCVImage adressiert NV12 als Y@0 .. pitch*h + UV@pitch*h .. —
        // deshalb ein eigener Buffer mit exakt diesem Layout.
        let pitch = nv12_output_pitch(out_w);
        let y_size = pitch * out_h as usize;
        let uv_size = pitch * (out_h as usize >> 1);
        let total_size = y_size + uv_size + 64; // +64 Slack wie C (av_malloc)
        if out.buf.len() < total_size {
            out.buf.resize(total_size, 0);
        }
        out.width = out_w;
        out.height = out_h;
        out.pitch = pitch;

        let mut cpu_view = NvCVImage::zeroed();
        unsafe {
            (api.image_init)(
                &mut cpu_view,
                out_w,
                out_h,
                pitch as c_int,
                out.buf.as_mut_ptr().cast(),
                NVCV_YUV420,
                NVCV_U8,
                NVCV_NV12,
                NVCV_CPU,
            );

            (api.cu_ctx_push_current)(self.cuda_ctx);
            let tst = (api.image_transfer)(
                dst_nv12 as *const NvCVImage,
                &mut cpu_view,
                1.0,
                self.cuda_stream,
                &mut *self.transfer_tmp,
            );
            (api.cu_stream_synchronize)(self.cuda_stream);
            self.pop_ctx();

            if tst != NVCV_SUCCESS {
                if self.fail_count == 0 {
                    tracing::error!(
                        "VSR: result transfer failed (status {} ({}))",
                        tst,
                        sys::status_name(tst)
                    );
                    self.last_error = Some(format!("result transfer failed (status {tst})"));
                }
                self.fail_count += 1;
                if self.fail_count > MAX_CONSECUTIVE_FAILURES {
                    tracing::error!(
                        "VSR: result transfer keeps failing (status {}), disabling upscaler",
                        tst
                    );
                    self.disabled = true;
                    self.active = false;
                }
                return false;
            }
        }

        self.fail_count = 0;

        // Metadaten übernehmen (Äquivalent zu av_frame_copy_props).
        out.pts = pts;
        out.duration = duration;
        out.frames_lost = frames_lost;
        out.recovered = recovered;
        true
    }

    /// C: `popCtx(nullptr)` — Kontext wieder zurücksetzen.
    ///
    /// # Safety
    /// Nur aufrufen, wenn vorher `cu_ctx_push_current` erfolgreich war.
    unsafe fn pop_ctx(&mut self) {
        let Some(api) = self.api else { return };
        let mut old = ptr::null_mut();
        (api.cu_ctx_pop_current)(&mut old);
    }
}

/// NVCV-Wert-Konsistenz (Header vs. Port) — weitere Asserts in sys.rs.
const _: () = {
    assert!(NVCV_Y == 1);
    assert!(NVCV_RGBA == 6);
    assert!(NVCV_YUV420 == 10);
    assert!(NVCV_U8 == 1);
    assert!(NVCV_NV12 == 7);
    assert!(NVCV_CPU == 0);
    assert!(NVCV_GPU == 1);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::FrameMemory;

    // --- Pure Helper (Parameter-Validation-Pfad, ohne GPU/DLLs) ------------

    #[test]
    fn output_dims_matches_c_formula() {
        // C: (w * pct / 100) & ~1
        assert_eq!(output_dims(1280, 720, 200), (2560, 1440));
        assert_eq!(output_dims(1920, 1080, 200), (3840, 2160));
        assert_eq!(output_dims(1920, 1080, 400), (7680, 4320));
        assert_eq!(output_dims(1280, 720, 150), (1920, 1080));
        // gerade-Zwang: ungerade Produkte werden abgerundet
        assert_eq!(output_dims(100, 100, 101), (100, 100));
        assert_eq!(output_dims(33, 33, 100), (32, 32));
    }

    #[test]
    fn quality_follows_scale_threshold() {
        assert_eq!(quality_for_scale(100), 2);
        assert_eq!(quality_for_scale(200), 2);
        assert_eq!(quality_for_scale(299), 2);
        assert_eq!(quality_for_scale(300), 3);
        assert_eq!(quality_for_scale(400), 3);
    }

    #[test]
    fn output_pitch_is_64_aligned() {
        assert_eq!(nv12_output_pitch(2560), 2560);
        assert_eq!(nv12_output_pitch(3841), 3904); // nächstes 64er-Vielfaches
        assert_eq!(nv12_output_pitch(1), 64);
    }

    #[test]
    fn framebuf_layout_is_contiguous_nv12() {
        // Der berühmte Fix: Y@0 + UV@pitch*h, keine FFmpeg-Padding-Lücken.
        let mut fb = FrameBuf::new();
        let pitch = nv12_output_pitch(2560);
        fb.buf = vec![0u8; pitch * 1440 * 3 / 2 + 64];
        fb.width = 2560;
        fb.height = 1440;
        fb.pitch = pitch;
        let planes = fb.planes();
        assert_eq!(planes[0].stride, pitch);
        assert_eq!(planes[1].stride, pitch);
        let y_addr = planes[0].as_ptr() as usize;
        let uv_addr = planes[1].as_ptr() as usize;
        assert_eq!(uv_addr - y_addr, pitch * 1440); // exakt pitch*h
        assert_eq!(
            crate::decoder::nv12_aligned_height(y_addr, uv_addr, pitch),
            Some(1440),
            "FrameBuf: aligned_height == height (kontiguierliches Eigen-Layout)"
        );
        assert_eq!(fb.y().len(), pitch * 1440);
        assert_eq!(fb.uv().len(), pitch * 720);
    }

    // --- Status-Mapping ------------------------------------------------------

    #[test]
    fn status_names_cover_known_codes() {
        assert_eq!(sys::status_name(0), "NVCV_SUCCESS");
        assert_eq!(sys::status_name(-1), "NVCV_ERR_GENERAL");
        assert_eq!(sys::status_name(-5), "NVCV_ERR_SELECTOR");
        assert_eq!(sys::status_name(-7), "NVCV_ERR_PARAMETER");
        assert_eq!(sys::status_name(-12), "NVCV_ERR_INITIALIZATION");
        // -1999 muss als ungemappter CUDA-Fehler erkennbar bleiben (REWORK.md
        // Root Cause 3: NvVFX_Load ohne Kontext → -1999).
        assert!(
            sys::status_name(-1999).contains("INVALID_CONTEXT=201"),
            "-1999-Dokumentation fehlt"
        );
        assert!(sys::status_name(-1002).starts_with("NVCV_ERR_CUDA"));
    }

    // --- Zustandsmaschine (ohne GPU/DLL) ------------------------------------

    #[test]
    fn inactive_by_default_and_passthrough_without_init() {
        let up = VsrUpscaler::new(None);
        assert!(!up.is_active());
        assert!(up.last_error().is_none());
        assert_eq!(up.output_size(), (0, 0));
        assert!(VsrUpscaler::requires_cuda_decoder());

        // process_frame vor init: Passthrough.
        let mut up = VsrUpscaler::new(None);
        let (_buf, frame) = synthetic_nv12_frame(128, 128);
        let mut out = FrameBuf::new();
        assert!(!up.process_frame(&frame, &mut out));
    }

    #[test]
    fn missing_sdk_disables_cleanly() {
        // Expliziter (unautoritativer, leerer) SDK-Pfad, der garantiert keine
        // NVVideoEffects.dll enthält → Zustandsmaschine: init false, disabled,
        // is_active() == false, kein Crash. Explizite Pfade schlagen Auto-
        // Detect (wie im C++: nicht-leerer sdkBinDir → nur dieser Ordner
        // zählt) — daher deterministisch, auch wenn auf dieser Maschine ein
        // echtes SDK per CHIAKI_VSR_SDK_DIR erreichbar ist.
        let mut up = VsrUpscaler::new(Some(PathBuf::from("Z:/definitiv/nicht/vorhanden")));
        let (_buf, frame) = synthetic_nv12_frame(128, 128);
        assert!(
            !up.init(&frame, ptr::null_mut(), ptr::null_mut(), 200),
            "init muss ohne SDK false liefern"
        );
        assert!(!up.is_active());
        assert!(up.last_error().is_none(), "fehlendes SDK ist kein Fehler");

        // Nach disabled bleibt process_frame Passthrough.
        let mut out = FrameBuf::new();
        assert!(!up.process_frame(&frame, &mut out));
        assert!(!up.is_active());
    }

    #[test]
    fn init_is_idempotent_once_disabled() {
        // Zweiter init nach disabled liefert weiter false (C: disabled-Flag).
        let mut up = VsrUpscaler::new(Some(PathBuf::from("Z:/definitiv/nicht/vorhanden")));
        let (_buf, frame) = synthetic_nv12_frame(128, 128);
        assert!(!up.init(&frame, ptr::null_mut(), ptr::null_mut(), 200));
        assert!(!up.init(&frame, ptr::null_mut(), ptr::null_mut(), 200));
        assert!(!up.is_active());
    }

    // --- Hilfskonstrukte für die Tests ---------------------------------------

    /// Synthetischer NV12-Frame (128er-Gradient + neutrales Chroma) im
    /// CPU-Layout mit echten per-Plane-Pointern (Y und UV getrennt — wie der
    /// Decoder sie liefert).
    pub(crate) fn synthetic_nv12_frame(w: u32, h: u32) -> (Vec<u8>, DecodedFrame) {
        let pitch = w as usize;
        let y_size = pitch * h as usize;
        let uv_size = pitch * h as usize / 2;
        let mut buf = vec![0u8; y_size + uv_size];
        for (i, b) in buf[..y_size].iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        for b in &mut buf[y_size..] {
            *b = 128;
        }
        let base = NonNull::new(buf.as_ptr() as *mut u8).expect("non-null");
        let frame = DecodedFrame {
            width: w,
            height: h,
            format: crate::decoder::FrameFormat::Nv12,
            planes: [
                Plane {
                    data: base,
                    stride: pitch,
                },
                Plane {
                    data: unsafe { base.add(y_size) },
                    stride: pitch,
                },
            ],
            aligned_height: h,
            pts: 0.5,
            duration: 1.0 / 60.0,
            frames_lost: 2,
            recovered: false,
            memory: FrameMemory::Cpu,
        };
        (buf, frame)
    }

    // Hinweis: Der volle GPU-Test (Init + Ein-Frame-Upscale) lebt als
    // Integrationstest in tests/vsr_gpu.rs — der NGX-Runtime-Manager meldet
    // in cargo-libtest-Prozessen mitunter einen degradierten Feature-Zustand
    // (NvVFX_SetImage → NVCV_ERR_SELECTOR), in Integrationstest-Prozessen
    // funktioniert derselbe Code deterministisch (siehe Modul-Doku oben).

    // --- DLL-Load + Symbol-Resolve (läuft ohne GPU) --------------------------

    #[test]
    fn sdk_dlls_load_and_export_all_symbols() {
        crate::test_setup::reference_dlls();
        let Some(dir) = std::env::var_os("CHIAKI_VSR_SDK_DIR").map(PathBuf::from) else {
            panic!("CHIAKI_VSR_SDK_DIR nicht gesetzt (test_setup)");
        };
        assert!(dir.join(DLL_NV_VIDEO_EFFECTS).exists());
        assert!(dir.join(DLL_NVCV_IMAGE).exists());

        unsafe {
            // SAFETY: Handles nur für die Testdauer (Leak ok).
            let ncvc = libloading::os::windows::Library::load_with_flags(
                dir.join(DLL_NVCV_IMAGE),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
            .expect("NVCVImage.dll muss ohne GPU ladbar sein");
            let vfx = libloading::os::windows::Library::load_with_flags(
                dir.join(DLL_NV_VIDEO_EFFECTS),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
            .expect("NVVideoEffects.dll muss ohne GPU ladbar sein");
            let ncvc: Library = ncvc.into();
            let vfx: Library = vfx.into();

            // Alle im VSR-Pfad gebrauchten Exporte — exakt die load_sdk-Liste.
            for name in [
                "NvCVImage_Init",
                "NvCVImage_Alloc",
                "NvCVImage_Dealloc",
                "NvCVImage_Transfer",
            ] {
                assert!(
                    ncvc.get::<*const c_void>(format!("{name}\0").as_bytes()).is_ok(),
                    "{name} fehlt in NVCVImage.dll"
                );
            }
            for name in [
                "NvVFX_CreateEffect",
                "NvVFX_DestroyEffect",
                "NvVFX_SetImage",
                "NvVFX_SetObject",
                "NvVFX_SetU32",
                "NvVFX_SetF32",
                "NvVFX_SetString",
                "NvVFX_GetU32",
                "NvVFX_Load",
                "NvVFX_Run",
            ] {
                assert!(
                    vfx.get::<*const c_void>(format!("{name}\0").as_bytes()).is_ok(),
                    "{name} fehlt in NVVideoEffects.dll"
                );
            }
        }

        // nvcuda.dll (Treiber) ist für diesen Test optional — ohne
        // NVIDIA-Treiber existiert sie nicht; mit Treiber müssen die drei
        // Symbole (und nur die werden genutzt!) auflösbar sein.
        unsafe {
            if let Ok(cuda) = Library::new(DLL_NVCUDA) {
                for name in ["cuCtxPushCurrent", "cuCtxPopCurrent", "cuStreamSynchronize"] {
                    assert!(
                        cuda.get::<*const c_void>(format!("{name}\0").as_bytes()).is_ok(),
                        "{name} fehlt in nvcuda.dll"
                    );
                }
            } else {
                tracing::info!("nvcuda.dll nicht ladbar (kein NVIDIA-Treiber) — übersprungen");
            }
        }
    }
}
