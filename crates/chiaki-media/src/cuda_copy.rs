// SPDX-License-Identifier: AGPL-3.0-only
//! CUDA-Device→Host-Kopie für den Virtual-Cam-Feed (NV12, zwei Planes).
//!
//! Der Virtual-Cam-Feed hängt standardmäßig VOR VSR an die dekodierten
//! Frames (Stream-Auflösung). Im CUDA-Raw-Pfad ([`FrameMemory::CudaDevice`])
//! leben die Planes als Device-Pointer — der Feed braucht sie im
//! Systemspeicher. Kopie über die Driver-API (`cuMemcpy2D`, synchron),
//! im Decoder-Kontext gepusht (gleiche Kontext-Disziplin wie VSR/Interop —
//! REWORK.md Root Cause 2: Kontextbindungsfehler 201 bei Kontext-Fremdnutzung).
//!
//! Vor dem Kopieren wird der Decoder-Stream synchronisiert (der NVDEC-Frame
//! ist erst nach dem Decode-Work auf dem Stream vollständig); `cuMemcpy2D`
//! blockiert selbst bis zum Ende der Kopie.

use std::os::raw::{c_int, c_void};
use std::ptr;

use libloading::Library;

use crate::vsr::sys::{FnCuCtxPopCurrent, FnCuCtxPushCurrent, FnCuStreamSynchronize};

/// CU_MEMORYTYPE_DEVICE / CU_MEMORYTYPE_HOST.
const CU_MEMORYTYPE_HOST: c_int = 0x01;
const CU_MEMORYTYPE_DEVICE: c_int = 0x02;

/// CUDA_MEMCPY2D (x64-Layout; srcHost/srcDevice/srcArray teilen sich einen
/// Pointer-Slot — wir nutzen nur Device-src/Host-dst).
#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: c_int,
    _pad_src: u32,
    src: *mut c_void,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: c_int,
    _pad_dst: u32,
    dst: *mut c_void,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

const _: () = assert!(std::mem::size_of::<CudaMemcpy2D>() == 96);

type FnCuMemcpy2D = unsafe extern "system" fn(copy: *const CudaMemcpy2D) -> c_int;

/// Geladene nvcuda.dll + die vier Driver-Funktionen. Das Library-Handle
/// leakt absichtlich (wie im VSR-Pfad — der Treiber hält globale Zustände).
pub struct CudaCopier {
    _cuda: Library,
    memcpy2d: FnCuMemcpy2D,
    ctx_push: FnCuCtxPushCurrent,
    ctx_pop: FnCuCtxPopCurrent,
    stream_sync: FnCuStreamSynchronize,
}

impl CudaCopier {
    /// Lädt nvcuda.dll (Treiber im System32; auf Nicht-NVIDIA-Systemen
    /// fehlschlagend — der Aufrufer fällt dann auf „kein Kamera-Feed“ zurück).
    pub fn new() -> Result<Self, String> {
        unsafe {
            let cuda: Library = Library::new("nvcuda.dll")
                .map_err(|e| format!("nvcuda.dll nicht ladbar: {e}"))?;
            let sym = |name: &'static str| -> Result<*mut c_void, String> {
                cuda.get::<*mut c_void>(format!("{name}\0").as_bytes())
                    .map(|f| *f)
                    .map_err(|e| format!("nvcuda.dll: {name} fehlt ({e})"))
            };
            Ok(CudaCopier {
                memcpy2d: std::mem::transmute::<*mut c_void, FnCuMemcpy2D>(sym("cuMemcpy2D")?),
                ctx_push: std::mem::transmute::<*mut c_void, FnCuCtxPushCurrent>(
                    sym("cuCtxPushCurrent")?,
                ),
                ctx_pop: std::mem::transmute::<*mut c_void, FnCuCtxPopCurrent>(
                    sym("cuCtxPopCurrent")?,
                ),
                stream_sync: std::mem::transmute::<*mut c_void, FnCuStreamSynchronize>(
                    sym("cuStreamSynchronize")?,
                ),
                _cuda: cuda,
            })
        }
    }

    /// Lädt einen NV12-Frame von Device-Planes in einen gepackten Host-Buffer
    /// (`out` wird auf `w*h*3/2` gebracht und wiederverwendet).
    ///
    /// # Safety
    /// `y_dev`/`uv_dev` müssen gültige CUDA-Device-Pointer aus `cuda_ctx`
    /// sein, deren Frame auf `cuda_stream` fertig dekodiert ist — und bis
    /// zum nächsten Decode gültig bleiben (DecodedFrame-Vertrag).
    pub unsafe fn download_nv12(
        &self,
        cuda_ctx: *mut c_void,
        cuda_stream: *mut c_void,
        y_dev: *const u8,
        y_stride: usize,
        uv_dev: *const u8,
        uv_stride: usize,
        w: u32,
        h: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        if cuda_ctx.is_null() || y_dev.is_null() || uv_dev.is_null() {
            return Err("CUDA-Download: Kontext oder Plane-Pointer ist null".into());
        }
        let (w, h) = (w as usize, h as usize);
        if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
            return Err(format!("CUDA-Download: ungültige Dimensionen {w}x{h}"));
        }
        let rc = (self.ctx_push)(cuda_ctx);
        if rc != 0 {
            return Err(format!("CUDA-Download: cuCtxPushCurrent fehlgeschlagen (rc={rc})"));
        }
        let result = self.download_nv12_in_ctx(cuda_stream, y_dev, y_stride, uv_dev, uv_stride, w, h, out);
        let mut old: *mut c_void = ptr::null_mut();
        let _ = (self.ctx_pop)(&mut old);
        result
    }

    unsafe fn download_nv12_in_ctx(
        &self,
        cuda_stream: *mut c_void,
        y_dev: *const u8,
        y_stride: usize,
        uv_dev: *const u8,
        uv_stride: usize,
        w: usize,
        h: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        // Decode-Work auf dem Decoder-Stream abschließen, bevor gelesen wird.
        let rc = (self.stream_sync)(cuda_stream);
        if rc != 0 {
            return Err(format!("CUDA-Download: cuStreamSynchronize fehlgeschlagen (rc={rc})"));
        }
        out.clear();
        out.resize(w * h * 3 / 2, 0);
        // Y-Plane
        let y = CudaMemcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            _pad_src: 0,
            src: y_dev as *mut c_void,
            src_pitch: y_stride,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_HOST,
            _pad_dst: 0,
            dst: out.as_mut_ptr() as *mut c_void,
            dst_pitch: w,
            width_in_bytes: w,
            height: h,
        };
        let rc = (self.memcpy2d)(&y);
        if rc != 0 {
            return Err(format!("CUDA-Download: Y-cuMemcpy2D fehlgeschlagen (rc={rc})"));
        }
        // UV-Plane (interleaved, halbe Zeilenzahl)
        let uv = CudaMemcpy2D {
            src_x_in_bytes: 0,
            src_y: 0,
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            _pad_src: 0,
            src: uv_dev as *mut c_void,
            src_pitch: uv_stride,
            dst_x_in_bytes: 0,
            dst_y: 0,
            dst_memory_type: CU_MEMORYTYPE_HOST,
            _pad_dst: 0,
            dst: out.as_mut_ptr().add(w * h) as *mut c_void,
            dst_pitch: w,
            width_in_bytes: w,
            height: h / 2,
        };
        let rc = (self.memcpy2d)(&uv);
        if rc != 0 {
            return Err(format!("CUDA-Download: UV-cuMemcpy2D fehlgeschlagen (rc={rc})"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memcpy2d_layout_passt_zu_cuda() {
        // x64: 2×(usize,usize,c_int+pad,ptr,usize) + 2×usize = 96 Bytes.
        assert_eq!(std::mem::size_of::<CudaMemcpy2D>(), 96);
        assert_eq!(std::mem::size_of::<usize>(), 8);
    }

    #[test]
    fn copier_ohne_nvidia_liefert_fehler() {
        // Auf Maschinen ohne nvcuda.dll (CI) muss der Loader sauber fehlschlagen;
        // auf der Dev-Maschine (NVIDIA) lädt er — beides kein Panic.
        match CudaCopier::new() {
            Ok(_) => {}
            Err(err) => assert!(err.contains("nvcuda"), "{err}"),
        }
    }
}
