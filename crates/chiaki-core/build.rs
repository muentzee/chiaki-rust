//! Build-Skript mit zwei Zuständigkeiten:
//!
//! 1. Proto: kompiliert proto/takion.proto (nanopb-Referenz: chiaki-ng
//!    `set/@nanopb/takion.proto`) mit prost-build nach OUT_DIR.
//! 2. Golden-FEC-Harness: kompiliert jerasure + gf-complete aus der
//!    C-Referenz (chiaki-ng), damit tests/fec_golden.rs die Rust-FEC
//!    bytegenau gegen die C-Originalbibliothek vergleichen kann (extern "C"
//!    Deklarationen im Test). Die C-Bibliothek ist klein — sie wird immer
//!    mitgebaut, wenn die C-Referenz auffindbar ist. Ist sie es nicht, gibt
//!    es nur eine cargo:warning und das cfg-Flag `fec_golden_c` wird nicht
//!    gesetzt; die jerasure-FFI-Tests sind dann per #[cfg] ausgeblendet,
//!    während die Golden-Vektoren aus test/fec_test_cases.inl weiterlaufen.
//!
//! prost-build 0.13 bringt kein protoc mit und erwartet `protoc` im PATH.
//! Damit der Build ohne System-protoc funktioniert, wird das vendierte
//! protoc aus `protoc-bin-vendored` via PROTOC-Env-Var gesetzt.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));

    // takion.proto liegt workspace-übergreifend unter <workspace>/proto/.
    let proto = manifest_dir.join("../../proto/takion.proto");
    println!("cargo:rerun-if-changed={}", proto.display());

    // Vendiertes protoc (kein System-protoc nötig).
    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc not found");
    env::set_var("PROTOC", protoc);

    prost_build::Config::new()
        .compile_protos(&[proto], &[manifest_dir.join("../../proto")])
        .expect("failed to compile takion.proto");

    println!("cargo::rustc-check-cfg=cfg(fec_golden_c)");
    build_golden_fec_c();
}

// Golden-FEC: jerasure/gf-complete aus der C-Referenz (chiaki-ng) bauen.
// Pfad: default `../chiaki-rust-remaster` (Geschwister des Workspace),
// überschreibbar via `CHIAKI_C_REF`.
fn build_golden_fec_c() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let c_ref = env::var("CHIAKI_C_REF")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            manifest_dir
                .ancestors()
                .nth(2)
                .map(|ws| ws.join("..").join("chiaki-rust-remaster"))
                .unwrap_or_else(|| PathBuf::from("..").join("chiaki-rust-remaster"))
        });

    let gf = c_ref.join("third-party").join("gf-complete");
    let jer = c_ref.join("third-party").join("jerasure");
    let gf_src = gf.join("src");
    let jer_src = jer.join("src");
    if !gf_src.is_dir() || !jer_src.is_dir() {
        println!(
            "cargo:warning=chiaki-core: C-Referenz nicht gefunden unter {} - \
             jerasure/gf-complete werden nicht gebaut, tests/fec_golden.rs \
             laeuft ohne die FFI-Vergleiche (cfg fec_golden_c fehlt)",
            c_ref.display()
        );
        return;
    }

    println!("cargo:rerun-if-env-changed=CHIAKI_C_REF");
    // gf-complete: Die SIMD-Pfade sind über GCC-Makros (__x86_64__ +
    // INTEL_SSE*/ARM_NEON) abgeschirmt und werden mit MSVC nicht kompiliert —
    // die Tabellen-/Shift-Implementierungen liefern identische Ergebnisse.
    // Alle gf_w*.c werden gebraucht, weil gf.c/gf_method.c sie per switch
    // referenzieren.
    let mut files: Vec<(PathBuf, &str)> = Vec::new();
    for f in [
        "gf.c",
        "gf_cpu.c",
        "gf_general.c",
        "gf_method.c",
        "gf_rand.c",
        "gf_w4.c",
        "gf_w8.c",
        "gf_w16.c",
        "gf_w32.c",
        "gf_w64.c",
        "gf_w128.c",
        "gf_wgen.c",
    ] {
        files.push((gf_src.clone(), f));
    }
    for f in ["jerasure.c", "galois.c", "cauchy.c"] {
        files.push((jer_src.clone(), f));
    }
    // fec.c selbst: der Harness ruft chiaki_fec_encode/decode direkt auf und
    // vergleicht damit die komplette chiaki-FEC-Semantik (Layout inklusive).
    files.push((c_ref.join("lib").join("src"), "fec.c"));
    for (dir, f) in &files {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }

    let mut build = cc::Build::new();
    for (dir, f) in &files {
        build.file(dir.join(f));
    }
    build
        .include(gf.join("include"))
        .include(&gf_src)
        .include(jer.join("include"))
        .include(c_ref.join("lib").join("include"))
        .define("_CRT_SECURE_NO_WARNINGS", None)
        .warnings(false);
    build.compile("chiaki_jerasure");

    // Schaltet die FFI-Vergleichstests in tests/fec_golden.rs frei.
    println!("cargo:rustc-cfg=fec_golden_c");
}
