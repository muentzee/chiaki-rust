// SPDX-License-Identifier: AGPL-3.0-only
//! Win32/D3D11-FFI für den GPU-Video-Sink ([`super`]) — sämtliche `unsafe`-
//! Aufrufe des Moduls sind hier gekapselt (CONVENTIONS: FFI in `sys`-Modul).
//!
//! Bewusst schlank: Die windows-rs-COM-Wrapper besitzt der Render-Thread;
//! nach außen gehen nur rohe `*mut c_void`-Pointer (ID3D11Device*,
//! ID3D11DeviceContext*, ID3D11Texture2D*). Konsumenten:
//! * FFmpeg-D3D11VA (chiaki-media): teilt Device+Context (MT-protected) und
//!   dekodiert direkt auf diesem Device — CopySubresourceRegion läuft dann
//!   geräteintern ohne CPU-Roundtrip.
//! * CUDA-Interop (chiaki-media): registriert die BGRA-Textur
//!   (cuGraphicsD3D11RegisterResource) und schreibt VSR-Output hinein.

use std::os::raw::c_void;

use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{
    BOOL, COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11ClassLinkage, ID3D11DepthStencilView, ID3D11Device,
    ID3D11DeviceContext, ID3D11Multithread, ID3D11PixelShader, ID3D11RasterizerState,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_COMPARISON_ALWAYS, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CULL_NONE, D3D11_FILL_SOLID,
    D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_RASTERIZER_DESC, D3D11_SAMPLER_DESC,
    D3D11_SDK_VERSION, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP,
    D3D11_USAGE_DEFAULT, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter, IDXGIFactory2, IDXGIOutput, IDXGISwapChain1,
    DXGI_CREATE_FACTORY_FLAGS, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT, DXGI_SCALING_NONE,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW,
    GetClientRect, IsWindow, PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW,
    CS_VREDRAW, HMENU, HWND_BOTTOM, LWA_COLORKEY, MSG, PM_REMOVE, SET_WINDOW_POS_FLAGS,
    SHOW_WINDOW_CMD, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
    WINDOW_STYLE, WM_APP, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// Fehler-Typ der sys-Schicht (win32/d3d11 — HRESULTs bzw. Meldungen).
#[derive(Debug, thiserror::Error)]
pub enum SysError {
    #[error("win32/d3d11 call failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("shader compile failed: {0}")]
    ShaderCompile(String),
    #[error("{0}")]
    Message(String),
}

pub type SysResult<T> = Result<T, SysError>;

// ---------------------------------------------------------------------------
// Fenster
// ---------------------------------------------------------------------------

/// `WM_APP`-Nachricht: ein neuer Videoframe liegt in der Mailbox.
pub const WM_APP_FRAME: u32 = WM_APP + 1;
/// `WM_APP`-Nachricht: Sink-Stop gewünscht (Owner-Drop).
pub const WM_APP_STOP: u32 = WM_APP + 2;

/// Fensterklasse des Video-Sinks (prozessweit einmalig registriert).
const CLASS_NAME: &str = "ChiakiGpuVideoSink";

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_APP_STOP {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Registriert die Fensterklasse (idempotent — zweiter Aufruf ist ok).
pub fn register_class() -> SysResult<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let name: windows::core::HSTRING = CLASS_NAME.into();
        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&class) == 0 {
            let err = windows::core::Error::from_win32();
            const ERROR_CLASS_ALREADY_EXISTS: i32 = 1410; // 0x582 (Win32-Fehlercode)
            if err.code().0 & 0xFFFF != ERROR_CLASS_ALREADY_EXISTS {
                return Err(err.into());
            }
        }
        Ok(())
    }
}

/// Borderless, nicht aktivierbares Top-Level-Fenster (Video-Sink).
pub fn create_window(w: u32, h: u32) -> SysResult<HWND> {
    register_class()?;
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let name: windows::core::HSTRING = CLASS_NAME.into();
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            PCWSTR(name.as_ptr()),
            windows::core::w!("chiaki-gpu-video"),
            WINDOW_STYLE(WS_POPUP.0),
            0,
            0,
            w as i32,
            h as i32,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(hinstance.0),
            None,
        )?;
        let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(SW_SHOWNOACTIVATE.0));
        Ok(hwnd)
    }
}

/// HWND eines Fensters anhand des exakten Titels (das gpui-Overlay-Fenster).
pub fn find_window_by_title(title: &str) -> Option<HWND> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())).ok() }
}

/// Lebt das Fensterhandle noch? (`IsWindow`) — Guard für alle Follow-Tick-
/// Fensteraufrufe gegen Teardown-Races: Overlay- und Sink-Fenster können
/// zwischen `FindWindowW` und dem eigentlichen Aufruf verschwinden; auf
/// toten HWNDs liefern SetWindowPos/ShowWindow/GetClientRect sonst
/// ERROR_INVALID_WINDOW_HANDLE (0x80070578) bzw. harte Fehler.
pub fn valid(hwnd: HWND) -> bool {
    unsafe { IsWindow(hwnd).as_bool() }
}

/// Client-Bereich eines Fensters in Bildschirmkoordinaten (physische Pixel).
/// `None`, wenn das Handle schon tot ist (Teardown-Race).
pub fn client_rect_screen(hwnd: HWND) -> Option<(i32, i32, u32, u32)> {
    if !valid(hwnd) {
        return None;
    }
    unsafe {
        let mut rc = RECT::default();
        GetClientRect(hwnd, &mut rc).ok()?;
        let mut origin = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd, &mut origin);
        Some((
            origin.x,
            origin.y,
            (rc.right - rc.left).max(0) as u32,
            (rc.bottom - rc.top).max(0) as u32,
        ))
    }
}

/// Positioniert `hwnd` an (x, y, w, h) und direkt UNTERHALB von `anchor`
/// (gpui-Overlay-Fenster) in der Z-Ordnung (hAnchor precedes in Z-Order).
/// No-Op, wenn eins der beiden Handles schon tot ist (Teardown-Race).
///
/// # Safety
/// `hwnd`/`anchor` müssen Fensterhandles sein (werden auf Gültigkeit geprüft).
pub unsafe fn set_pos_below(hwnd: HWND, anchor: HWND, x: i32, y: i32, w: u32, h: u32) {
    if !valid(hwnd) || !valid(anchor) {
        return;
    }
    let _ = SetWindowPos(hwnd, anchor, x, y, w as i32, h as i32, SWP_NOACTIVATE);
}

/// Versteckt das Fenster (Overlay-Fenster verschwunden). No-Op auf totem
/// Handle (Teardown-Race).
///
/// # Safety
/// `hwnd` muss zum Sink-Fenster gehören (wird auf Gültigkeit geprüft).
pub unsafe fn hide_window(hwnd: HWND) {
    if !valid(hwnd) {
        return;
    }
    let flags: SET_WINDOW_POS_FLAGS = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
    let _ = SetWindowPos(hwnd, HWND_BOTTOM, 0, 0, 0, 0, flags);
    let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(SW_HIDE.0));
}

/// Zeigt das Fenster wieder (ohne Fokus zu klauen). No-Op auf totem Handle
/// (Teardown-Race).
///
/// # Safety
/// `hwnd` muss zum Sink-Fenster gehören (wird auf Gültigkeit geprüft).
pub unsafe fn show_window_no_activate(hwnd: HWND) {
    if !valid(hwnd) {
        return;
    }
    let _ = ShowWindow(hwnd, SHOW_WINDOW_CMD(SW_SHOWNOACTIVATE.0));
}

/// Nächste Nachricht abholen (blockierend im Render-Thread). `false` = WM_QUIT.
pub fn get_message(msg: &mut MSG) -> bool {
    unsafe { GetMessageW(msg, HWND::default(), 0, 0).as_bool() }
}

pub fn translate_and_dispatch(msg: &MSG) {
    unsafe {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
    }
}

/// Frame-Wake-Nachricht posten (thread-safe; weckt den Message-Loop sofort).
///
/// # Safety
/// `hwnd` muss zum lebenden Sink-Fenster gehören.
pub unsafe fn post_frame(hwnd: HWND) {
    let _ = PostMessageW(hwnd, WM_APP_FRAME, WPARAM(0), LPARAM(0));
}

/// Entfernt alle bereits queueden WM_APP_FRAME-Nachrichten des Sink-Fensters
/// und liefert die Anzahl (Burst-Collapse: bei ruckartig eintreffenden
/// Frames presentet der Render-Thread nur den NEUESTEN Zustand statt jeden
/// Ankunftszeitpunkt einzeln — gleiche Wirkung wie die C++-Render-Loop, die
/// pro Display-Tick einmal zeichnet).
///
/// # Safety
/// `hwnd` muss zum lebenden Sink-Fenster gehören.
pub unsafe fn drain_frame_messages(hwnd: HWND) -> u32 {
    let mut collapsed = 0;
    let mut msg = MSG::default();
    while PeekMessageW(
        &mut msg,
        hwnd,
        WM_APP_FRAME,
        WM_APP_FRAME,
        PM_REMOVE,
    ).as_bool() {
        collapsed += 1;
    }
    collapsed
}

/// Stop-Nachricht posten (Owner-Drop → Render-Thread beendet sich).
///
/// # Safety
/// `hwnd` muss zum lebenden Sink-Fenster gehören.
pub unsafe fn post_stop(hwnd: HWND) {
    let _ = PostMessageW(hwnd, WM_APP_STOP, WPARAM(0), LPARAM(0));
}

/// Follow-Tick-Timer (WM_TIMER weckt den Message-Loop ohne Frames).
///
/// # Safety
/// `hwnd` muss zum lebenden Sink-Fenster gehören.
pub unsafe fn set_timer(hwnd: HWND, id: usize, ms: u32) {
    SetTimer(hwnd, id, ms, None);
}

/// LWA_COLORKEY auf ein Fenster legen — dokumentierter FALLBACK, falls die
/// gpui-DComp-Transparenz auf einer Maschine nicht greift (siehe Modul-Doku).
/// `rgb` = COLORREF (0x00BBGGRR). Benötigt WS_EX_LAYERED auf dem Fenster.
///
/// # Safety
/// `hwnd` muss gültig sein.
pub unsafe fn set_color_key(hwnd: HWND, rgb: u32) -> bool {
    SetLayeredWindowAttributes(hwnd, COLORREF(rgb), 0, LWA_COLORKEY).is_ok()
}

// ---------------------------------------------------------------------------
// D3D11
// ---------------------------------------------------------------------------

/// Besitzende D3D11-Objekte des Render-Threads.
pub struct D3d11 {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub swap_chain: IDXGISwapChain1,
    pub render_target: Option<ID3D11RenderTargetView>,
    pub rasterizer: ID3D11RasterizerState,
    pub width: u32,
    pub height: u32,
}

/// Device + FLIP_DISCARD-Swapchain für das Sink-Fenster (BGRA8; Present mit
/// SyncInterval 0 — wie der C++-Client „vsync aus" und wie gpui presentet).
pub fn create_device_and_swapchain(hwnd: HWND, width: u32, height: u32) -> SysResult<D3d11> {
    unsafe {
        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut feature_level = D3D_FEATURE_LEVEL(0);
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )?;
        let device: ID3D11Device =
            device.ok_or_else(|| SysError::Message("D3D11CreateDevice: no device".into()))?;
        let context: ID3D11DeviceContext = context
            .ok_or_else(|| SysError::Message("D3D11CreateDevice: no context".into()))?;

        // Threadsicherheit: FFmpeg (Media-Thread) nutzt denselben Immediate-
        // Context für D3D11VA-Decode — MT-Protect serialisiert die Aufrufe.
        let mt: ID3D11Multithread = context.cast()?;
        let _ = mt.SetMultithreadProtected(BOOL(1));

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        let swap_chain = factory.CreateSwapChainForHwnd(
            &device,
            hwnd,
            &desc,
            None,
            None::<&IDXGIOutput>,
        )?;
        let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);

        let rasterizer_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: BOOL(1),
            ..Default::default()
        };
        let mut rasterizer: Option<ID3D11RasterizerState> = None;
        device.CreateRasterizerState(&rasterizer_desc, Some(&mut rasterizer))?;
        let rasterizer = rasterizer
            .ok_or_else(|| SysError::Message("CreateRasterizerState: null".into()))?;

        let render_target = create_backbuffer_view(&device, &swap_chain)?;
        Ok(D3d11 {
            device,
            context,
            swap_chain,
            render_target,
            rasterizer,
            width,
            height,
        })
    }
}

fn create_backbuffer_view(
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain1,
) -> SysResult<Option<ID3D11RenderTargetView>> {
    unsafe {
        let back: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;
        Ok(rtv)
    }
}

/// Swapchain nach Fenstergrößenänderung neu dimensionieren (+RTV neu bauen).
/// Muss vom Render-Thread laufen (besitzt die Objekte).
pub fn resize_swap_chain(d3d: &mut D3d11, width: u32, height: u32) -> SysResult<()> {
    if width == 0 || height == 0 {
        return Ok(());
    }
    unsafe {
        // Alle Backbuffer-Referenzen lösen (ResizeBuffers-Regel), dann resizen.
        d3d.render_target = None;
        d3d.context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        d3d.swap_chain.ResizeBuffers(
            0,
            width,
            height,
            DXGI_FORMAT_UNKNOWN,
            DXGI_SWAP_CHAIN_FLAG(0),
        )?;
        d3d.render_target = create_backbuffer_view(&d3d.device, &d3d.swap_chain)?;
        d3d.width = width;
        d3d.height = height;
    }
    Ok(())
}

/// NV12-Shader-Input-Textur (Y-Plane = Subresource 0, UV-Plane = Subresource 1).
pub struct Nv12Texture {
    pub texture: ID3D11Texture2D,
    pub srv_y: ID3D11ShaderResourceView,
    pub srv_uv: ID3D11ShaderResourceView,
}

pub fn create_nv12_texture(device: &ID3D11Device, width: u32, height: u32) -> SysResult<Nv12Texture> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let texture = unsafe {
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        tex.ok_or_else(|| SysError::Message("CreateTexture2D(NV12): null".into()))?
    };
    // Planare Formate: Plane-SRVs über TEXTURE2D-Views — das Format wählt die
    // Plane (D3D11.1 „plane SRVs"): R8_UNORM → Y-Plane, R8G8_UNORM → UV-Plane.
    // (Die TEXTURE2DARRAY-Variante mit FirstArraySlice = Plane wurde von der
    // RTX-4090-Treiberkette mit E_INVALIDARG abgelehnt — probe_d3d-Diagnose.)
    let srv_for = |fmt: DXGI_FORMAT| -> SysResult<ID3D11ShaderResourceView> {
        let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: fmt,
            ViewDimension: windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: windows::Win32::Graphics::Direct3D11::D3D11_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                },
            },
        };
        let mut srv: Option<ID3D11ShaderResourceView> = None;
        unsafe { device.CreateShaderResourceView(&texture, Some(&srv_desc), Some(&mut srv))? };
        Ok(srv.ok_or_else(|| SysError::Message("CreateShaderResourceView(NV12): null".into()))?)
    };
    let srv_y = srv_for(DXGI_FORMAT_R8_UNORM)?;
    let srv_uv = srv_for(DXGI_FORMAT_R8G8_UNORM)?;
    Ok(Nv12Texture { texture, srv_y, srv_uv })
}

/// RGBA8-Shader-Input-Textur (DXGI_FORMAT_R8G8B8A8_UNORM) — Ziel des CUDA-
/// Interops: der VSR-Output ist NVCV_RGBA (R,G,B,A-Bytes) und der SDK-Transfer
/// verlangt formatgleiche Images (RGBA→BGRA-Swizzle wird nicht bedient,
/// NVCV_ERR_PIXELFORMAT) — deshalb R8G8B8A8 und KEIN Swizzle im Shader.
pub struct RgbaTexture {
    pub texture: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,
}

pub fn create_rgba_texture(device: &ID3D11Device, width: u32, height: u32) -> SysResult<RgbaTexture> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        // BIND_RENDER_TARGET: erlaubt auch ClearRenderTargetView auf die
        // Interop-Textur (z. B. bei Source-Wechsel auf leer).
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let texture = unsafe {
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        tex.ok_or_else(|| SysError::Message("CreateTexture2D(RGBA): null".into()))?
    };
    let mut srv: Option<ID3D11ShaderResourceView> = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut srv))? };
    let srv = srv.ok_or_else(|| SysError::Message("CreateShaderResourceView(RGBA): null".into()))?;
    Ok(RgbaTexture { texture, srv })
}

/// Ein NV12-Frame in die NV12-Textur hochladen (UpdateSubresource, zwei Planes).
///
/// # Safety
/// `tex` muss vom `context`-Device stammen; die Slices müssen mindestens
/// stride*rows Bytes liefern (wird geprüft).
pub unsafe fn update_nv12(
    context: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
) -> SysResult<()> {
    let y_rows = height as usize;
    let uv_rows = height as usize / 2;
    if y.len() < y_stride * y_rows || uv.len() < uv_stride * uv_rows {
        return Err(SysError::Message(format!(
            "NV12 plane too small: y {} < {}, uv {} < {}",
            y.len(),
            y_stride * y_rows,
            uv.len(),
            uv_stride * uv_rows
        )));
    }
    let y_box = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: height,
        back: 1,
    };
    context.UpdateSubresource(tex, 0, Some(&y_box), y.as_ptr().cast(), y_stride as u32, 0);
    let uv_box = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: width,
        bottom: uv_rows as u32,
        back: 1,
    };
    context.UpdateSubresource(tex, 1, Some(&uv_box), uv.as_ptr().cast(), uv_stride as u32, 0);
    Ok(())
}

/// GPU-GPU-Kopie aus einer FFmpeg-D3D11VA-Textur-ARRAY-Quelle in unsere
/// NV12-Textur (kein CPU-Roundtrip). `array_index` = Textur-Array-Index des
/// Frames (FFmpeg: `frame->data[1] as isize`).
///
/// # Safety
/// `src`/`dst` müssen Texturen DESSELBEN Devices sein (Decoder teilt den
/// Sink-Device) und der Aufrufer muss den Context serialisiert nutzen
/// (Render-Thread oder MT-protected).
pub unsafe fn copy_d3d11_nv12(
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    array_index: u32,
    dst: &ID3D11Texture2D,
) {
    // Planar-Formate zählen jede Plane als eigenen Subresource:
    // subresource = arraySlice * planeCount + plane (planeCount = 2 für NV12).
    context.CopySubresourceRegion(dst, 0, 0, 0, 0, src, array_index * 2, None);
    context.CopySubresourceRegion(dst, 1, 0, 0, 0, src, array_index * 2 + 1, None);
}

// ---------------------------------------------------------------------------
// Shader (HLSL → D3DCompile; d3dcompiler_47 ist Bestandteil von Windows)
// ---------------------------------------------------------------------------

const VS_SRC: &str = r#"
struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

VSOut main(uint vid : SV_VertexID) {
    VSOut o;
    float2 uv = float2((vid << 1) & 2, vid & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}
"#;

/// NV12 → BGRA (BT.601 limited range — identisch zu chiaki-render::nv12:
/// R = 1.164*(Y-16) + 1.596*(V-128) usw.).
const PS_NV12_SRC: &str = r#"
Texture2D    yTex    : register(t0);
Texture2D    uvTex   : register(t1);
SamplerState linSamp : register(s0);

struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

float4 main(VSOut i) : SV_Target {
    float y = yTex.Sample(linSamp, i.uv).r;
    float2 uv = uvTex.Sample(linSamp, i.uv).rg;
    float u = uv.x - 0.5;
    float v = uv.y - 0.5;
    float c = 1.164383 * (y - 0.062745);
    float r = c + 1.596027 * v;
    float g = c - 0.391762 * u - 0.812968 * v;
    float b = c + 2.017234 * u;
    return float4(r, g, b, 1.0);
}
"#;

/// VSR-Interop-Pfad: R8G8B8A8-Textur mit NVCV_RGBA-Bytes — direktes Sampling,
/// kein Swizzle (siehe create_rgba_texture-Kommentar).
const PS_RGBA_PASSTHROUGH_SRC: &str = r#"
Texture2D    rgbaTex : register(t0);
SamplerState linSamp : register(s0);

struct VSOut {
    float4 pos : SV_POSITION;
    float2 uv  : TEXCOORD0;
};

float4 main(VSOut i) : SV_Target {
    float4 t = rgbaTex.Sample(linSamp, i.uv);
    return float4(t.r, t.g, t.b, 1.0);
}
"#;

pub struct Shaders {
    pub vs: ID3D11VertexShader,
    pub ps_nv12: ID3D11PixelShader,
    pub ps_bgra: ID3D11PixelShader,
    pub sampler: ID3D11SamplerState,
}

pub fn create_shaders(device: &ID3D11Device) -> SysResult<Shaders> {
    unsafe {
        let compile = |src: &str, target: &str| -> SysResult<ID3DBlob> {
            let mut blob: Option<ID3DBlob> = None;
            let mut err: Option<ID3DBlob> = None;
            let hr = D3DCompile(
                src.as_ptr().cast(),
                src.len(),
                PCSTR::null(),
                None,
                None::<&ID3DInclude>,
                PCSTR(b"main\0".as_ptr()),
                PCSTR(format!("{target}\0").as_ptr()),
                0,
                0,
                &mut blob,
                Some(&mut err),
            );
            if hr.is_err() {
                let msg = err
                    .map(|e| {
                        let ptr = e.GetBufferPointer() as *const u8;
                        let len = e.GetBufferSize();
                        String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
                    })
                    .unwrap_or_default();
                return Err(SysError::ShaderCompile(format!("{target}: {msg}")));
            }
            blob.ok_or_else(|| SysError::ShaderCompile(format!("{target}: no blob")))
        };

        let vs_blob = compile(VS_SRC, "vs_5_0")?;
        let mut vs: Option<ID3D11VertexShader> = None;
        device.CreateVertexShader(
            std::slice::from_raw_parts(
                vs_blob.GetBufferPointer() as *const u8,
                vs_blob.GetBufferSize(),
            ),
            None::<&ID3D11ClassLinkage>,
            Some(&mut vs),
        )?;
        let ps_nv12_blob = compile(PS_NV12_SRC, "ps_5_0")?;
        let mut ps_nv12: Option<ID3D11PixelShader> = None;
        device.CreatePixelShader(
            std::slice::from_raw_parts(
                ps_nv12_blob.GetBufferPointer() as *const u8,
                ps_nv12_blob.GetBufferSize(),
            ),
            None::<&ID3D11ClassLinkage>,
            Some(&mut ps_nv12),
        )?;
        let ps_bgra_blob = compile(PS_RGBA_PASSTHROUGH_SRC, "ps_5_0")?;
        let mut ps_bgra: Option<ID3D11PixelShader> = None;
        device.CreatePixelShader(
            std::slice::from_raw_parts(
                ps_bgra_blob.GetBufferPointer() as *const u8,
                ps_bgra_blob.GetBufferSize(),
            ),
            None::<&ID3D11ClassLinkage>,
            Some(&mut ps_bgra),
        )?;

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            MipLODBias: 0.0,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_ALWAYS,
            BorderColor: [0.0, 0.0, 0.0, 0.0],
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
        };
        let mut sampler: Option<ID3D11SamplerState> = None;
        device.CreateSamplerState(&sampler_desc, Some(&mut sampler))?;

        Ok(Shaders {
            vs: vs.ok_or_else(|| SysError::Message("CreateVertexShader: null".into()))?,
            ps_nv12: ps_nv12
                .ok_or_else(|| SysError::Message("CreatePixelShader(nv12): null".into()))?,
            ps_bgra: ps_bgra
                .ok_or_else(|| SysError::Message("CreatePixelShader(bgra): null".into()))?,
            sampler: sampler.ok_or_else(|| SysError::Message("CreateSamplerState: null".into()))?,
        })
    }
}

/// Was gezeichnet wird: NV12 (Y/UV-SRVs, CPU-Upload oder D3D11VA-Kopie) oder
/// RGBA (VSR-Interop, direktes Sampling).
pub enum DrawSource<'a> {
    Nv12(&'a ID3D11ShaderResourceView, &'a ID3D11ShaderResourceView),
    Rgba(&'a ID3D11ShaderResourceView),
}

/// Skalierungsmodus (C: window_type — identisch zur gpui-VideoSurface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ZoomMode {
    #[default]
    Fit,
    Zoom,
    Stretch,
}

/// Fullscreen-Dreieck + Letterbox-Viewport + Draw + Present. Läuft
/// ausschließlich im Render-Thread.
///
/// `sync_interval`: 0 = Present ohne Sync (Default, niedrigste Latenz);
/// 1 = am Display-Takt (settings/vsync, „Vertical sync") — rückt den
/// Inhaltswechsel auf Vblank-Grenzen, kostet bis zu ein Refresh-Intervall
/// Latenz.
///
/// `zoom_factor` (settings/zoom_factor, 0 = aus): bei ZoomMode::Zoom wird
/// statt der füllenden Skala die **Fit-Skala × Faktor** verwendet
/// („Benutzerdefinierter Zoom" — Faktor ≥ 1 croppt um den Overhang);
/// Fit/Stretch sind unverändert.
pub fn draw_and_present(
    d3d: &D3d11,
    shaders: &Shaders,
    source: DrawSource<'_>,
    video_size: (u32, u32),
    zoom: ZoomMode,
    zoom_factor: f32,
    sync_interval: u32,
) -> SysResult<()> {
    let (vw, vh) = (video_size.0.max(1) as f32, video_size.1.max(1) as f32);
    let (ww, wh) = (d3d.width.max(1) as f32, d3d.height.max(1) as f32);

    // Viewport-Mathe wie VideoSurface.paint (Fit/Zoom/Stretch).
    let (vp_x, vp_y, vp_w, vp_h) = match zoom {
        ZoomMode::Stretch => (0.0f32, 0.0f32, ww, wh),
        ZoomMode::Fit => {
            let scale = (ww / vw).min(wh / vh);
            let (w, h) = (vw * scale, vh * scale);
            ((ww - w) / 2.0, (wh - h) / 2.0, w, h)
        }
        ZoomMode::Zoom => {
            let scale = if zoom_factor > 1.0 {
                (ww / vw).min(wh / vh) * zoom_factor
            } else {
                (ww / vw).max(wh / vh)
            };
            let (w, h) = (vw * scale, vh * scale);
            ((ww - w) / 2.0, (wh - h) / 2.0, w, h)
        }
    };

    unsafe {
        let black = [0.0f32, 0.0, 0.0, 1.0];
        if let Some(rtv) = &d3d.render_target {
            d3d.context.ClearRenderTargetView(rtv, &black);
            let viewport = D3D11_VIEWPORT {
                TopLeftX: vp_x,
                TopLeftY: vp_y,
                Width: vp_w,
                Height: vp_h,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            let scissor = RECT {
                left: 0,
                top: 0,
                right: d3d.width as i32,
                bottom: d3d.height as i32,
            };
            d3d.context
                .OMSetRenderTargets(Some(&[Some(rtv.clone())]), None::<&ID3D11DepthStencilView>);
            d3d.context.RSSetViewports(Some(&[viewport]));
            d3d.context.RSSetScissorRects(Some(&[scissor]));
            d3d.context.RSSetState(Some(&d3d.rasterizer));
            d3d.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            d3d.context.VSSetShader(Some(&shaders.vs), None);
            let ps = match source {
                DrawSource::Nv12(..) => &shaders.ps_nv12,
                DrawSource::Rgba(..) => &shaders.ps_bgra,
            };
            d3d.context.PSSetShader(Some(ps), None);
            d3d.context.PSSetSamplers(0, Some(&[Some(shaders.sampler.clone())]));
            match source {
                DrawSource::Nv12(srv_y, srv_uv) => {
                    d3d.context
                        .PSSetShaderResources(0, Some(&[Some(srv_y.clone()), Some(srv_uv.clone())]));
                }
                DrawSource::Rgba(srv) => {
                    d3d.context
                        .PSSetShaderResources(0, Some(&[Some(srv.clone()), None]));
                }
            }
            d3d.context.Draw(3, 0);
            d3d.context.Flush();
        }
        d3d.swap_chain.Present(sync_interval, DXGI_PRESENT(0)).ok()?;
    }
    Ok(())
}

/// Device-Removed-Grund abfragen (für Log/Fallback). `None` = Device ok.
pub fn device_removed_reason(device: &ID3D11Device) -> Option<String> {
    unsafe {
        let hr = device.GetDeviceRemovedReason();
        if hr.is_ok() {
            None
        } else {
            Some(format!("{}", hr.unwrap_err()))
        }
    }
}

/// Roh-Pointer für FFmpeg-D3D11VA (AVD3D11VADeviceContext.device).
pub fn device_raw(d3d: &D3d11) -> *mut c_void {
    d3d.device.as_raw() as *mut c_void
}

/// Roh-Pointer für FFmpeg-D3D11VA (AVD3D11VADeviceContext.device_context).
pub fn context_raw(d3d: &D3d11) -> *mut c_void {
    d3d.context.as_raw() as *mut c_void
}
