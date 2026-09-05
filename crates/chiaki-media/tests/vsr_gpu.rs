// SPDX-License-Identifier: AGPL-3.0-only
//! Voller VSR-GPU-Test (Integrationstest).
//!
//! Bewusst als Integrationstest und nicht als Lib-Unittest: Der NGX-Runtime-
//! Manager (`nvngxruntime.dll` → `IsFeatureAvailable`), von dem der
//! "VideoSuperRes"-Wrapper seine Parameter-Tabelle abhängt, meldet in
//! cargo-libtest-Prozessen mitunter "Feature nicht verfügbar" — das SDK baut
//! dann einen degradierten Effekt ohne `SrcImage0`/`DstImage0`-Parameter
//! (`NvVFX_SetImage` → NVCV_ERR_SELECTOR, `NvVFX_Run` → NVCV_ERR_PARAMETER).
//! In Integrationstest-/App-Prozessen (FFmpeg-DLLs + CUDA-Kontext zuerst,
//! SDK-DLLs danach) meldet NGX korrekt "verfügbar". [`VsrUpscaler::init`]
//! behandelt den degradierten Zustand trotzdem sauber (Init schlägt fehl,
//! VSR deaktiviert sich) — dafür gibt es den Unit-Test-Pfad in `vsr::tests`.
//!
//! GPU-nötig, daher `#[ignore]`:
//! ```text
//! cargo test -p chiaki-media --test vsr_gpu -- --ignored --nocapture
//! ```

use std::ptr::{self, NonNull};

use chiaki_media::{Decoder, FrameBuf, FrameFormat, HwBackend, Plane, VsrUpscaler};

/// Synthetischer NV12-Frame (Gradient + neutrales Chroma) mit echten
/// per-Plane-Pointern (Y und UV getrennt — wie der Decoder sie liefert).
fn synthetic_nv12_frame(w: u32, h: u32) -> (Vec<u8>, chiaki_media::DecodedFrame) {
    let pitch = w as usize;
    let y_size = pitch * h as usize;
    let uv_size = pitch * h as usize / 2;
    let mut buf = vec![0u8; y_size + uv_size];
    // Einfarbiges mittelgrau (Y=128, UV=128): VSR eines uniformen Bildes muss
    // uniform bleiben — robuster Statistik-Beweis als ein Saegezahn-Gradient,
    // auf den das SR-Netz unvorhersehbar reagieren darf.
    for b in buf.iter_mut() {
        *b = 128;
    }
    let base = NonNull::new(buf.as_ptr() as *mut u8).expect("non-null");
    let frame = chiaki_media::DecodedFrame {
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
        pts: 0.5,
        duration: 1.0 / 60.0,
        frames_lost: 2,
        recovered: false,
    };
    (buf, frame)
}

#[test]
#[ignore = "CUDA-GPU + VFX SDK nötig (NVDEC-Kontext, TensorRT-Engine-Load)"]
fn vsr_full_init_and_single_frame_upscale_on_gpu() {
    // Referenz-DLL-Pfade (FFmpeg + VFX SDK) wie im Crate-Test-Setup.
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

    // CUDA-Decoder → Kontext/Stream (derselbe Kontext, aus dem im C++
    // hw_frames_ctx → AVCUDADeviceContext gelesen wird).
    let decoder =
        Decoder::new(chiaki_core::error::Codec::H264, HwBackend::Cuda, 60)
            .expect("CUDA-Decoder muss auf dieser Maschine laufen");
    let cuda_ctx = decoder
        .cuda_context()
        .expect("NVDEC-CUDA-Kontext (VSR-Prämisse)");
    let cuda_stream = decoder.cuda_stream().unwrap_or(ptr::null_mut());
    println!("CUDA decoder context = {cuda_ctx:p}, stream = {cuda_stream:p}");

    let mut up = VsrUpscaler::new(None); // Auto-Detect via CHIAKI_VSR_SDK_DIR
    let (_buf, frame) = synthetic_nv12_frame(128, 128);

    assert!(
        up.init(&frame, cuda_ctx, cuda_stream, 200),
        "VSR init muss mit CUDA-Kontext klappen: last_error={:?}",
        up.last_error()
    );
    assert!(up.is_active());
    assert_eq!(up.output_size(), (256, 256));
    let load_ms = up.engine_load_ms().expect("engine load gemessen");
    println!("VSR engine load {load_ms} ms");

    // Mehrere Frames (Robustheit: fail_count bleibt 0)
    for i in 0..3 {
        let mut out = FrameBuf::new();
        assert!(
            up.process_frame(&frame, &mut out),
            "frame {i} muss skaliert werden: last_error={:?}",
            up.last_error()
        );
        assert_eq!(out.width(), 256);
        assert_eq!(out.height(), 256);
        assert_eq!(out.pitch(), chiaki_media::vsr::nv12_output_pitch(256));
        // kontiguierliches Layout: UV exakt hinter Y (pitch*h) — der Fix
        // gegen den grünen Streifen.
        let planes = out.planes();
        assert_eq!(
            planes[1].as_ptr() as usize - planes[0].as_ptr() as usize,
            out.pitch() * 256
        );
        // Metadaten übernommen (av_frame_copy_props-Äquivalent)
        assert!((out.pts() - 0.5).abs() < f64::EPSILON);
        assert_eq!(out.frames_lost(), 2);

        // Eingabe war Gradient + neutrales Chroma: der Output muss näherungs-
        // weise "normalisiert grau" bleiben (VSR eines flächigen Bildes ändert
        // die Statistik kaum) — gleichzeitig Beweis, dass echte Bytes
        // transferiert wurden.
        let y_mean =
            out.y().iter().map(|&b| b as u64).sum::<u64>() as f64 / out.y().len() as f64;
        let uv_mean =
            out.uv().iter().map(|&b| b as u64).sum::<u64>() as f64 / out.uv().len() as f64;
        println!("frame {i}: Y mean = {y_mean:.1}, UV mean = {uv_mean:.1}");
        assert!(
            (y_mean - 128.0).abs() < 24.0,
            "Y-Mittelwert soll näherungsweise grau bleiben ({y_mean})"
        );
        assert!(
            (uv_mean - 128.0).abs() < 12.0,
            "UV-Mittelwert soll neutral bleiben ({uv_mean})"
        );
    }
    assert!(up.last_error().is_none(), "kein Fehler erwartet");
    assert!(up.is_active(), "VSR muss nach den Frames aktiv bleiben");
}
