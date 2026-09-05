// FEC (Reed-Solomon, Cauchy-Matrix, GF(2^8)) — 1:1-Port von
//   lib/src/fec.c + lib/include/chiaki/fec.h (chiaki-ng)
// und der von fec.c benutzten jerasure/gf-complete-Teile:
//   third-party/jerasure/src/cauchy.c   (cauchy_original_coding_matrix)
//   third-party/jerasure/src/jerasure.c (jerasure_matrix_encode/decode,
//                                        jerasure_matrix_dotprod,
//                                        jerasure_make_decoding_matrix,
//                                        jerasure_invert_matrix,
//                                        jerasure_erasures_to_erased)
//   third-party/gf-complete (Default-Galois-Feld w=8, primitives Polynom
//                            0x11d — siehe gf_w8.c: "h->prim_poly = 0x11d")
//
// chiaki nutzt die FEC so: Ein Frame besteht aus k Source-Units + m FEC-Units
// gleicher Länge (unit_size), die in einem frame_buf liegen. Encode berechnet
// die m FEC-Units als Linearkombination der Source-Units über eine Cauchy-
// Matrix in GF(2^8) (w = CHIAKI_FEC_WORDSIZE = 8). Decode rekonstruiert bis
// zu m gelöschte Units (erasures) in-place.
//
// Byteidentität mit jerasure ist durch denselben Körper GF(2^8)/0x11d,
// dieselbe Cauchy-Matrix und denselben Decode-Algorithmus gegeben; der
// Golden-Harness (tests/fec_golden.rs) vergleicht bytegenau gegen die echte
// C-Bibliothek (via cc in build.rs gebaut) und gegen die Testvektoren aus
// test/fec_test_cases.inl.
//
// D6-Entscheidung: `reed-solomon-erasure` ist NICHT byteidentisch (eigene
// systematische Backblaze-Matrix statt jerasure-Cauchy) — siehe
// tests/fec_golden.rs::d6_reed_solomon_erasure_is_not_byte_identical.

use std::sync::OnceLock;

/// CHIAKI_FEC_WORDSIZE (fec.h): Galois-Feld-Breite.
pub const FEC_WORDSIZE: usize = 8;

/// Primitives Polynom des Default-Felds GF(2^8) in gf-complete (gf_w8.c,
/// `h->prim_poly = 0x11d`).
const GF_POLY: u16 = 0x11d;

/// Entspricht den `ChiakiErrorCode`-Werten, die chiaki_fec_encode/decode
/// zurückgeben können (CHIAKI_ERR_INVALID_DATA / _MEMORY / _FEC_FAILED).
/// TODO: mit `crate::error::ChiakiError` vereinheitlichen, sobald dessen
/// Varianten vorliegen (error.rs wird parallel portiert).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FecError {
    /// CHIAKI_ERR_INVALID_DATA — z. B. stride < unit_size oder frame_buf zu
    /// klein (in C: ungeprüfter Zugriff, hier definiert abgefangen).
    #[error("invalid data")]
    InvalidData,
    /// CHIAKI_ERR_MEMORY — Matrix-Erzeugung fehlgeschlagen (k+m > 2^w).
    #[error("out of memory")]
    Memory,
    /// CHIAKI_ERR_FEC_FAILED — Decode nicht möglich (zu viele/nicht
    /// invertierbare Erasures).
    #[error("fec failed")]
    Failed,
}

type Result<T> = std::result::Result<T, FecError>;

// ---------------------------------------------------------------------------
// GF(2^8): Multiplikation/Division wie gf-complete (Default-Implementierung)
// ---------------------------------------------------------------------------

/// gf_single_multiply per Shift-Add (entspricht der gf-w8 Shift-Multiplikation
/// mit prim_poly 0x11d). Wird nur zum Aufbau der Tabellen benutzt.
fn gf_mul_slow(a: u8, b: u8) -> u8 {
    let mut a = a as u16;
    let mut b = b;
    let mut r: u16 = 0;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= GF_POLY;
        }
        b >>= 1;
    }
    r as u8
}

/// Volle 256x256-Multiplikationstabelle + Inverse-Tabelle des Felds.
struct GfTables {
    mul: Box<[[u8; 256]; 256]>,
    inv: Box<[u8; 256]>,
}

fn gf_tables() -> &'static GfTables {
    static TABLES: OnceLock<GfTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut mul = vec![[0u8; 256]; 256];
        for (a, row) in mul.iter_mut().enumerate() {
            for (b, cell) in row.iter_mut().enumerate() {
                *cell = gf_mul_slow(a as u8, b as u8);
            }
        }
        let mut inv = [0u8; 256];
        // inv[0] bleibt 0 (wie gf-complete: inverse(0) = 0 → Division durch 0 = 0)
        for a in 1..=255usize {
            for b in 1..=255usize {
                if mul[a][b] == 1 {
                    inv[a] = b as u8;
                    break;
                }
            }
        }
        GfTables { mul: Box::new(mul.try_into().unwrap()), inv: Box::new(inv) }
    })
}

/// galois_single_multiply(a, b, 8)
fn gf_mul(a: u8, b: u8) -> u8 {
    gf_tables().mul[a as usize][b as usize]
}

/// galois_single_divide(a, b, 8) = a * b^-1
fn gf_div(a: u8, b: u8) -> u8 {
    gf_mul(a, gf_tables().inv[b as usize])
}

// ---------------------------------------------------------------------------
// cauchy_original_coding_matrix (jerasure/src/cauchy.c)
// ---------------------------------------------------------------------------

/// `create_matrix` aus fec.c = cauchy_original_coding_matrix(k, m,
/// CHIAKI_FEC_WORDSIZE). Zeile i (0..m) ist die Coding-Zeile für FEC-Unit i,
/// Spalte j (0..k) gehört zu Source-Unit j:
///   matrix[i*k + j] = 1 / (i ^ (m + j))   (in GF(2^8))
/// None wie in C (NULL), wenn k+m die Feldgröße sprengt.
pub fn create_matrix(k: u32, m: u32) -> Option<Vec<i32>> {
    let w = FEC_WORDSIZE;
    if w < 31 && k + m > (1 << w) as u32 {
        return None;
    }
    let (k, m) = (k as usize, m as usize);
    let mut matrix = vec![0i32; k * m];
    for (i, row) in matrix.chunks_mut(k).enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            *cell = gf_div(1, (i ^ (m + j)) as u8) as i32;
        }
    }
    Some(matrix)
}

// ---------------------------------------------------------------------------
// jerasure_invert_matrix (jerasure.c) — Gauß-Elimination über GF(2^8)
// ---------------------------------------------------------------------------

fn invert_matrix(mat: &mut [i32], inv: &mut [i32], rows: usize) -> std::result::Result<(), ()> {
    let cols = rows;

    // inv = Einheitsmatrix
    for i in 0..rows {
        for j in 0..cols {
            inv[i * cols + j] = if i == j { 1 } else { 0 };
        }
    }

    // Erst in obere Dreiecksform bringen
    for i in 0..cols {
        let row_start = cols * i;

        // Zeile tauschen, wenn das i,i-Element 0 ist; wenn nicht tauschbar,
        // ist die Matrix singulär.
        if mat[row_start + i] == 0 {
            let mut j = i + 1;
            while j < rows && mat[cols * j + i] == 0 {
                j += 1;
            }
            if j == rows {
                return Err(());
            }
            let rs2 = j * cols;
            for k in 0..cols {
                mat.swap(row_start + k, rs2 + k);
                inv.swap(row_start + k, rs2 + k);
            }
        }

        // Zeile auf 1/element[i][i] normieren
        let tmp = mat[row_start + i];
        if tmp != 1 {
            let inverse = gf_div(1, tmp as u8) as i32;
            for j in 0..cols {
                mat[row_start + j] = gf_mul(mat[row_start + j] as u8, inverse as u8) as i32;
                inv[row_start + j] = gf_mul(inv[row_start + j] as u8, inverse as u8) as i32;
            }
        }

        // Für alle j>i: A_ji * A_i auf A_j addieren (XOR)
        for j in i + 1..cols {
            let k = cols * j + i;
            if mat[k] != 0 {
                let rs2 = cols * j;
                if mat[k] == 1 {
                    for x in 0..cols {
                        mat[rs2 + x] ^= mat[row_start + x];
                        inv[rs2 + x] ^= inv[row_start + x];
                    }
                } else {
                    let tmp = mat[k];
                    for x in 0..cols {
                        mat[rs2 + x] ^= gf_mul(tmp as u8, mat[row_start + x] as u8) as i32;
                        inv[rs2 + x] ^= gf_mul(tmp as u8, inv[row_start + x] as u8) as i32;
                    }
                }
            }
        }
    }

    // Obere Dreiecksform: von unten nach oben fertig multiplizieren
    for i in (0..rows).rev() {
        let row_start = i * cols;
        for j in 0..i {
            let rs2 = j * cols;
            if mat[rs2 + i] != 0 {
                let tmp = mat[rs2 + i];
                mat[rs2 + i] = 0;
                for k in 0..cols {
                    inv[rs2 + k] ^= gf_mul(tmp as u8, inv[row_start + k] as u8) as i32;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// jerasure_matrix_dotprod (jerasure.c), w=8
// ---------------------------------------------------------------------------

/// `jerasure_matrix_dotprod` für w=8:
///   dest = XOR_j matrix_row[j] * unit(src_ids[j])
///
/// Das C-Original schreibt per memcpy (Koeffizient 1), galois_region_xor und
/// galois_w08_region_multiply (init-Flag: overwrite vs. XOR) in das Ziel —
/// da alle Operationen XOR-basiert sind, ist das Ergebnis exakt die
/// XOR-Summe der Produkte, hier ohne Aliasing direkt in `out` akkumuliert.
/// Die Source-Unit `idx` liegt wie überall in diesem Modul bei
/// `frame_buf[stride*idx .. stride*idx + unit_size]`.
fn matrix_dotprod(
    matrix_row: &[i32],
    src_ids: &[usize],
    frame_buf: &[u8],
    stride: usize,
    unit_size: usize,
    out: &mut [u8],
) {
    for v in out.iter_mut() {
        *v = 0;
    }
    for (j, &coef) in matrix_row.iter().enumerate() {
        if coef == 0 {
            continue;
        }
        let src = &frame_buf[stride * src_ids[j]..stride * src_ids[j] + unit_size];
        if coef == 1 {
            for (d, s) in out.iter_mut().zip(src.iter()) {
                *d ^= *s;
            }
        } else {
            let coef = coef as u8;
            for (d, s) in out.iter_mut().zip(src.iter()) {
                *d ^= gf_mul(coef, *s);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// chiaki_fec_encode (fec.c)
// ---------------------------------------------------------------------------

/// Berechnet die m FEC-Units für die k Source-Units in `frame_buf` und
/// schreibt sie — wie in fec.c — **gepackt** hinter die k Source-Units:
/// `frame_buf[k*unit_size + i*unit_size ..]`, während die Source-Units bei
/// `stride*i` liegen (das FEC-Ergebnis hängt also nicht vom `stride` ab).
///
/// C: `chiaki_fec_encode(frame_buf, unit_size, stride, k, m)`
pub fn encode(frame_buf: &mut [u8], unit_size: usize, stride: usize, k: u32, m: u32) -> Result<()> {
    if stride < unit_size {
        return Err(FecError::InvalidData);
    }
    let (k, m) = (k as usize, m as usize);

    let matrix = create_matrix(k as u32, m as u32).ok_or(FecError::Memory)?;

    // Bounds (C liest/schreibt ungeprüft; hier abgefangen statt UB):
    let data_end = if k == 0 { 0 } else { stride * (k - 1) + unit_size };
    let coding_end = unit_size * (k + m);
    if frame_buf.len() < data_end.max(coding_end) {
        return Err(FecError::InvalidData);
    }

    // jerasure_matrix_encode: Coding-Unit i = XOR_j matrix[i][j] * data[j]
    let src_ids: Vec<usize> = (0..k).collect();
    let mut out = vec![0u8; unit_size];
    for (i, row) in matrix.chunks(k).enumerate() {
        matrix_dotprod(row, &src_ids, frame_buf, stride, unit_size, &mut out);
        frame_buf[(k + i) * unit_size..(k + i + 1) * unit_size].copy_from_slice(&out);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// chiaki_fec_decode (fec.c)
// ---------------------------------------------------------------------------

/// Rekonstruiert bis zu m gelöschte Units in-place. `erasures` sind die
/// Indizes der fehlenden Units (0..k = Source, k..k+m = FEC) in beliebiger
/// Reihenfolge; doppelte Einträge sind wie in jerasure erlaubt (werden
/// ignoriert). Alle Units liegen bei `stride * index`.
///
/// C: `chiaki_fec_decode(frame_buf, unit_size, stride, k, m, erasures,
///                       erasures_count)`
pub fn decode(
    frame_buf: &mut [u8],
    unit_size: usize,
    stride: usize,
    k: u32,
    m: u32,
    erasures: &[u32],
) -> Result<()> {
    if stride < unit_size {
        return Err(FecError::InvalidData);
    }
    let (k, m) = (k as usize, m as usize);
    let total = k + m;

    if frame_buf.len() < (total.max(1) - 1) * stride + unit_size {
        return Err(FecError::InvalidData);
    }

    // jerasure_erasures_to_erased: Erasures deduplizieren; wenn weniger als
    // k Units übrig bleiben → nicht dekodierbar (jerasure gibt -1 zurück).
    let mut erased = vec![false; total];
    let mut t_non_erased = total;
    for &e in erasures {
        let e = e as usize;
        if e >= total {
            // C: Indexprüfung fehlt (UB) — hier definiert als FEC_FAILED.
            return Err(FecError::Failed);
        }
        if !erased[e] {
            erased[e] = true;
            t_non_erased -= 1;
            if t_non_erased < k {
                return Err(FecError::Failed);
            }
        }
    }

    let matrix = create_matrix(k as u32, m as u32).ok_or(FecError::Memory)?;

    // jerasure_matrix_decode: Anzahl gelöschter Datenheiten. row_k_ones ist in
    // fec.c immer 0 → lastdrive = k ("if (!row_k_ones || erased[k])").
    let mut edd = 0;
    for &e in erased.iter().take(k) {
        if e {
            edd += 1;
        }
    }
    let lastdrive = k;

    if edd > 0 {
        // jerasure_make_decoding_matrix: dm_ids = die ersten k nicht
        // gelöschten Units; tmpmat = Koeffizientenmatrix von diesen Units
        // (Einheitszeilen für intakte Datenheiten, Coding-Zeilen für FECs).
        let mut dm_ids = Vec::with_capacity(k);
        let mut i = 0;
        while dm_ids.len() < k {
            if !erased[i] {
                dm_ids.push(i);
            }
            i += 1;
        }

        let mut tmpmat = vec![0i32; k * k];
        for (row, &di) in tmpmat.chunks_mut(k).zip(dm_ids.iter()) {
            if di < k {
                row[di] = 1;
            } else {
                row.copy_from_slice(&matrix[(di - k) * k..(di - k) * k + k]);
            }
        }

        let mut decoding_matrix = vec![0i32; k * k];
        if invert_matrix(&mut tmpmat, &mut decoding_matrix, k).is_err() {
            return Err(FecError::Failed);
        }

        // Gelöschte Datenheiten aus den dm_ids-Units rekonstruieren:
        //   for (i = 0; edd > 0 && i < lastdrive; i++)
        // (lastdrive = k ⇒ alle k Datenheiten werden durchlaufen, edd ist
        // danach 0 — der darauf folgende "decode drive lastdrive"-Zweig des
        // C-Codes ist mit row_k_ones=0 unerreichbar und entfällt.)
        let mut out = vec![0u8; unit_size];
        for i in 0..lastdrive {
            if erased[i] {
                matrix_dotprod(
                    &decoding_matrix[i * k..(i + 1) * k],
                    &dm_ids,
                    frame_buf,
                    stride,
                    unit_size,
                    &mut out,
                );
                frame_buf[stride * i..stride * i + unit_size].copy_from_slice(&out);
            }
        }
    }

    // Zuletzt gelöschte FEC-Units neu enkodieren
    let src_ids: Vec<usize> = (0..k).collect();
    let mut out = vec![0u8; unit_size];
    for (i, row) in matrix.chunks(k).enumerate() {
        if erased[k + i] {
            matrix_dotprod(row, &src_ids, frame_buf, stride, unit_size, &mut out);
            frame_buf[stride * (k + i)..stride * (k + i) + unit_size].copy_from_slice(&out);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Unit-Tests (rein Rust; Golden-Vergleiche gegen die C-Bibliothek in
// tests/fec_golden.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// XORShift-PRNG für deterministische Testdaten.
    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn gf_tables_are_consistent() {
        // Körperaxiome: a * b == b * a, a * inv(a) == 1, a / b == a * inv(b)
        let tables = gf_tables();
        for a in 1..=255u8 {
            assert_eq!(tables.mul[a as usize][tables.inv[a as usize] as usize], 1);
            for b in 1..=255u8 {
                assert_eq!(gf_mul(a, b), gf_mul(b, a));
                if a != b {
                    assert_eq!(gf_div(a, b), gf_mul(a, tables.inv[b as usize]));
                }
            }
        }
        assert_eq!(gf_mul(0, 137), 0);
        assert_eq!(gf_div(0, 137), 0);
        assert_eq!(gf_div(137, 0), 0); // wie gf-complete: inverse(0) = 0
    }

    #[test]
    fn cauchy_matrix_matches_formula() {
        // k=6, m=1 (wie im ersten C-Testfall): Zeile 0 = 1/(0 ^ (1+j))
        let matrix = create_matrix(6, 1).unwrap();
        for j in 0..6usize {
            assert_eq!(matrix[j], gf_div(1, (1 + j) as u8) as i32);
        }
        // Mehrere Coding-Zeilen
        let matrix = create_matrix(4, 3).unwrap();
        for i in 0..3usize {
            for j in 0..4usize {
                assert_eq!(matrix[i * 4 + j], gf_div(1, (i ^ (3 + j)) as u8) as i32);
            }
        }
    }

    #[test]
    fn cauchy_matrix_rejects_oversize() {
        assert!(create_matrix(200, 100).is_none()); // k+m > 256
        assert!(create_matrix(128, 128).is_some()); // k+m == 256
    }

    #[test]
    fn invert_matrix_roundtrip() {
        // Untermatrix * Untermatrix^-1 == Einheitsmatrix, mit demselben
        // Untermatrix-Aufbau wie in decode (Erasures = erste e Datenheiten)
        for &(k, m) in &[(4usize, 2usize), (6, 1), (10, 3), (1, 1)] {
            let matrix = create_matrix(k as u32, m as u32).unwrap();
            for e in 0..=m.min(k) {
                let mut dm_ids: Vec<usize> = Vec::with_capacity(k);
                dm_ids.extend(e..k); // intakte Datenheiten
                dm_ids.extend(k..k + e); // FEC-Einheiten statt der gelöschten
                let mut sub = vec![0i32; k * k];
                for (row, &di) in sub.chunks_mut(k).zip(dm_ids.iter()) {
                    if di < k {
                        row[di] = 1;
                    } else {
                        row.copy_from_slice(&matrix[(di - k) * k..(di - k) * k + k]);
                    }
                }
                let mut inv = vec![0i32; k * k];
                let sub_orig = sub.clone();
                invert_matrix(&mut sub, &mut inv, k).unwrap();
                // Produkt Inverse * Untermatrix == Einheit
                for i in 0..k {
                    for j in 0..k {
                        let mut acc = 0u8;
                        for t in 0..k {
                            acc ^= gf_mul(inv[i * k + t] as u8, sub_orig[t * k + j] as u8);
                        }
                        assert_eq!(acc as i32, if i == j { 1 } else { 0 }, "k={k} m={m} e={e}");
                    }
                }
            }
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        // k Source-Units, m FEC-Units, verschiedene Erasure-Muster
        for &(k, m, unit_size, stride) in
            &[(6usize, 1usize, 64usize, 64usize), (4, 3, 48, 64), (16, 4, 128, 160)]
        {
            // Encode im gepackten Layout (stride == unit_size — so schreibt
            // fec.c die FEC-Units hinter die Source-Units).
            let mut packed = vec![0u8; (k + m) * unit_size];
            let mut state = 0x1234 + k as u64;
            for b in packed[..k * unit_size].iter_mut() {
                *b = xorshift(&mut state) as u8;
            }
            encode(&mut packed, unit_size, unit_size, k as u32, m as u32).unwrap();

            // Decode-Layout: alle Units strided
            let mut reference = vec![0u8; stride * (k + m)];
            for u in 0..k + m {
                reference[stride * u..stride * u + unit_size]
                    .copy_from_slice(&packed[u * unit_size..(u + 1) * unit_size]);
            }

            // Bis zu m Units löschen und rekonstruieren
            let mut state = 0x99u64;
            for round in 0..8u64 {
                let mut frame_e = reference.clone();
                let mut erasures: Vec<u32> = Vec::new();
                for _ in 0..(round as usize % (m + 1)) {
                    let e = (xorshift(&mut state) % (k + m) as u64) as usize;
                    if !erasures.contains(&(e as u32)) {
                        erasures.push(e as u32);
                    }
                }
                // Garbage über die Erasures schreiben (wie im C-Test)
                for &e in &erasures {
                    let e = e as usize;
                    for b in &mut frame_e[stride * e..stride * e + unit_size] {
                        *b = 0x42;
                    }
                }
                decode(&mut frame_e, unit_size, stride, k as u32, m as u32, &erasures).unwrap();
                // Alle k+m Units müssen wieder dem Soll entsprechen
                for unit in 0..k + m {
                    assert_eq!(
                        frame_e[stride * unit..stride * unit + unit_size],
                        reference[stride * unit..stride * unit + unit_size],
                        "k={k} m={m} round={round} unit={unit} erasures={erasures:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn decode_errors() {
        let mut frame = vec![0u8; 64 * 8];
        // stride < unit_size
        assert_eq!(decode(&mut frame.clone(), 64, 32, 6, 1, &[]), Err(FecError::InvalidData));
        assert_eq!(encode(&mut frame.clone(), 64, 32, 6, 1), Err(FecError::InvalidData));
        // mehr als m Erasures → weniger als k intakte Units
        let erasures = [0u32, 1, 2, 3, 4, 5, 6];
        assert_eq!(decode(&mut frame.clone(), 64, 64, 6, 1, &erasures), Err(FecError::Failed));
        // Out-of-Bounds-Erasure (in C UB)
        assert_eq!(decode(&mut frame, 64, 64, 6, 1, &[99]), Err(FecError::Failed));
        // frame_buf zu klein
        assert_eq!(encode(&mut vec![0u8; 10], 64, 64, 6, 1), Err(FecError::InvalidData));
    }
}
