//! Golden-Harness für `chiaki_core::fec` (1:1-Port von lib/src/fec.c).
//!
//! Drei Ebenen:
//!
//! 1. **C-Golden-Vektoren:** `fec_test_cases.inl` ist die unveränderte
//!    Datentabelle aus `F:\projekte\chiaki-rust-remaster\test\fec_test_cases.inl`
//!    (Referenz-Frames der C-Tests, base64-kodiert). Der Test ist eine 1:1-
//!    Übersetzung von `test/fec.c`: Frame ins Strided-Layout bringen, über die
//!    Erasures 0x42-Garbage schreiben, dekodieren, die k Source-Units mit der
//!    Referenz vergleichen. Läuft immer (kein C-Build nötig).
//!
//! 2. **Bytegenauer Vergleich gegen die echte C-Bibliothek** (#[cfg(fec_golden_c)],
//!    freigeschaltet von build.rs, wenn die C-Referenz auffindbar ist:
//!    default `../chiaki-rust-remaster`, überschreibbar via `CHIAKI_C_REF`):
//!    build.rs kompiliert `lib/src/fec.c` + jerasure + gf-complete (MSVC;
//!    die SIMD-Pfade von gf-complete sind über GCC-Makros abgeschirmt und
//!    werden nicht kompiliert — die Tabellenpfade sind mathematisch
//!    identisch). Die FFI-Signaturen spiegeln fec.h bzw. cauchy.h.
//!
//! 3. **D6-Dokumentation:** `reed-solomon-erasure` teilt denselben Körper
//!    GF(2^8)/0x11d (Log-Tabellen-Generator 29 ist ein primitives Element
//!    dieses Felds), baut aber eine systematische Backblaze-Matrix
//!    (Vandermonde invertiert, oben Einheitsmatrix) statt jerasures
//!    Cauchy-Matrix 1/(i ^ (m+j)) und bietet keine API, um eine fremde Matrix
//!    zu injizieren. Der Test beweist das empirisch: Die Parity-Bytes
//!    unterscheiden sich → Custom-Port nötig.

use base64::alphabet::STANDARD as B64_ALPHABET;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use chiaki_core::fec;

/// base64-Engine ohne Padding-Anforderung (die .inl-Literale sind teils
/// mit, teils ohne '='-Padding konkateniert).
const B64: GeneralPurpose = GeneralPurpose::new(
    &B64_ALPHABET,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
);

/// XORShift-PRNG (deterministisch, versionsstabil — keine rand-Abhängigkeit
/// im Testvektor-Pfad).
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn xorshift_fill(buf: &mut [u8], seed: u64) {
    let mut state = seed | 1;
    for b in buf.iter_mut() {
        *b = xorshift(&mut state) as u8;
    }
}

// ---------------------------------------------------------------------------
// 1) C-Golden-Vektoren aus test/fec_test_cases.inl (Port von test/fec.c)
// ---------------------------------------------------------------------------

const TEST_CASES_INL: &str = include_str!("fec_test_cases.inl");

struct FecTestCase {
    k: u32,
    m: u32,
    /// Wie im C-Struct: -1-terminierte Liste.
    erasures: Vec<i32>,
    frame_buffer_b64: String,
    unit_size: usize,
}

/// Minimaler Parser für die C-Datentabelle:
///   { <k>, <m>, { <erasures, -1-terminiert> }, "<b64>" "<b64>"..., <unit_size> }
/// (C-String-Literale über Zeilengrenzen sind konkateniert; Kommentare und
/// Escapes kommen in der Tabelle nicht vor.)
fn parse_inl(s: &str) -> Vec<FecTestCase> {
    let b = s.as_bytes();

    fn skip(b: &[u8], mut i: usize, commas: bool) -> usize {
        while i < b.len() {
            match b[i] {
                b' ' | b'\t' | b'\r' | b'\n' => i += 1,
                b',' if commas => i += 1,
                _ => break,
            }
        }
        i
    }

    fn int(b: &[u8], i: &mut usize) -> i64 {
        let neg = b[*i] == b'-';
        if neg {
            *i += 1;
        }
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        let v: i64 = std::str::from_utf8(&b[start..*i]).unwrap().parse().unwrap();
        if neg {
            -v
        } else {
            v
        }
    }

    let arr_start = s.find("static FECTestCase fec_test_cases").expect("Array-Anfang");
    // Hinter die öffnende Klammer des Arrays springen; der Loop erwartet
    // jeweils die öffnende Klammer des nächsten Falls.
    let mut i = arr_start + b[arr_start..].iter().position(|&c| c == b'{').expect("Array-Klammer") + 1;

    let mut cases = Vec::new();
    loop {
        i = skip(b, i, true);
        if i >= b.len() || b[i] == b'}' {
            break;
        }
        assert_eq!(b[i], b'{', "Case-Klammer bei Byte {i}");
        i += 1;

        i = skip(b, i, false);
        let k = int(b, &mut i) as u32;
        i = skip(b, i, true);
        let m = int(b, &mut i) as u32;
        i = skip(b, i, true);

        // Erasures (bis zur schließenden Klammer, -1-terminiert wie im C)
        assert_eq!(b[i], b'{', "Erasures-Klammer bei Byte {i}");
        i += 1;
        let mut erasures = Vec::new();
        loop {
            i = skip(b, i, true);
            if b[i] == b'}' {
                i += 1;
                break;
            }
            erasures.push(int(b, &mut i) as i32);
        }

        // Konkatenierte base64-Stringliterale (Komma nach der Erasures-
        // Klammer mitüberspringen)
        let mut frame_buffer_b64 = String::new();
        loop {
            i = skip(b, i, true);
            if b[i] == b'"' {
                i += 1;
                while b[i] != b'"' {
                    frame_buffer_b64.push(b[i] as char);
                    i += 1;
                }
                i += 1;
            } else {
                break;
            }
        }

        i = skip(b, i, true);
        let unit_size = int(b, &mut i) as usize;

        i = skip(b, i, false);
        assert_eq!(b[i], b'}', "Case-Ende bei Byte {i}");
        i += 1;

        cases.push(FecTestCase { k, m, erasures, frame_buffer_b64, unit_size });
    }
    cases
}

/// Port von test/fec.c::test_fec_case über alle 64 C-Testfälle.
#[test]
fn golden_c_fec_test_cases_inl() {
    let cases = parse_inl(TEST_CASES_INL);
    assert_eq!(cases.len(), 64, "C-Testtabelle hat 64 Fälle");

    for (idx, tc) in cases.iter().enumerate() {
        let unit_size = tc.unit_size;
        let (k, m) = (tc.k as usize, tc.m as usize);
        // stride = ((unit_size + 0xf) / 0x10) * 0x10 — exakt wie im C-Test
        let stride = ((unit_size + 0xf) / 0x10) * 0x10;

        let frame_buffer_ref = B64.decode(tc.frame_buffer_b64.as_bytes())
            .unwrap_or_else(|e| panic!("case {idx}: base64: {e}"));
        assert_eq!(frame_buffer_ref.len(), unit_size * (k + m), "case {idx}");

        let mut frame_buffer = vec![0u8; stride * (k + m)];
        for u in 0..k + m {
            frame_buffer[stride * u..stride * u + unit_size]
                .copy_from_slice(&frame_buffer_ref[u * unit_size..(u + 1) * unit_size]);
        }

        let erasures: Vec<u32> =
            tc.erasures.iter().copied().take_while(|&e| e >= 0).map(|e| e as u32).collect();
        assert!(erasures.len() < 0x10, "C-Struct hat Platz für 0x10 Erasures");

        // Garbage über die Erasures schreiben (wie im C-Test)
        for &e in &erasures {
            let e = e as usize;
            assert!(e < k + m, "case {idx}");
            for b in &mut frame_buffer[stride * e..stride * e + unit_size] {
                *b = 0x42;
            }
        }

        fec::decode(&mut frame_buffer, unit_size, stride, tc.k, tc.m, &erasures)
            .unwrap_or_else(|e| panic!("case {idx}: decode: {e}"));

        // Alle k Source-Units müssen der Referenz entsprechen
        for u in 0..k {
            assert_eq!(
                &frame_buffer[stride * u..stride * u + unit_size],
                &frame_buffer_ref[u * unit_size..(u + 1) * unit_size],
                "case {idx} (k={k}, m={m}, erasures={erasures:?}): Unit {u}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2) Bytegenauer Vergleich gegen die C-Bibliothek (jerasure/gf-complete/fec.c)
// ---------------------------------------------------------------------------

// ChiakiErrorCode-Werte (lib/include/chiaki/common.h)
const CHIAKI_ERR_SUCCESS: i32 = 0;
const CHIAKI_ERR_INVALID_DATA: i32 = 11;
const CHIAKI_ERR_FEC_FAILED: i32 = 19;

#[cfg(fec_golden_c)]
mod c_reference {
    // FFI-Spiegel von lib/include/chiaki/fec.h und
    // third-party/jerasure/include/cauchy.h; gebaut von build.rs.
    extern "C" {
        pub fn chiaki_fec_encode(
            frame_buf: *mut u8,
            unit_size: usize,
            stride: usize,
            k: u32,
            m: u32,
        ) -> i32;
        pub fn chiaki_fec_decode(
            frame_buf: *mut u8,
            unit_size: usize,
            stride: usize,
            k: u32,
            m: u32,
            erasures: *const u32,
            erasures_count: usize,
        ) -> i32;
        pub fn cauchy_original_coding_matrix(k: u32, m: u32, w: i32) -> *mut i32;
        pub fn free(ptr: *mut core::ffi::c_void);
    }
}

/// Referenzframe erzeugen: k Zufalls-Source-Units, per C-chiaki_fec_encode
/// gepackt kodierte FEC-Units, alles ins Strided-Decode-Layout kopiert.
#[cfg(fec_golden_c)]
fn reference_frame(k: usize, m: usize, unit_size: usize, stride: usize, seed: u64) -> Vec<u8> {
    let mut packed = vec![0u8; (k + m) * unit_size];
    xorshift_fill(&mut packed[..k * unit_size], seed);
    let rc =
        unsafe { c_reference::chiaki_fec_encode(packed.as_mut_ptr(), unit_size, unit_size, k as u32, m as u32) };
    assert_eq!(rc, CHIAKI_ERR_SUCCESS);
    let mut frame = vec![0u8; stride * (k + m)];
    for u in 0..k + m {
        frame[stride * u..stride * u + unit_size]
            .copy_from_slice(&packed[u * unit_size..(u + 1) * unit_size]);
    }
    frame
}

// Alle C-FFI-Vergleiche laufen in EINEM Test: jerasure/gf-complete
// initialisieren ihr Galois-Feld lazy über globale, nicht threadsichere
// Zustände (gfp_array[]) — parallel laufende Tests würden sich dort
// gelegentlich gegenseitig abschießen (STATUS_ACCESS_VIOLATION).
#[cfg(fec_golden_c)]
#[test]
fn golden_c_reference_byte_identical() {
    matrix_matches_jerasure_cauchy();
    encode_byte_identical_with_c();
    decode_byte_identical_with_c();
    error_codes_match_c();
    c_fec_test_cases_inl_also_via_c();
}

#[cfg(fec_golden_c)]
fn matrix_matches_jerasure_cauchy() {
    for &(k, m) in
        &[(1u32, 1u32), (2, 2), (6, 1), (6, 3), (10, 4), (16, 4), (32, 8), (64, 16), (128, 128)]
    {
        let mat = unsafe { c_reference::cauchy_original_coding_matrix(k, m, 8) };
        assert!(!mat.is_null(), "cauchy matrix k={k} m={m}");
        let c_matrix = unsafe { std::slice::from_raw_parts(mat, (k * m) as usize) }.to_vec();
        unsafe { c_reference::free(mat.cast()) };
        assert_eq!(fec::create_matrix(k, m).unwrap(), c_matrix, "create_matrix k={k} m={m}");
    }
    // k+m > 2^8: C liefert NULL → FecError::Memory
    assert!(fec::create_matrix(200, 100).is_none());
    assert!(unsafe { c_reference::cauchy_original_coding_matrix(200, 100, 8) }.is_null());
}

#[cfg(fec_golden_c)]
fn encode_byte_identical_with_c() {
    for &(k, m, unit_size) in &[
        (1usize, 1usize, 16usize),
        (6, 1, 64),
        (6, 3, 48),
        (10, 4, 128),
        (16, 4, 100),
        (32, 8, 33),
        (64, 16, 16),
    ] {
        // stride == unit_size: das Layout, in dem chiaki_fec_encode nutzbar
        // ist (es schreibt die FEC-Units gepackt an frame_buf + k*unit_size).
        let mut frame_r = vec![0u8; (k + m) * unit_size];
        xorshift_fill(&mut frame_r[..k * unit_size], 0xE0 + k as u64 + m as u64);
        let mut frame_c = frame_r.clone();

        fec::encode(&mut frame_r, unit_size, unit_size, k as u32, m as u32).unwrap();
        let rc = unsafe {
            c_reference::chiaki_fec_encode(frame_c.as_mut_ptr(), unit_size, unit_size, k as u32, m as u32)
        };
        assert_eq!(rc, CHIAKI_ERR_SUCCESS, "encode k={k} m={m}");

        assert_eq!(frame_r, frame_c, "encode k={k} m={m} unit_size={unit_size}");
    }
}

#[cfg(fec_golden_c)]
fn decode_byte_identical_with_c() {
    /// Referenzframe + deterministische Erasure-Muster dekodieren (C und
    /// Rust) und Byteidentität des gesamten Frames fordern.
    fn run_case(k: usize, m: usize, unit_size: usize, stride: usize, seed: u64) {
        let reference = reference_frame(k, m, unit_size, stride, seed);

        let mut state = seed ^ 0x5eed_5eed;
        for pattern in 0..16u64 {
            let count = 1 + (pattern as usize % m);
            let mut erasures: Vec<u32> = Vec::with_capacity(count);
            while erasures.len() < count {
                let e = (xorshift(&mut state) % (k + m) as u64) as u32;
                if !erasures.contains(&e) {
                    erasures.push(e);
                }
            }
            erasures.sort_unstable();

            let mut frame_r = reference.clone();
            let mut frame_c = reference.clone();
            for &e in &erasures {
                let e = e as usize;
                for b in frame_r[stride * e..stride * e + unit_size].iter_mut() {
                    *b = 0x42;
                }
                for b in frame_c[stride * e..stride * e + unit_size].iter_mut() {
                    *b = 0x42;
                }
            }

            let rc = unsafe {
                c_reference::chiaki_fec_decode(
                    frame_c.as_mut_ptr(),
                    unit_size,
                    stride,
                    k as u32,
                    m as u32,
                    erasures.as_ptr(),
                    erasures.len(),
                )
            };
            assert_eq!(rc, CHIAKI_ERR_SUCCESS, "k={k} m={m} pattern={pattern}");

            fec::decode(&mut frame_r, unit_size, stride, k as u32, m as u32, &erasures)
                .unwrap_or_else(|e| panic!("k={k} m={m} pattern={pattern}: {e}"));

            // Beide Dekodierungen müssen byteidentisch sein UND vollständig
            // rekonstruieren.
            assert_eq!(frame_r, frame_c, "k={k} m={m} pattern={pattern}: C vs Rust");
            assert_eq!(frame_r, reference, "k={k} m={m} pattern={pattern}: Rekonstruktion");
        }

        // Edge-Case: keine Erasures → No-op, Puffer unverändert
        let mut frame_r = reference.clone();
        fec::decode(&mut frame_r, unit_size, stride, k as u32, m as u32, &[]).unwrap();
        assert_eq!(frame_r, reference);
    }

    // (k, m, unit_size, stride) mit stride > unit_size — genau wie im
    // Frameprocessor (buf_stride_per_unit >= buf_size_per_unit)
    run_case(6, 1, 64, 80, 0xD01);
    run_case(6, 3, 48, 64, 0xD02);
    run_case(10, 4, 128, 144, 0xD03);
    run_case(16, 4, 100, 112, 0xD04);
    run_case(32, 8, 17, 32, 0xD05);
}

#[cfg(fec_golden_c)]
fn error_codes_match_c() {
    let k = 6usize;
    let m = 1usize;
    let unit_size = 64usize;
    let stride = 80usize;
    let reference = reference_frame(k, m, unit_size, stride, 0xE1);

    // stride < unit_size → CHIAKI_ERR_INVALID_DATA
    let mut frame_r = reference.clone();
    let mut frame_c = reference.clone();
    assert_eq!(
        unsafe {
            c_reference::chiaki_fec_decode(
                frame_c.as_mut_ptr(),
                unit_size,
                32,
                k as u32,
                m as u32,
                std::ptr::null(),
                0,
            )
        },
        CHIAKI_ERR_INVALID_DATA
    );
    assert_eq!(
        fec::decode(&mut frame_r, unit_size, 32, k as u32, m as u32, &[]),
        Err(fec::FecError::InvalidData)
    );

    let mut frame_r = reference.clone();
    let mut frame_c = reference.clone();
    assert_eq!(
        unsafe {
            c_reference::chiaki_fec_encode(frame_c.as_mut_ptr(), unit_size, 32, k as u32, m as u32)
        },
        CHIAKI_ERR_INVALID_DATA
    );
    assert_eq!(
        fec::encode(&mut frame_r, unit_size, 32, k as u32, m as u32),
        Err(fec::FecError::InvalidData)
    );

    // k+m Erasures → CHIAKI_ERR_FEC_FAILED (jerasure: weniger als k intakt)
    let erasures: Vec<u32> = (0..(k + m) as u32).collect();
    let mut frame_r = reference.clone();
    let mut frame_c = reference.clone();
    assert_eq!(
        unsafe {
            c_reference::chiaki_fec_decode(
                frame_c.as_mut_ptr(),
                unit_size,
                stride,
                k as u32,
                m as u32,
                erasures.as_ptr(),
                erasures.len(),
            )
        },
        CHIAKI_ERR_FEC_FAILED
    );
    assert_eq!(
        fec::decode(&mut frame_r, unit_size, stride, k as u32, m as u32, &erasures),
        Err(fec::FecError::Failed)
    );

    // m+1 Erasures (gerade noch zu viel) ebenso
    let erasures: Vec<u32> = (0..(m + 1) as u32).collect();
    let mut frame_r = reference.clone();
    let mut frame_c = reference.clone();
    assert_eq!(
        unsafe {
            c_reference::chiaki_fec_decode(
                frame_c.as_mut_ptr(),
                unit_size,
                stride,
                k as u32,
                m as u32,
                erasures.as_ptr(),
                erasures.len(),
            )
        },
        CHIAKI_ERR_FEC_FAILED
    );
    assert_eq!(
        fec::decode(&mut frame_r, unit_size, stride, k as u32, m as u32, &erasures),
        Err(fec::FecError::Failed)
    );
}

/// Decode der ersten C-Testfälle zusätzlich gegen die echte C-Bibliothek —
/// verknüpft Ebene 1 und 2 (die .inl-Referenzframes stammen aus genau diesem
/// C-Code).
#[cfg(fec_golden_c)]
fn c_fec_test_cases_inl_also_via_c() {
    let cases = parse_inl(TEST_CASES_INL);
    for (idx, tc) in cases.iter().enumerate().take(8) {
        let unit_size = tc.unit_size;
        let (k, m) = (tc.k as usize, tc.m as usize);
        let stride = ((unit_size + 0xf) / 0x10) * 0x10;
        let reference_packed = B64.decode(tc.frame_buffer_b64.as_bytes()).expect("base64");

        let erasures: Vec<u32> =
            tc.erasures.iter().copied().take_while(|&e| e >= 0).map(|e| e as u32).collect();

        let mut frame_c = vec![0u8; stride * (k + m)];
        for u in 0..k + m {
            frame_c[stride * u..stride * u + unit_size]
                .copy_from_slice(&reference_packed[u * unit_size..(u + 1) * unit_size]);
        }
        for &e in &erasures {
            let e = e as usize;
            for b in frame_c[stride * e..stride * e + unit_size].iter_mut() {
                *b = 0x42;
            }
        }
        let rc = unsafe {
            c_reference::chiaki_fec_decode(
                frame_c.as_mut_ptr(),
                unit_size,
                stride,
                k as u32,
                m as u32,
                erasures.as_ptr(),
                erasures.len(),
            )
        };
        assert_eq!(rc, CHIAKI_ERR_SUCCESS, "case {idx}: C-decode");

        // Die k Source-Units des C-Ergebnisses müssen der b64-Referenz
        // entsprechen (das ist exakt die Behauptung des C-Tests).
        for u in 0..k {
            assert_eq!(
                &frame_c[stride * u..stride * u + unit_size],
                &reference_packed[u * unit_size..(u + 1) * unit_size],
                "case {idx}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3) D6: reed-solomon-erasure ist NICHT byteidentisch zu chiaki/jerasure
// ---------------------------------------------------------------------------

/// Beweist die D6-Entscheidung empirisch: rs-erasure kodiert dieselben
/// Source-Units mit seiner eigenen (systematischen) Matrix — die Parity-Bytes
/// unterscheiden sich von chiakis jerasure-Cauchy-Kodierung. Da die Crate
/// keine fremde Matrix injizieren kann, scheidet sie für einen
/// byteidentischen Port aus (der Feldkörper GF(2^8)/0x11d ist dagegen
/// identisch — nur die Matrix ist der Unterschied).
#[test]
fn d6_reed_solomon_erasure_is_not_byte_identical() {
    use reed_solomon_erasure::galois_8::ReedSolomon;

    for &(k, m) in &[(6usize, 1usize), (10, 4), (16, 4)] {
        let unit_size = 64;
        // Chiaki-seitige Kodierung (stride == unit_size → gepacktes Layout)
        let mut frame_r = vec![0u8; (k + m) * unit_size];
        xorshift_fill(&mut frame_r[..k * unit_size], 0xD6 + k as u64);
        fec::encode(&mut frame_r, unit_size, unit_size, k as u32, m as u32).unwrap();

        // rs-erasure über dieselben Source-Units
        let rs = ReedSolomon::new(k, m).expect("rs-erasure codec");
        let mut shards: Vec<Vec<u8>> =
            (0..k + m).map(|u| frame_r[u * unit_size..(u + 1) * unit_size].to_vec()).collect();
        let mut refs: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
        rs.encode(&mut refs).expect("rs-erasure encode");

        let chiaki_parity = &frame_r[k * unit_size..];
        let rs_parity: Vec<u8> =
            shards[k..].iter().flat_map(|s| s.iter().copied()).collect();
        assert_ne!(
            chiaki_parity, rs_parity,
            "k={k} m={m}: rs-erasure-Parity muss sich von jerasure-Cauchy unterscheiden"
        );
    }
}
