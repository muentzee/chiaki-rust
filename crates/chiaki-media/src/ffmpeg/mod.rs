// SPDX-License-Identifier: AGPL-3.0-only
//! FFmpeg-Zugriff über **dynamisches Laden** (libloading) — bewusst KEINE
//! ffmpeg-sys/ffmpeg-next-Crate, denn die erwarten Linkzeit-Bindings (.lib) und
//! eine feste Version. Wir laden genau die Shared-Build-DLLs, die der chiaki-ng-
//! Remaster-Build mitbringt:
//!
//! - `avutil-59.dll`, `avcodec-61.dll`, `swscale-8.dll` (FFmpeg n7.1)
//!
//! Suche-Reihenfolge (erste Fundstelle gewinnt, pro Kandidaten-Ordner werden alle
//! drei DLLs gemeinsam geladen):
//! 1. exe-dir
//! 2. `<exe-dir>/ffmpeg/bin` ( portable chiaki-Layout)
//! 3. `CHIAKI_FFMPEG_DIR` (Umgebung)
//! 4. PATH (Standard-DLL-Suche mit nacktem Namen)
//!
//! Beim ersten `init()` wird zusätzlich der av_log-Callback auf eine tracing-Bridge
//! gesetzt (ChiakiLog-Mapping gemäß CONVENTIONS: DEBUG→debug, VERBOSE→trace).
//!
//! Diese Bindings pinnen die ABI-Majors 59/61/8 (Struct-Felder sind transkribiert,
//! siehe `sys`). Lädt man andere Majors, bricht `init` mit Fehler ab.

pub mod sys;

use std::ffi::{c_char, CStr};
use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use chiaki_core::{ChiakiError, ChiakiResult};
use libloading::Library;
use thiserror::Error;

use sys::Api;

/// Erwartete DLL-Majors (FFmpeg n7.1) — die Struct-Layouts in `sys` sind daran gepinnt.
const EXPECTED_AVUTIL_MAJOR: u32 = 59;
const EXPECTED_AVCODEC_MAJOR: u32 = 61;
const EXPECTED_SWSCALE_MAJOR: u32 = 8;

const DLL_AVUTIL: &str = "avutil-59.dll";
const DLL_AVCODEC: &str = "avcodec-61.dll";
const DLL_SWSCALE: &str = "swscale-8.dll";

#[derive(Debug, Error)]
pub(crate) enum LoadError {
    #[error("DLL `{dll}` not found (tried: {tried})")]
    NotFound { dll: String, tried: String },
    #[error("symbol lookup failed: {0}: {1}")]
    Symbol(String, #[source] libloading::Error),
    #[error("version mismatch: {dll} reports major {found}, expected {expected}")]
    Version {
        dll: String,
        found: u32,
        expected: u32,
    },
}

impl From<LoadError> for ChiakiError {
    fn from(e: LoadError) -> Self {
        tracing::error!("FFmpeg load failed: {e}");
        match e {
            LoadError::NotFound { .. } => ChiakiError::Uninitialized,
            LoadError::Symbol(..) => ChiakiError::Unknown,
            LoadError::Version { .. } => ChiakiError::VersionMismatch,
        }
    }
}

/// Geladene FFmpeg-Bibliotheken (Handles + aufgelöste Funktionen).
pub struct FfmpegLib {
    /// Reihenfolge beachten: avutil zuerst (avcodec hängt davon ab), swscale zuletzt.
    _avutil: Library,
    _avcodec: Library,
    _swscale: Library,
    api: Api,
}

impl FfmpegLib {
    /// Aufgelöste FFI-Funktionen. `init()` muss vorher gelaufen sein.
    pub fn api(&self) -> &Api {
        &self.api
    }
}

static FFMPEG: OnceLock<FfmpegLib> = OnceLock::new();
static LOG_BRIDGE_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Lädt die FFmpeg-DLLs (einmalig pro Prozess) und installiert die av_log→tracing-
/// Bridge. Bei jedem weiteren Aufruf wird die bestehende Instanz zurückgegeben.
pub fn init() -> ChiakiResult<&'static FfmpegLib> {
    if let Some(lib) = FFMPEG.get() {
        return Ok(lib);
    }
    let lib = load()?;
    match FFMPEG.set(lib) {
        Ok(()) => {}
        Err(existing) => {
            // Doppel-Init (Race) — die zuerst Eingesetzte gewinnt; Library-Drop ist ok.
            drop(existing);
        }
    }
    install_log_bridge();
    Ok(FFMPEG.get().expect("just set"))
}

fn load() -> Result<FfmpegLib, LoadError> {
    let dirs = candidate_dirs();
    let mut tried: Vec<String> = Vec::new();
    let mut last_err: Option<LoadError> = None;

    for dir in &dirs {
        let result = load_from_dir(dir);
        match result {
            Ok(lib) => {
                tracing::info!(
                    "FFmpeg loaded from {}: {} + {} + {}",
                    dir.display(),
                    DLL_AVUTIL,
                    DLL_AVCODEC,
                    DLL_SWSCALE
                );
                return Ok(lib);
            }
            Err(LoadError::NotFound { .. }) => {
                // Ordner hat nicht alle DLLs — nächster Kandidat.
                tried.push(dir.display().to_string());
            }
            Err(e) => {
                // DLL gefunden, aber kaputt/zu neu — Fehler merken, weitersuchen.
                tracing::warn!("FFmpeg candidate {} failed: {e}", dir.display());
                tried.push(dir.display().to_string());
                last_err = Some(e);
            }
        }
    }

    // 4. Fallback: nackte Namen → Windows-Standard-Suche (inkl. PATH).
    match load_bare() {
        Ok(lib) => {
            tracing::info!("FFmpeg loaded via system DLL search path (PATH)");
            return Ok(lib);
        }
        Err(LoadError::NotFound { .. }) => tried.push("<PATH>".to_string()),
        Err(e) => {
            tracing::warn!("FFmpeg from PATH failed: {e}");
            last_err = Some(e);
        }
    }

    let _ = last_err; // NotFound ist die informativere Meldung (enthält alle Pfade)
    Err(LoadError::NotFound {
        dll: format!("{DLL_AVUTIL}/{DLL_AVCODEC}/{DLL_SWSCALE}"),
        tried: tried.join(", "),
    })
}

/// Kandidaten-Ordner in der vorgeschriebenen Reihenfolge.
fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.to_path_buf());
            dirs.push(parent.join("ffmpeg").join("bin"));
        }
    }
    if let Some(v) = std::env::var_os("CHIAKI_FFMPEG_DIR") {
        if !v.is_empty() {
            dirs.push(PathBuf::from(v));
        }
    }
    dirs
}

fn load_from_dir(dir: &std::path::Path) -> Result<FfmpegLib, LoadError> {
    // LOAD_WITH_ALTERED_SEARCH_PATH: Dependent-DLLs (avcodec braucht avutil etc.)
    // werden relativ zum DLL-Ordner gesucht, nicht zum exe-Ordner — sonst lädt
    // avcodec-61.dll aus dem Referenz-Ordner nicht, wenn der exe-Ordner sie
    // nicht auch enthält.
    const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

    let try_load = |name: &str| -> Result<Library, LoadError> {
        unsafe {
            // SAFETY: Handle bleibt für die Prozess-Laufzeit offen (kein unload —
            // FFmpeg hält globale Zustände wie den Log-Callback).
            libloading::os::windows::Library::load_with_flags(
                dir.join(name),
                LOAD_WITH_ALTERED_SEARCH_PATH,
            )
            .map(libloading::Library::from)
            .map_err(|_| LoadError::NotFound {
                dll: name.into(),
                tried: dir.display().to_string(),
            })
        }
    };

    // Reihenfolge beachten: avutil zuerst (avcodec hängt davon ab).
    let avutil = try_load(DLL_AVUTIL)?;
    let avcodec = try_load(DLL_AVCODEC)?;
    let swscale = try_load(DLL_SWSCALE)?;
    let api = resolve(&avutil, &avcodec, &swscale)?;
    check_versions(&api)?;
    Ok(FfmpegLib {
        _avutil: avutil,
        _avcodec: avcodec,
        _swscale: swscale,
        api,
    })
}

fn load_bare() -> Result<FfmpegLib, LoadError> {
    unsafe {
        let not_found = |d: &str| LoadError::NotFound {
            dll: d.into(),
            tried: "PATH".into(),
        };
        let avutil = Library::new(DLL_AVUTIL).map_err(|_| not_found(DLL_AVUTIL))?;
        let avcodec = Library::new(DLL_AVCODEC).map_err(|_| not_found(DLL_AVCODEC))?;
        let swscale = Library::new(DLL_SWSCALE).map_err(|_| not_found(DLL_SWSCALE))?;
        let api = resolve(&avutil, &avcodec, &swscale)?;
        check_versions(&api)?;
        Ok(FfmpegLib {
            _avutil: avutil,
            _avcodec: avcodec,
            _swscale: swscale,
            api,
        })
    }
}

macro_rules! sym {
    ($lib:expr, $name:literal as $ty:ty) => {
        *$lib
            .get::<$ty>(concat!($name, "\0").as_bytes())
            .map_err(|e| LoadError::Symbol($name.to_string(), e))?
    };
}

fn resolve(avutil: &Library, avcodec: &Library, swscale: &Library) -> Result<Api, LoadError> {
    unsafe {
        Ok(Api {
            // avcodec — inkl. Packet-API (seit FFmpeg 5.0 wieder in libavcodec!)
            avcodec_version: sym!(avcodec, "avcodec_version" as sys::FnAvcodecVersion),
            avcodec_find_decoder: sym!(
                avcodec,
                "avcodec_find_decoder" as sys::FnAvcodecFindDecoder
            ),
            avcodec_alloc_context3: sym!(
                avcodec,
                "avcodec_alloc_context3" as sys::FnAvcodecAllocContext3
            ),
            avcodec_free_context: sym!(
                avcodec,
                "avcodec_free_context" as sys::FnAvcodecFreeContext
            ),
            avcodec_open2: sym!(avcodec, "avcodec_open2" as sys::FnAvcodecOpen2),
            avcodec_send_packet: sym!(avcodec, "avcodec_send_packet" as sys::FnAvcodecSendPacket),
            avcodec_receive_frame: sym!(
                avcodec,
                "avcodec_receive_frame" as sys::FnAvcodecReceiveFrame
            ),
            avcodec_get_hw_config: sym!(
                avcodec,
                "avcodec_get_hw_config" as sys::FnAvcodecGetHwConfig
            ),
            av_packet_alloc: sym!(avcodec, "av_packet_alloc" as sys::FnAvPacketAlloc),
            av_packet_free: sym!(avcodec, "av_packet_free" as sys::FnAvPacketFree),
            // avutil
            avutil_version: sym!(avutil, "avutil_version" as sys::FnAvutilVersion),
            av_log_set_callback: sym!(avutil, "av_log_set_callback" as sys::FnAvLogSetCallback),
            av_log_set_level: sym!(avutil, "av_log_set_level" as sys::FnAvLogSetLevel),
            av_log_format_line: sym!(avutil, "av_log_format_line" as sys::FnAvLogFormatLine),
            av_hwdevice_find_type_by_name: sym!(
                avutil,
                "av_hwdevice_find_type_by_name" as sys::FnAvHwdeviceFindTypeByName
            ),
            av_hwdevice_ctx_create: sym!(
                avutil,
                "av_hwdevice_ctx_create" as sys::FnAvHwdeviceCtxCreate
            ),
            av_hwdevice_ctx_alloc: sym!(
                avutil,
                "av_hwdevice_ctx_alloc" as sys::FnAvHwdeviceCtxAlloc
            ),
            av_hwdevice_ctx_init: sym!(
                avutil,
                "av_hwdevice_ctx_init" as sys::FnAvHwdeviceCtxInit
            ),
            av_get_pix_fmt: sym!(avutil, "av_get_pix_fmt" as sys::FnAvGetPixFmt),
            av_buffer_ref: sym!(avutil, "av_buffer_ref" as sys::FnAvBufferRefFn),
            av_buffer_unref: sym!(avutil, "av_buffer_unref" as sys::FnAvBufferUnref),
            av_buffer_create: sym!(avutil, "av_buffer_create" as sys::FnAvBufferCreate),
            av_malloc: sym!(avutil, "av_malloc" as sys::FnAvMalloc),
            av_free: sym!(avutil, "av_free" as sys::FnAvFree),
            av_frame_alloc: sym!(avutil, "av_frame_alloc" as sys::FnAvFrameAlloc),
            av_frame_free: sym!(avutil, "av_frame_free" as sys::FnAvFrameFree),
            av_frame_unref: sym!(avutil, "av_frame_unref" as sys::FnAvFrameUnref),
            av_frame_get_buffer: sym!(avutil, "av_frame_get_buffer" as sys::FnAvFrameGetBuffer),
            av_frame_copy_props: sym!(avutil, "av_frame_copy_props" as sys::FnAvFrameCopyProps),
            av_hwframe_transfer_data: sym!(
                avutil,
                "av_hwframe_transfer_data" as sys::FnAvHwframeTransferData
            ),
            av_get_pix_fmt_name: sym!(avutil, "av_get_pix_fmt_name" as sys::FnAvGetPixFmtName),
            av_strerror: sym!(avutil, "av_strerror" as sys::FnAvStrerror),
            // swscale
            swscale_version: sym!(swscale, "swscale_version" as sys::FnSwsVersion),
            sws_get_context: sym!(swscale, "sws_getContext" as sys::FnSwsGetContext),
            sws_scale: sym!(swscale, "sws_scale" as sys::FnSwsScale),
            sws_free_context: sym!(swscale, "sws_freeContext" as sys::FnSwsFreeContext),
        })
    }
}

fn check_versions(api: &Api) -> Result<(), LoadError> {
    let major = |v: u32| v >> 16;
    let avutil_major = major(unsafe { (api.avutil_version)() });
    if avutil_major != EXPECTED_AVUTIL_MAJOR {
        return Err(LoadError::Version {
            dll: DLL_AVUTIL.into(),
            found: avutil_major,
            expected: EXPECTED_AVUTIL_MAJOR,
        });
    }
    let avcodec_major = major(unsafe { (api.avcodec_version)() });
    if avcodec_major != EXPECTED_AVCODEC_MAJOR {
        return Err(LoadError::Version {
            dll: DLL_AVCODEC.into(),
            found: avcodec_major,
            expected: EXPECTED_AVCODEC_MAJOR,
        });
    }
    let swscale_major = major(unsafe { (api.swscale_version)() });
    if swscale_major != EXPECTED_SWSCALE_MAJOR {
        return Err(LoadError::Version {
            dll: DLL_SWSCALE.into(),
            found: swscale_major,
            expected: EXPECTED_SWSCALE_MAJOR,
        });
    }
    Ok(())
}

/// av_strerror auf der C-Seite rufen (Meldungstexte wie im C-Client).
pub(crate) fn err_str(api: &Api, code: c_int) -> String {
    let mut buf = [0u8; 128];
    unsafe {
        // SAFETY: buf lebt über den Aufruf, Größe wird mitgegeben.
        (api.av_strerror)(code, buf.as_mut_ptr() as *mut c_char, buf.len());
        CStr::from_ptr(buf.as_ptr() as *const c_char)
            .to_string_lossy()
            .into_owned()
    }
}

pub(crate) fn pix_fmt_name(api: &Api, pix_fmt: c_int) -> String {
    unsafe {
        let p = (api.av_get_pix_fmt_name)(pix_fmt);
        if p.is_null() {
            format!("<unknown pix_fmt {pix_fmt}>")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

/// av_log → tracing. Einmal pro Prozess installiert, danach für die Laufzeit aktiv.
///
/// Die Varargs werden NICHT in Rust formatiert (geht nicht portabel), sondern auf
/// der C-Seite per `av_log_format_line` in einen Puffer gerendert — exakt der
/// Zweck dieser Funktion.
unsafe extern "system" fn av_log_bridge(
    avcl: *mut c_void,
    level: c_int,
    fmt: *const c_char,
    vl: sys::VaList,
) {
    let Some(lib) = FFMPEG.get() else { return };
    let mut line = [0u8; 1024];
    let mut print_prefix: c_int = 1;
    // SAFETY: line lebt über den Aufruf; vl stammt unverändert aus dem Callback.
    (lib.api().av_log_format_line)(
        avcl,
        level,
        fmt,
        vl,
        line.as_mut_ptr() as *mut c_char,
        line.len() as c_int,
        &mut print_prefix,
    );
    let msg = CStr::from_ptr(line.as_ptr() as *const c_char).to_string_lossy();
    let msg = msg.trim_end_matches(['\n', '\r']);
    // Mapping gemäß CONVENTIONS (ChiakiLog): ERROR→error, WARN→warn, INFO→info,
    // DEBUG→debug, VERBOSE→trace. FFmpeg PANIC/FATAL werden wie ERROR behandelt.
    match level {
        sys::AV_LOG_QUIET => {}
        l if l <= sys::AV_LOG_ERROR => tracing::error!("[ffmpeg] {msg}"),
        l if l <= sys::AV_LOG_WARNING => tracing::warn!("[ffmpeg] {msg}"),
        l if l <= sys::AV_LOG_INFO => tracing::info!("[ffmpeg] {msg}"),
        l if l <= sys::AV_LOG_VERBOSE => tracing::trace!("[ffmpeg] {msg}"),
        _ => tracing::debug!("[ffmpeg] {msg}"),
    }
}

fn install_log_bridge() {
    if LOG_BRIDGE_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(lib) = FFMPEG.get() {
        unsafe {
            // SAFETY: Callback ist 'static (bare fn) und wird nie wieder abgemeldet.
            (lib.api().av_log_set_callback)(Some(av_log_bridge));
            // Alles an tracing durchlassen — Filtern übernimmt das tracing-Subsystem.
            (lib.api().av_log_set_level)(sys::AV_LOG_TRACE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        crate::test_setup::reference_dlls();
    }

    #[test]
    fn loads_reference_dlls_and_versions_match() {
        setup();
        let lib = init().expect("FFmpeg DLLs müssen ladbar sein (Referenz-Binaries)");
        let api = lib.api();
        let avcodec = unsafe { (api.avcodec_version)() };
        assert_eq!(avcodec >> 16, EXPECTED_AVCODEC_MAJOR);
        let avutil = unsafe { (api.avutil_version)() };
        assert_eq!(avutil >> 16, EXPECTED_AVUTIL_MAJOR);
        let swscale = unsafe { (api.swscale_version)() };
        assert_eq!(swscale >> 16, EXPECTED_SWSCALE_MAJOR);
    }

    #[test]
    fn finds_h264_and_hevc_decoders() {
        setup();
        let lib = init().expect("FFmpeg init");
        let api = lib.api();
        unsafe {
            // SAFETY: reine Abfragen auf geladenen Symbolen.
            assert!(
                !(api.avcodec_find_decoder)(sys::AV_CODEC_ID_H264).is_null(),
                "H264 decoder must be available"
            );
            assert!(
                !(api.avcodec_find_decoder)(sys::AV_CODEC_ID_HEVC).is_null(),
                "HEVC decoder must be available"
            );
            assert!(
                (api.avcodec_find_decoder)(-9999).is_null(),
                "unknown codec id must yield NULL"
            );
        }
    }

    #[test]
    fn hw_configs_are_enumerable() {
        setup();
        let lib = init().expect("FFmpeg init");
        let api = lib.api();
        unsafe {
            let h264 = (api.avcodec_find_decoder)(sys::AV_CODEC_ID_H264);
            assert!(!h264.is_null());
            // Mindestens ein HW-Config-Eintrag mit HW_DEVICE_CTX-Methode muss
            // existieren (die gpl-shared-Builds haben cuda/d3d11va/vulkan).
            let mut any_hw = false;
            let mut i = 0;
            loop {
                let cfg = (api.avcodec_get_hw_config)(h264, i);
                if cfg.is_null() {
                    break;
                }
                if (*cfg).methods & sys::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX != 0 {
                    any_hw = true;
                }
                i += 1;
                assert!(i < 100, "hw config list must terminate");
            }
            assert!(any_hw, "H264 must expose at least one hw device config");
        }
    }
}
