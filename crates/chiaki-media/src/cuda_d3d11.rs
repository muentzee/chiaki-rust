// SPDX-License-Identifier: AGPL-3.0-only
//! CUDA↔D3D11-Interop über das NVCVImage-SDK (`NVCVImage.dll`) — schreibt den
//! GPU-residenten VSR-Output (RGBA, `NVCV_GPU`) direkt in eine D3D11-BGRA-
//! Textur des GPU-Sinks (chiaki-render). Kein CPU-Roundtrip.
//!
//! ## Warum SDK-Interop statt handgerolltem cuGraphicsD3D11RegisterResource
//! Das VFX-SDK bringt mit `NvCVImage_InitFromD3D11Texture` +
//! `NvCVImage_MapResource`/`NvCVImage_UnmapResource` eine fertige D3D11-Brücke
//! mit (nicht-implementiert im ersten Anlauf gemachte Kontext-Jonglier-Arbeit
//! entfällt). Der Versuch, die Brücke direkt über die Driver-API
//! (`cuGraphicsD3D11RegisterResource` + `cuMemcpy2D`) zu bauen, scheiterte
//! mit `CUDA_ERROR_INVALID_CONTEXT (201)` am Kopiervorgang — die SDK-Route
//! funktioniert (das SDK kennt seine Kontexte selbst, siehe auch vsr/mod.rs
//! Root Cause 2: „alle Kopien über NvCVImage_Transfer").
//!
//! ## Ablauf pro Frame
//! 1. `NvCVImage_InitFromD3D11Texture` einmalig (registriert die Textur als
//!    CUDA-Graphics-Resource; der Destruktor/Dealloc unregistriert).
//! 2. pro Frame (D3D11-Nutzung der Textur durch [`interop_lock`]
//!    ausgeschlossen — chiaki-render): `MapResource` → `NvCVImage_Transfer`
//!    (VSR-RGBA → D3D-Textur; RGBA-Bytes in BGRA-getypter Ressource, der
//!    Shader des Sinks macht den .bgr-Swizzle) → `UnmapResource`.
//!
//! Alle Aufrufe pushen den Decoder-CUDA-Kontext (derselbe Kontext, in dem
//! VSR seine Buffer alloziert hat — Root Cause 3 aus REWORK.md).

use std::os::raw::{c_int, c_void};
use std::ptr;

use libloading::Library;

use crate::vsr::sys::{
    FnCuCtxPopCurrent, FnCuCtxPushCurrent, FnCuStreamSynchronize, FnNvCVImageDealloc,
    FnNvCVImageTransfer, NvCVImage, NVCV_SUCCESS,
};

/// SDK-DLL (`NVCVImage.dll`), dynamisch geladen (Handles leaken wie im VSR-
/// Pfad — SDK hält globale Zustände).
struct SdkApi {
    _lib: Library,
    init_from_d3d11: unsafe extern "system" fn(im: *mut NvCVImage, tx: *mut c_void) -> c_int,
    map_resource: unsafe extern "system" fn(im: *mut NvCVImage, stream: *mut c_void) -> c_int,
    unmap_resource: unsafe extern "system" fn(im: *mut NvCVImage, stream: *mut c_void) -> c_int,
    transfer: FnNvCVImageTransfer,
    dealloc: FnNvCVImageDealloc,
    ctx_push_current: FnCuCtxPushCurrent,
    ctx_pop_current: FnCuCtxPopCurrent,
    stream_synchronize: FnCuStreamSynchronize,
}

fn load_sdk_api() -> Result<SdkApi, String> {
    unsafe {
        let lib: Library = Library::new("NVCVImage.dll")
            .map_err(|e| format!("NVCVImage.dll nicht ladbar: {e}"))?;
        let sym = |name: &'static str| -> Result<*mut c_void, String> {
            lib.get::<*mut c_void>(format!("{name}\0").as_bytes())
                .map(|f| *f)
                .map_err(|e| format!("NVCVImage.dll: {name} fehlt ({e})"))
        };
        // nvcuda für Kontext-Push/Pop (Treiber-DLL im System32).
        let cuda: Library = Library::new("nvcuda.dll")
            .map_err(|e| format!("nvcuda.dll nicht ladbar: {e}"))?;
        let csym = |name: &'static str| -> Result<*mut c_void, String> {
            cuda.get::<*mut c_void>(format!("{name}\0").as_bytes())
                .map(|f| *f)
                .map_err(|e| format!("nvcuda.dll: {name} fehlt ({e})"))
        };
        Ok(SdkApi {
            init_from_d3d11: std::mem::transmute::<*mut c_void, unsafe extern "system" fn(*mut NvCVImage, *mut c_void) -> c_int>(
                sym("NvCVImage_InitFromD3D11Texture")?,
            ),
            map_resource: std::mem::transmute::<*mut c_void, unsafe extern "system" fn(*mut NvCVImage, *mut c_void) -> c_int>(
                sym("NvCVImage_MapResource")?,
            ),
            unmap_resource: std::mem::transmute::<*mut c_void, unsafe extern "system" fn(*mut NvCVImage, *mut c_void) -> c_int>(
                sym("NvCVImage_UnmapResource")?,
            ),
            transfer: std::mem::transmute::<*mut c_void, FnNvCVImageTransfer>(
                sym("NvCVImage_Transfer")?,
            ),
            dealloc: std::mem::transmute::<*mut c_void, FnNvCVImageDealloc>(
                sym("NvCVImage_Dealloc")?,
            ),
            ctx_push_current: std::mem::transmute::<*mut c_void, FnCuCtxPushCurrent>(
                csym("cuCtxPushCurrent")?,
            ),
            ctx_pop_current: std::mem::transmute::<*mut c_void, FnCuCtxPopCurrent>(
                csym("cuCtxPopCurrent")?,
            ),
            stream_synchronize: std::mem::transmute::<*mut c_void, FnCuStreamSynchronize>(
                csym("cuStreamSynchronize")?,
            ),
            _lib: lib,
        })
    }
}

/// D3D11-BGRA-Textur als NVCV-Image (registriert im Decoder-CUDA-Kontext).
pub struct CudaD3d11Interop {
    api: SdkApi,
    cuda_ctx: *mut c_void,
    cuda_stream: *mut c_void,
    image: Box<NvCVImage>,
    /// Leerer Tmp-Buffer für die konvertierenden Transfers (SDK wächst ihn —
    /// gleicher Mechanismus wie in vsr/mod.rs, transferTmp).
    tmp: Box<NvCVImage>,
}

// SAFETY: Handles sind opaque; Nutzung ist über den interop_lock des Sinks
// serialisiert, Kontexte werden pro Aufruf gepusht.
unsafe impl Send for CudaD3d11Interop {}

impl CudaD3d11Interop {
    /// Initialisiert das Interop für `d3d11_texture` (ID3D11Texture2D*).
    /// `cuda_ctx`/`cuda_stream` stammen vom Decoder (wie bei VSR).
    pub fn new(
        cuda_ctx: *mut c_void,
        cuda_stream: *mut c_void,
        d3d11_texture: *mut c_void,
    ) -> Result<Self, String> {
        if cuda_ctx.is_null() || d3d11_texture.is_null() {
            return Err("Interop: cuda_ctx oder Textur ist null".into());
        }
        let api = load_sdk_api()?;
        unsafe {
            let rc = (api.ctx_push_current)(cuda_ctx);
            if rc != 0 {
                return Err(format!("Interop: cuCtxPushCurrent fehlgeschlagen (rc={rc})"));
            }
            let mut image = Box::new(NvCVImage::zeroed());
            let st = (api.init_from_d3d11)(&mut *image, d3d11_texture);
            let mut old: *mut c_void = ptr::null_mut();
            let _ = (api.ctx_pop_current)(&mut old);
            if st != NVCV_SUCCESS {
                return Err(format!(
                    "Interop: NvCVImage_InitFromD3D11Texture fehlgeschlagen (status {st})"
                ));
            }
            tracing::info!("CUDA-D3D11-Interop: BGRA-Textur registriert (SDK-Pfad)");
            Ok(CudaD3d11Interop {
                api,
                cuda_ctx,
                cuda_stream,
                image,
                tmp: Box::new(NvCVImage::zeroed()),
            })
        }
    }

    /// Kopiert den VSR-RGBA-Output (`src`, GPU-Image des Upscalers) in die
    /// D3D11-Textur. Map → Transfer → Unmap, alles im Decoder-Kontext.
    ///
    /// # Safety
    /// `src` muss ein gültiges NVCV_GPU-RGBA-Image aus demselben CUDA-Kontext
    /// sein (siehe [`crate::vsr::VsrUpscaler::gpu_rgba_image`]) und D3D11 die
    /// Zieltextur gerade nicht nutzen (interop_lock).
    pub unsafe fn write_from(&mut self, src: *const NvCVImage) -> Result<(), String> {
        let rc = (self.api.ctx_push_current)(self.cuda_ctx);
        if rc != 0 {
            return Err(format!("Interop: cuCtxPushCurrent fehlgeschlagen (rc={rc})"));
        }
        let result = self.write_from_locked(src);
        let mut old: *mut c_void = ptr::null_mut();
        let _ = (self.api.ctx_pop_current)(&mut old);
        result
    }

    unsafe fn write_from_locked(&mut self, src: *const NvCVImage) -> Result<(), String> {
        let st = (self.api.map_resource)(&mut *self.image, self.cuda_stream);
        if st != NVCV_SUCCESS {
            return Err(format!("Interop: MapResource fehlgeschlagen (status {st})"));
        }
        // tmp = NULL (NVIDIA-Sample-Konvention für GPU→GPU-Transfers; ein
    // leerer, nicht allozierter tmp-Image crasht stattdessen im SDK).
    let st = (self.api.transfer)(
            src,
            &mut *self.image,
            1.0,
            self.cuda_stream,
            ptr::null_mut(),
        );
        if st != NVCV_SUCCESS {
            let _ = (self.api.unmap_resource)(&mut *self.image, self.cuda_stream);
            return Err(format!("Interop: Transfer fehlgeschlagen (status {st})"));
        }
        let _ = (self.api.stream_synchronize)(self.cuda_stream);
        let st = (self.api.unmap_resource)(&mut *self.image, self.cuda_stream);
        if st != NVCV_SUCCESS {
            return Err(format!("Interop: UnmapResource fehlgeschlagen (status {st})"));
        }
        Ok(())
    }
}

impl Drop for CudaD3d11Interop {
    fn drop(&mut self) {
        unsafe {
            let rc = (self.api.ctx_push_current)(self.cuda_ctx);
            if rc == 0 {
                // Dealloc unregistriert die Graphics-Resource (SDK-Vertrag).
                (self.api.dealloc)(&mut *self.image);
                let mut old: *mut c_void = ptr::null_mut();
                let _ = (self.api.ctx_pop_current)(&mut old);
            } else {
                tracing::warn!("CUDA-D3D11-Interop: Drop ohne Kontext (rc={rc})");
            }
        }
    }
}
