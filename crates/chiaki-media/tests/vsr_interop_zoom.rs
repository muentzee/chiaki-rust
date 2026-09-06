// SPDX-License-Identifier: AGPL-3.0-only
//! Readback-Repro für den Zoom-Bug im VSR-CUDA→D3D11-Interop-Pfad.
//!
//! Kette (wie im App-GPU-Pfad, aber ohne Sink-Fenster):
//! 1. Synthetisches NV12 1920x1080 mit **positionskodiertem** Muster
//!    (horizontaler Grau-Verlauf: Spalte x → `Y = 16 + x*219/1919`, UV=128)
//!    — CPU alloziert, per `NvCVImage_Transfer` auf ein SDK-GPU-NV12-Image
//!    gebracht (robuster Weg, REWORK Root Cause 2: keine rohe Driver-API).
//! 2. `VsrUpscaler::process_frame_gpu` (NVDEC-Gerätepointer-Schnittstelle,
//!    exakt wie `sessions.rs`) → VSR 200 % → GPU-RGBA 3840x2160.
//! 3. `CudaD3d11Interop::write_from` in eine D3D11-R8G8B8A8-Textur
//!    (vorher auf Magenta geklemmt, damit "nicht geschrieben" eindeutig wäre).
//! 4. Staging-Readback (CPU_ACCESS_READ + CopyResource + Map) → Spaltenprofil.
//!
//! Zwei Szenarien in einem Test (ein einziger teurer TensorRT-Engine-Load):
//! * **Repro (Pre-Fix-Sink)**: Interop-Ziel 1280x720 — exakt die Texturgröße,
//!   die `gpu_sink::render_thread` vor dem Fix registriert hat
//!   (`build_state(hwnd, 1280, 720)`-Hardcode trotz
//!   `GpuSink::new(.., (3840, 2160))`). Erwartet: der SDK-Transfer beschnet
//!   auf das Ziel — die Textur enthält nur den links-oberen 1280er-Streifen
//!   des 4K-Frames (Verlauf endet mitten im Grauwert statt am rechten Rand).
//!   Das ist das vom User gemeldete "vergrößert, oben-links verankert,
//!   rechts/unten abgeschnitten"-Symptom.
//! * **Regression (Post-Fix-Sink)**: Interop-Ziel 3840x2160 (= VSR-Output-
//!   Dims, so registriert der gefixte Sink). Erwartet: der Verlauf läuft
//!   über die VOLLE Breite — Samples an x=200/1920/3600 steigen monoton,
//!   Drittel-Mittelwerte klar getrennt, rechter Rand am Verlaufs-Ende.
//!
//! Grauwert-Muster + neutrales Chroma → RGB-Ausgabe ist exakt grau
//! (R=G=B ≈ 1.164·(Y−16), BT.601 limited→full); Magenta-Untergrund wäre
//! am G=0-Killer erkennbar.
//!
//! GPU-nötig, daher `#[ignore]`:
//! ```text
//! cargo test -p chiaki-media --test vsr_interop_zoom -- --ignored --nocapture
//! ```

use std::os::raw::{c_int, c_void};
use std::path::PathBuf;
use std::ptr::{self, NonNull};

use chiaki_media::cuda_d3d11::CudaD3d11Interop;
use chiaki_media::vsr::sys::{
    FnCuCtxPopCurrent, FnCuCtxPushCurrent, FnNvCVImageAlloc, FnNvCVImageDealloc, FnNvCVImageInit,
    FnNvCVImageTransfer, NvCVImage, NVCV_CPU, NVCV_GPU, NVCV_INTERLEAVED, NVCV_NV12,
    NVCV_SUCCESS, NVCV_U8, NVCV_YUV420, NVCV_Y,
};
use chiaki_media::{
    Decoder, DecodedFrame, FrameFormat, FrameMemory, HwBackend, Plane, VsrUpscaler,
};
use libloading::Library;
use windows::core::Interface as _;

// ---------------------------------------------------------------------------
// Mini-SDK-Loader (nur NVCVImage + Kontext-Push — der Test baut sein eigenes
// GPU-NV12-Input-Image; alles andere macht VsrUpscaler selbst).
// ---------------------------------------------------------------------------

const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

struct TestSdk {
    image_init: FnNvCVImageInit,
    image_alloc: FnNvCVImageAlloc,
    image_dealloc: FnNvCVImageDealloc,
    image_transfer: FnNvCVImageTransfer,
    ctx_push_current: FnCuCtxPushCurrent,
    ctx_pop_current: FnCuCtxPopCurrent,
    _libs: (Library, Library),
}

fn load_test_sdk() -> TestSdk {
    let sdk_dir = std::env::var_os("CHIAKI_VSR_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(r"F:\projekte\chiaki-rust-remaster\vfx_sdk\sdk\VideoFX\bin")
        });
    unsafe {
        // SAFETY: Handles bleiben für die Testdauer offen (SDK hält globale
        // Zustände — gleiche Konvention wie vsr/mod.rs).
        let ncvc: Library = libloading::os::windows::Library::load_with_flags(
            sdk_dir.join("NVCVImage.dll"),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
        .map(libloading::Library::from)
        .expect("NVCVImage.dll muss ladbar sein");
        let cuda: Library = libloading::os::windows::Library::load_with_flags(
            sdk_dir.join("nvcuda.dll"),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
        .map(libloading::Library::from)
        .or_else(|_| Library::new("nvcuda.dll"))
        .expect("nvcuda.dll muss ladbar sein (NVIDIA-Treiber)");
        let sym = |lib: &Library, name: &'static str| -> *mut c_void {
            lib.get::<*mut c_void>(format!("{name}\0").as_bytes())
                .map(|f| *f)
                .unwrap_or_else(|e| panic!("Symbol {name} fehlt: {e}"))
        };
        TestSdk {
            image_init: std::mem::transmute::<*mut c_void, FnNvCVImageInit>(sym(
                &ncvc,
                "NvCVImage_Init",
            )),
            image_alloc: std::mem::transmute::<*mut c_void, FnNvCVImageAlloc>(sym(
                &ncvc,
                "NvCVImage_Alloc",
            )),
            image_dealloc: std::mem::transmute::<*mut c_void, FnNvCVImageDealloc>(sym(
                &ncvc,
                "NvCVImage_Dealloc",
            )),
            image_transfer: std::mem::transmute::<*mut c_void, FnNvCVImageTransfer>(sym(
                &ncvc,
                "NvCVImage_Transfer",
            )),
            ctx_push_current: std::mem::transmute::<*mut c_void, FnCuCtxPushCurrent>(sym(
                &cuda,
                "cuCtxPushCurrent",
            )),
            ctx_pop_current: std::mem::transmute::<*mut c_void, FnCuCtxPopCurrent>(sym(
                &cuda,
                "cuCtxPopCurrent",
            )),
            _libs: (ncvc, cuda),
        }
    }
}

// ---------------------------------------------------------------------------
// Muster
// ---------------------------------------------------------------------------

/// Positionskodiertes NV12-Muster (CPU): Spalte x → `Y = 16 + x*219/(w-1)`
/// (limited-range-Grauverlauf 16..235), UV konstant 128 (neutral).
fn gradient_nv12_cpu(w: u32, h: u32) -> Vec<u8> {
    let pitch = w as usize;
    let y_size = pitch * h as usize;
    let mut buf = vec![0u8; y_size + pitch * h as usize / 2];
    for row in 0..h as usize {
        for x in 0..w as usize {
            let y = 16.0 + (x as f32 / (w as f32 - 1.0)) * 219.0;
            buf[row * pitch + x] = y.round().clamp(16.0, 235.0) as u8;
        }
    }
    for b in &mut buf[y_size..] {
        *b = 128;
    }
    buf
}

fn cpu_decoded_frame(w: u32, h: u32, buf: &[u8]) -> DecodedFrame {
    let pitch = w as usize;
    let y_size = pitch * h as usize;
    let base = NonNull::new(buf.as_ptr() as *mut u8).expect("non-null buffer");
    DecodedFrame {
        width: w,
        height: h,
        format: FrameFormat::Nv12,
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
        pts: 0.0,
        duration: 1.0 / 60.0,
        frames_lost: 0,
        recovered: false,
        memory: FrameMemory::Cpu,
    }
}

/// SDK-GPU-NV12-Image mit dem Muster füllen (CPU → GPU per NvCVImage_Transfer,
/// pro-Plane Y/UV-Views — identisch zur App-Staging-Logik).
unsafe fn upload_gradient_nv12_to_gpu(
    sdk: &TestSdk,
    w: u32,
    h: u32,
    cpu: &[u8],
    cuda_ctx: *mut c_void,
    cuda_stream: *mut c_void,
) -> Box<NvCVImage> {
    let pitch = w as usize;
    let y_size = pitch * h as usize;

    let rc = (sdk.ctx_push_current)(cuda_ctx);
    assert_eq!(rc, 0, "cuCtxPushCurrent(decoder) muss klappen");
    let mut gpu_nv12 = Box::new(NvCVImage::zeroed());
    let st = (sdk.image_alloc)(
        &mut *gpu_nv12,
        w,
        h,
        NVCV_YUV420,
        NVCV_U8,
        NVCV_NV12,
        NVCV_GPU,
        0,
    );
    assert_eq!(st, NVCV_SUCCESS, "GPU-NV12-Alloc (status {st})");

    // CPU-Source-Views + GPU-Dest-Views (Y und UV einzeln).
    let mut cpu_y = NvCVImage::zeroed();
    let mut cpu_uv = NvCVImage::zeroed();
    let mut gpu_y = NvCVImage::zeroed();
    let mut gpu_uv = NvCVImage::zeroed();
    (sdk.image_init)(
        &mut cpu_y,
        w,
        h,
        pitch as c_int,
        cpu.as_ptr() as *mut c_void,
        NVCV_Y,
        NVCV_U8,
        NVCV_INTERLEAVED,
        NVCV_CPU,
    );
    (sdk.image_init)(
        &mut cpu_uv,
        w,
        h / 2,
        pitch as c_int,
        cpu.as_ptr().add(y_size) as *mut c_void,
        NVCV_Y,
        NVCV_U8,
        NVCV_INTERLEAVED,
        NVCV_CPU,
    );
    (sdk.image_init)(
        &mut gpu_y,
        w,
        h,
        gpu_nv12.pitch,
        gpu_nv12.pixels,
        NVCV_Y,
        NVCV_U8,
        NVCV_INTERLEAVED,
        NVCV_GPU,
    );
    (sdk.image_init)(
        &mut gpu_uv,
        w,
        h / 2,
        gpu_nv12.pitch,
        (gpu_nv12.pixels as *mut u8).add(gpu_nv12.pitch as usize * h as usize) as *mut c_void,
        NVCV_Y,
        NVCV_U8,
        NVCV_INTERLEAVED,
        NVCV_GPU,
    );

    let mut tmp = Box::new(NvCVImage::zeroed());
    let st_y = (sdk.image_transfer)(&cpu_y, &mut gpu_y, 1.0, cuda_stream, &mut *tmp);
    let st_uv = (sdk.image_transfer)(&cpu_uv, &mut gpu_uv, 1.0, cuda_stream, &mut *tmp);
    let mut old: *mut c_void = ptr::null_mut();
    (sdk.ctx_pop_current)(&mut old);
    assert_eq!(st_y, NVCV_SUCCESS, "Y CPU→GPU (status {st_y})");
    assert_eq!(st_uv, NVCV_SUCCESS, "UV CPU→GPU (status {st_uv})");
    gpu_nv12
}

// ---------------------------------------------------------------------------
// D3D11: Device + RGBA-Textur (Magenta-gecleart) + Staging-Readback
// ---------------------------------------------------------------------------

fn create_d3d11_device() -> (
    windows::Win32::Graphics::Direct3D11::ID3D11Device,
    windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
) {
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_SDK_VERSION,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL(0);
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .expect("D3D11CreateDevice");
        (device.expect("device"), context.expect("context"))
    }
}

/// R8G8B8A8-Textur (SHADER_RESOURCE | RENDER_TARGET), initial auf Magenta
/// geklemmt — "nicht vom Transfer geschrieben" bliebe damit eindeutig lesbar
/// (Magenta: R=191, G=0 — Grauinhalt hat G≈R).
fn create_cleared_rgba_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    w: u32,
    h: u32,
) -> windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11RenderTargetView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
        D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    unsafe {
        let mut tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .expect("CreateTexture2D(RGBA)");
        let tex = tex.expect("texture");
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device
            .CreateRenderTargetView(&tex, None, Some(&mut rtv))
            .expect("CreateRenderTargetView");
        context.ClearRenderTargetView(&rtv.expect("rtv"), &[0.75, 0.0, 0.75, 1.0]);
        tex
    }
}

/// Readback: CopyResource auf eine Staging-Textur + Map → (Bytes, RowPitch).
fn readback_texture(
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    tex: &windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    w: u32,
    h: u32,
) -> (Vec<u8>, u32) {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Texture2D,
    };
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        tex.GetDesc(&mut desc);
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let device = context.GetDevice().expect("GetDevice");
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .expect("CreateTexture2D(staging)");
        let staging = staging.expect("staging");
        context.CopyResource(&staging, tex);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("Map(staging)");
        let row_pitch = mapped.RowPitch;
        let mut out = vec![0u8; row_pitch as usize * h as usize];
        std::ptr::copy_nonoverlapping(mapped.pData.cast(), out.as_mut_ptr(), out.len());
        context.Unmap(&staging, 0);
        (out, row_pitch)
    }
}

// ---------------------------------------------------------------------------
// Analyse (RGBA-Readback; Grauinhalt → R=G=B)
// ---------------------------------------------------------------------------

fn channel_at(data: &[u8], row_pitch: u32, x: u32, y: u32, ch: usize) -> u8 {
    let off = row_pitch as usize * y as usize + x as usize * 4 + ch;
    data[off]
}

fn red_at(data: &[u8], row_pitch: u32, x: u32, y: u32) -> u8 {
    channel_at(data, row_pitch, x, y, 0)
}

fn green_at(data: &[u8], row_pitch: u32, x: u32, y: u32) -> u8 {
    channel_at(data, row_pitch, x, y, 1)
}

/// Mittleres R über einen Spaltenbereich und die mittleren Scanlines (robust
/// gegen punktuelles VSR-Netz-Rauschen).
fn mean_red_over_columns(data: &[u8], row_pitch: u32, h: u32, x0: u32, x1: u32) -> f64 {
    let (y0, y1) = (h / 3, h * 2 / 3);
    let mut sum = 0f64;
    let mut n = 0u64;
    for y in y0..y1 {
        for x in x0..x1 {
            sum += red_at(data, row_pitch, x, y) as f64;
            n += 1;
        }
    }
    sum / n.max(1) as f64
}

// ---------------------------------------------------------------------------
// Der Test
// ---------------------------------------------------------------------------

#[test]
#[ignore = "CUDA-GPU + VFX SDK + D3D11 nötig (NVDEC-Kontext, TensorRT-Engine-Load)"]
fn vsr_interop_writes_full_gradient_into_full_size_texture() {
    if std::env::var_os("CHIAKI_FFMPEG_DIR").is_none() {
        std::env::set_var(
            "CHIAKI_FFMPEG_DIR",
            r"F:\projekte\chiaki-rust-remaster\ffmpeg-n7.1-latest-win64-gpl-shared-7.1\bin",
        );
    }
    if std::env::var_os("CHIAKI_VSR_SDK_DIR").is_none() {
        std::env::set_var(
            "CHIAKI_VSR_SDK_DIR",
            r"F:\projekte\chiaki-rust-remaster\vfx_sdk\sdk\VideoFX\bin",
        );
    }

    let (in_w, in_h) = (1920u32, 1080u32);
    let (out_w, out_h) = chiaki_media::vsr::output_dims(in_w, in_h, 200);
    assert_eq!((out_w, out_h), (3840, 2160));

    // --- CUDA-Decoder (Kontext/Stream) + SDK -------------------------------
    let decoder = Decoder::new(chiaki_core::error::Codec::H264, HwBackend::Cuda, 60)
        .expect("CUDA-Decoder muss auf dieser Maschine laufen");
    let cuda_ctx = decoder
        .cuda_context()
        .expect("NVDEC-CUDA-Kontext (VSR-Prämisse)");
    let cuda_stream = decoder.cuda_stream().unwrap_or(ptr::null_mut());
    let sdk = load_test_sdk();

    // --- VSR init (liest nur width/height vom Frame) ------------------------
    let mut up = VsrUpscaler::new(None);
    let init_frame_buf = vec![128u8; in_w as usize * in_h as usize * 3 / 2];
    let init_frame = cpu_decoded_frame(in_w, in_h, &init_frame_buf);
    assert!(
        up.init(&init_frame, cuda_ctx, cuda_stream, 200, None),
        "VSR init: last_error={:?}",
        up.last_error()
    );
    assert_eq!(up.output_size(), (out_w, out_h));
    println!("VSR engine load: {:?} ms", up.engine_load_ms());

    // --- GPU-NV12-Input mit positionskodiertem Muster + process_frame_gpu ---
    let pattern_cpu = gradient_nv12_cpu(in_w, in_h);
    let mut gpu_nv12 = unsafe {
        upload_gradient_nv12_to_gpu(&sdk, in_w, in_h, &pattern_cpu, cuda_ctx, cuda_stream)
    };
    let pitch = gpu_nv12.pitch as usize;
    let y_dev = gpu_nv12.pixels as *const c_void;
    let uv_dev =
        unsafe { (gpu_nv12.pixels as *mut u8).add(pitch * in_h as usize) as *const c_void };
    assert!(
        up.process_frame_gpu(
            y_dev,
            pitch,
            uv_dev,
            pitch,
            in_w,
            in_h,
            0.0,
            1.0 / 60.0,
            0,
            false
        ),
        "process_frame_gpu: last_error={:?}",
        up.last_error()
    );
    let (dims_w, dims_h, dst_pitch) = up.gpu_rgba_dims().expect("VSR aktiv → GPU-RGBA-Dims");
    println!("VSR GPU-RGBA-Output: {dims_w}x{dims_h} (pitch {dst_pitch})");
    assert_eq!((dims_w, dims_h), (out_w, out_h));
    let src_img = up.gpu_rgba_image().expect("GPU-RGBA-Image");

    // --- D3D11 ---------------------------------------------------------------
    let (device, context) = create_d3d11_device();

    // Szenario 1 — REPRO (Pre-Fix-Sink-Größe): Interop-Ziel 1280x720.
    {
        let tex = create_cleared_rgba_texture(&device, &context, 1280, 720);
        let mut interop =
            CudaD3d11Interop::new(cuda_ctx, cuda_stream, tex.as_raw() as *mut c_void)
                .expect("Interop auf 1280x720-Textur");
        unsafe { interop.write_from(src_img) }.expect("write_from (1280x720)");
        let (data, row_pitch) = readback_texture(&context, &tex, 1280, 720);

        let y_mid = 360;
        let r_left = mean_red_over_columns(&data, row_pitch, 720, 40, 200);
        let r_right = red_at(&data, row_pitch, 1279, y_mid);
        let g_right = green_at(&data, row_pitch, 1279, y_mid);
        println!("REPRO 1280x720: Mittel(links 40..200)={r_left:.1}, R(1279)={r_right}, G(1279)={g_right}");
        // Linker Rand des Streifens: dunkler Verlaufsanfang (Grauinhalt, kein
        // Magenta — G>0), Mittel deutlich unter 60.
        assert!(
            (0.0..60.0).contains(&r_left),
            "linker Streifen soll den DUNKLEN Verlaufsanfang zeigen (Mittel {r_left})"
        );
        // Rechter Rand des Streifens: Grauinhalt (G>0 → kein Magenta-Rest)
        // und MITTEN im Verlauf (nicht am Ende ~255 → kein Resize-to-fit),
        // aber schon deutlich gestiegen (>40 → kein Nichts-geschrieben).
        assert!(
            g_right > 20,
            "rechter Streifenrand muss Grauinhalt haben (G={g_right})"
        );
        assert!(
            (40..180).contains(&r_right),
            "Verlauf soll am rechten Streifenrand MITTEN enden (Crop-Signatur), \
             R(1279)={r_right}"
        );
    }

    // Szenario 2 — REGRESSION (Post-Fix-Sink-Größe): Interop-Ziel 3840x2160.
    {
        let tex = create_cleared_rgba_texture(&device, &context, out_w, out_h);
        let mut interop =
            CudaD3d11Interop::new(cuda_ctx, cuda_stream, tex.as_raw() as *mut c_void)
                .expect("Interop auf 3840x2160-Textur");
        unsafe { interop.write_from(src_img) }.expect("write_from (3840x2160)");
        let (data, row_pitch) = readback_texture(&context, &tex, out_w, out_h);

        let y_mid = out_h / 2;
        // 1) Voll geschrieben: Grauinhalt (G>0) an allen drei Positionen.
        for x in [200u32, 1920, 3600] {
            assert!(
                green_at(&data, row_pitch, x, y_mid) > 8,
                "x={x}: G={} — Textur dort nicht geschrieben?",
                green_at(&data, row_pitch, x, y_mid)
            );
        }
        // 2) Positionskodiert: R steigt monoton über die VOLLE Breite.
        let (r200, r1920, r3600) = (
            red_at(&data, row_pitch, 200, y_mid),
            red_at(&data, row_pitch, 1920, y_mid),
            red_at(&data, row_pitch, 3600, y_mid),
        );
        println!("REGRESSION 3840x2160: R(200)={r200}, R(1920)={r1920}, R(3600)={r3600}");
        assert!(
            r200 < r1920 && r1920 < r3600,
            "Verlauf muss über die volle Breite monoton steigen \
             (R(200)={r200}, R(1920)={r1920}, R(3600)={r3600})"
        );
        // 3) Drittel-Mittelwerte klar getrennt (kein komprimiertes Repeat).
        let left = mean_red_over_columns(&data, row_pitch, out_h, 40, 300);
        let mid = mean_red_over_columns(&data, row_pitch, out_h, 1700, 2100);
        let right = mean_red_over_columns(&data, row_pitch, out_h, 3500, 3800);
        println!("REGRESSION Drittel-Mittel: links={left:.1}, mitte={mid:.1}, rechts={right:.1}");
        assert!(
            left + 40.0 < mid && mid + 40.0 < right,
            "Drittel-Mittel müssen klar steigen (links {left}, mitte {mid}, rechts {right})"
        );
        // 4) Rechter Rand zeigt das ENDE des Verlaufs (nahe 255, kein Crop):
        assert!(
            right > 190.0,
            "rechter Rand soll das Verlaufsende zeigen (Mittel {right})"
        );
    }

    // GPU-Input-Image freigeben (SDK-Vertrag: Alloc/Dealloc im Decoder-Kontext).
    unsafe {
        let rc = (sdk.ctx_push_current)(cuda_ctx);
        if rc == 0 {
            (sdk.image_dealloc)(&mut *gpu_nv12);
            let mut old: *mut c_void = ptr::null_mut();
            (sdk.ctx_pop_current)(&mut old);
        }
    }
}
