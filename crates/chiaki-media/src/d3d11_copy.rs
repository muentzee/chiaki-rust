// SPDX-License-Identifier: AGPL-3.0-only
//! D3D11-NV12-Staging-Download für den Virtual-Cam-Feed.
//!
//! Im D3D11VA-Raw-Pfad ([`FrameMemory::D3d11Texture`]) lebt der dekodierte
//! Frame als NV12-Array-Textur auf dem GPU-Sink-Device. Der Kamera-Feed
//! braucht die Bytes im Systemspeicher: Staging-Textur (CPU-READ) +
//! `CopySubresourceRegion` beider Planes (Subresource = arrayIndex*2 + Plane,
//! siehe FFmpeg-D3D11VA-Gotcha) + `Map`/zeilenweises Kopieren.
//!
//! Der Immediate-Context wird parallel vom Sink-Render-Thread (Draw) und
//! FFmpeg (Decode) genutzt — das Device läuft mit `SetMultithreadProtected(1)`
//! (chiaki-render gpu_sink::sys, gleiches Argument wie beim Decode), D3D11
//! serialisiert die Aufrufe intern. Die Kopie läuft synchron im Media-Thread;
//! die Surface gilt bis zum nächsten Decode (DecodedFrame-Vertrag).

use std::os::raw::c_void;

use windows::core::Interface as _;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

/// Besitzer der Staging-Textur (Dimensionen des Dekoder-Outputs, einmalig
/// erzeugt und über die Session wiederverwendet). `Drop` released die vom
/// Sink übernommenen +1-Referenzen (GpuSinkHandle::d3d11_*_addref-Vertrag).
pub struct D3d11Nv12Downloader {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
}

unsafe impl Send for D3d11Nv12Downloader {}

impl D3d11Nv12Downloader {
    /// Übernimmt die +1-Referenzen des Sinks (Adoption ohne zusätzliches
    /// AddRef — die Pointer stammen von `d3d11_device_addref`/`…_context_addref`).
    pub fn new(
        device_ptr: *mut c_void,
        context_ptr: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        if device_ptr.is_null() || context_ptr.is_null() {
            return Err("D3D11-Download: Device/Context ist null".into());
        }
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!("D3D11-Download: ungültige Dimensionen {width}x{height}"));
        }
        let device = unsafe { ID3D11Device::from_raw(device_ptr as *mut _) };
        let context = unsafe { ID3D11DeviceContext::from_raw(context_ptr as *mut _) };
        // Staging-Textur (CPU-READ) einmalig — Dimensionen stehen beim
        // Session-Start fest (Dekoder-Output ändert sich nicht mid-stream).
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| format!("D3D11-Download: Staging-Textur fehlgeschlagen: {e}"))?;
        }
        staging
            .as_ref()
            .ok_or_else(|| "D3D11-Download: CreateTexture2D lieferte keine Textur".to_string())?;
        Ok(D3d11Nv12Downloader {
            _device: device,
            context,
            staging,
            width,
            height,
        })
    }

    fn staging_texture(&self) -> Result<&ID3D11Texture2D, String> {
        Ok(self.staging.as_ref().expect("Staging-Textur im Konstruktor erzeugt"))
    }

    /// Kopiert die NV12-Planes der Decoded-Textur in zwei Host-Buffer
    /// (`out_y` = w*h, `out_uv` = w*h/2, gepackt ohne Stride).
    ///
    /// # Safety
    /// `src_texture` muss eine gültige NV12-Array-Textur (Dekoder-Output,
    /// gleiches Device) sein; `subresource_base` = arrayIndex*2 (Y-Plane,
    /// UV = base+1). Die Surface bleibt bis zum nächsten Decode gültig —
    /// der Aufrufer kopiert synchron im Decode-Loop.
    pub unsafe fn download(
        &mut self,
        src_texture: *mut c_void,
        subresource_base: u32,
        out_y: &mut Vec<u8>,
        out_uv: &mut Vec<u8>,
    ) -> Result<(), String> {
        if src_texture.is_null() {
            return Err("D3D11-Download: Quelltextur ist null".into());
        }
        let staging = self.staging_texture()?;
        let src: &ID3D11Texture2D = &*(src_texture as *const ID3D11Texture2D);
        let (w, h) = (self.width as usize, self.height as usize);

        // Y-Plane (subresource base) und UV-Plane (base+1) auf Staging.
        self.context.CopySubresourceRegion(staging, 0, 0, 0, 0, src, subresource_base, None);
        self.context.CopySubresourceRegion(staging, 1, 0, 0, 0, src, subresource_base + 1, None);

        out_y.clear();
        out_y.resize(w * h, 0);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("D3D11-Download: Map(Y) fehlgeschlagen: {e}"))?;
        let src_rows = std::slice::from_raw_parts(mapped.pData as *const u8, mapped.RowPitch as usize * h);
        for row in 0..h {
            let start = row * mapped.RowPitch as usize;
            out_y[row * w..(row + 1) * w].copy_from_slice(&src_rows[start..start + w]);
        }
        self.context.Unmap(staging, 0);

        out_uv.clear();
        out_uv.resize(w * h / 2, 0);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(staging, 1, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| format!("D3D11-Download: Map(UV) fehlgeschlagen: {e}"))?;
        let src_rows =
            std::slice::from_raw_parts(mapped.pData as *const u8, mapped.RowPitch as usize * (h / 2));
        for row in 0..h / 2 {
            let start = row * mapped.RowPitch as usize;
            out_uv[row * w..(row + 1) * w].copy_from_slice(&src_rows[start..start + w]);
        }
        self.context.Unmap(staging, 1);
        Ok(())
    }
}

impl Drop for D3d11Nv12Downloader {
    fn drop(&mut self) {
        // Referenz-Vertrag: Device/Context sind die vom Sink addref'ten
        // +1-Referenzen — from_raw adoptiert sie, Drop released genau diese.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_null_fehler_sauber() {
        assert!(D3d11Nv12Downloader::new(std::ptr::null_mut(), std::ptr::null_mut(), 1280, 720)
            .is_err());
    }

    #[test]
    fn neue_ungerade_dimensionen_fehler() {
        assert!(D3d11Nv12Downloader::new(std::ptr::null_mut(), std::ptr::null_mut(), 1281, 720)
            .is_err());
    }
}
