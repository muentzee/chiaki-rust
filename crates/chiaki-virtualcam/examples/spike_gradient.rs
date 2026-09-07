// SPDX-License-Identifier: AGPL-3.0-only
//! Spike (HANDOFF §8/V1a): Testpattern → virtuelle Kamera. Verifiziert den
//! kompletten Pfad ohne Konsole — wir sind Writer der OBS-Shmem-Queue, ein
//! beliebiger Konsument (ffmpeg dshow / Discord / OBS) liest „OBS Virtual
//! Camera“.
//!
//! Ausführen:  cargo run -p chiaki-virtualcam --example spike_gradient -- --secs 8
//! Gegenprobe: ffmpeg -f dshow -i video="OBS Virtual Camera" -frames:v 3 frame_%d.png
//!             (die PNGs müssen den Gradient/Balken + Chroma-Streifen zeigen)

use chiaki_virtualcam::{CamFeed, CamFeedConfig, CamResolution};

struct Args {
    secs: u64,
    fps: u32,
    downscale: bool,
}

fn parse_args() -> Args {
    let mut args = Args { secs: 8, fps: 30, downscale: false };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--secs" => args.secs = argv.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(8),
            "--fps" => args.fps = argv.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(30),
            "--downscale" => args.downscale = true,
            _ => {}
        }
        i += 1;
    }
    args
}

fn main() {
    let args = parse_args();
    // Quelle 1920x1080 (zeigt gleichzeitig den Downscale-Pfad auf 720p).
    let (sw, sh) = (1920u32, 1080u32);
    let mut feed = CamFeed::open(CamFeedConfig {
        width: sw,
        height: sh,
        fps: args.fps,
        resolution: if args.downscale { CamResolution::P720 } else { CamResolution::Stream },
    })
    .expect("Kamera muss offen sein (OBS Virtual Camera installiert?)");
    let (w, h) = feed.dims();
    println!(
        "Spike: schicke {}x{} @ {} fps in „OBS Virtual Camera“ ({} s) …",
        w, h, args.fps, args.secs
    );

    // Quell-Buffer in STREAM-Auflösung mit künstlichem Stride-Padding
    // (_stride_ > width), damit Entstrip + Downscale-Pfad wirklich getestet
    // werden; die Kamera selbst läuft in feed.dims() (Zielauflösung).
    let stride = (sw as usize) + 64;
    let mut y = vec![0u8; stride * sh as usize];
    let mut uv = vec![0u8; stride * (sh as usize / 2)];
    let period = std::time::Duration::from_millis(u64::from(1000 / args.fps.max(1)));
    let started = std::time::Instant::now();
    let mut frame_index = 0u64;
    while started.elapsed().as_secs() < args.secs {
        let t = frame_index as f32 / args.fps as f32;
        let bar_x = ((t * 240.0) as usize) % (sw as usize + 80);
        for row in 0..sh as usize {
            for col in 0..sw as usize {
                let mut v = ((col / 3 + row / 3 + (t * 80.0) as usize) % 256) as u8;
                if col >= bar_x && col < bar_x + 80 {
                    v = 235; // Weißbalken
                }
                y[row * stride + col] = v;
            }
        }
        for row in 0..sh as usize / 2 {
            for pair in 0..sw as usize / 2 {
                // 8 rotierende Farbbalken im Chroma.
                let idx = (pair / (sw as usize / 16) + frame_index as usize / 30) % 8;
                let (u, v) = [(128u8, 128u8), (160, 70), (120, 170), (150, 40), (60, 200), (200, 150), (170, 40), (110, 110)][idx];
                uv[row * stride + pair * 2] = u;
                uv[row * stride + pair * 2 + 1] = v;
            }
        }
        feed.push_nv12(sw, sh, &y, stride, &uv, stride);
        std::thread::sleep(period);
        frame_index += 1;
    }
    println!(
        "Spike fertig: {} Frames gesendet, {} Fehler → Kamera schließt (Mapping frei)",
        feed.frames_sent(),
        feed.errors()
    );
    drop(feed);
}
