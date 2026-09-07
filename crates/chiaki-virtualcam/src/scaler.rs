// SPDX-License-Identifier: AGPL-3.0-only
//! NV12-Hilfsfunktionen für den Kamera-Feed (pure, getestet).
//!
//! Die DecodedFrame-Planes haben NVDEC-/VSR-Strides (1280 breite Rows können
//! z. B. 1280+Padding Bytes belegen); das OBS-Backend erwartet gepacktes
//! NV12 (`width * height * 3 / 2`, Zeilenabstand = width). Für die
//! Kamera-Auflösung „720p/1080p" kleiner als der Stream wird zusätzlich ein
//! Area-Average-Downscaler (Box-Filter, ganzzahlig, ohne FFI) geboten.

/// Kopiert NV12 aus zwei gestrideten Planes (Y + interleaved UV) in einen
/// gepackten Buffer (`out` wird auf `w*h*3/2` gebracht). Fehler, wenn eine
/// gelesene Row über das Ende der Quell-Slice hinausläuft (verhindert,
/// dass ein falscher Stride fremden Speicher liest).
pub fn pack_nv12_strided(
    y: &[u8],
    y_stride: usize,
    uv: &[u8],
    uv_stride: usize,
    w: u32,
    h: u32,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(format!("pack_nv12: ungültige Dimensionen {w}x{h}"));
    }
    fn row(
        buf: &[u8],
        stride: usize,
        row_index: usize,
        width: usize,
    ) -> Result<&[u8], String> {
        let start = stride
            .checked_mul(row_index)
            .ok_or_else(|| "pack_nv12: Stride-Overflow".to_string())?;
        let end = start
            .checked_add(width)
            .ok_or_else(|| "pack_nv12: Row-Ende-Overflow".to_string())?;
        buf.get(start..end).ok_or_else(|| {
            format!(
                "pack_nv12: Row {row_index} liegt außerhalb der Plane (stride {stride}, len {})",
                buf.len()
            )
        })
    }

    out.clear();
    out.resize(w * h * 3 / 2, 0);
    for r in 0..h {
        let src = row(y, y_stride, r, w)?;
        out[r * w..(r + 1) * w].copy_from_slice(src);
    }
    let (ch, cw) = (h / 2, w);
    for r in 0..ch {
        let src = row(uv, uv_stride, r, cw)?;
        out[w * h + r * cw..w * h + (r + 1) * cw].copy_from_slice(src);
    }
    Ok(())
}

/// Area-Average-Downscale eines gepackten NV12-Frames (`sw×sh` → `dw×dh`).
/// Nur Verkleinerung (Upscale = Fehler — der Kamera-Feed fällt dann auf
/// Stream-Auflösung zurück); gerade Dimensionen. Luma und Chroma werden mit
/// demselben Box-Filter verkleinert (Chroma-Ebene ist `w/2 × h/2` groß).
pub fn downscale_nv12(
    src: &[u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    dst: &mut Vec<u8>,
) -> Result<(), String> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    if dw == 0 || dh == 0 || dw > sw || dh > sh || dw % 2 != 0 || dh % 2 != 0 {
        return Err(format!("downscale_nv12: {sw}x{sh} → {dw}x{dh} nicht unterstützt"));
    }
    let need = dw * dh * 3 / 2;
    dst.clear();
    dst.resize(need, 0);

    let (y_len_s, y_len_d) = (sw * sh, dw * dh);
    let src_y = src
        .get(..y_len_s)
        .ok_or_else(|| format!("downscale_nv12: Luma-Plane zu kurz ({} < {y_len_s})", src.len()))?;
    let src_uv = src.get(y_len_s..).ok_or_else(|| {
        format!("downscale_nv12: UV-Plane fehlt (len {}, Y braucht {y_len_s})", src.len())
    })?;

    downscale_plane_stride(src_y, sw, sh, 1, &mut dst[..y_len_d], dw, dh);
    // UV interleaved: w/2 Paare pro Row, 2 Byte je Paar (U,V getrennt mitteln).
    downscale_plane_stride(src_uv, sw / 2, sh / 2, 2, &mut dst[y_len_d..], dw / 2, dh / 2);
    Ok(())
}

/// Box-Filter einer Ebene: jeder Ziel-Pixel mittelt über seinen Quell-
/// Rechteckbereich `[x*sw/dw .. ceil((x+1)*sw/dw))` (Area-Mapping, ganzzahlig).
/// `comps` = Bytes je Pixel (1 = Y-Ebene, 2 = interleaved UV).
fn downscale_plane_stride(
    src: &[u8],
    sw: usize,
    sh: usize,
    comps: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
) {
    for dy in 0..dh {
        let sy0 = dy * sh / dh;
        let sy1 = ((dy + 1) * sh + dh - 1) / dh;
        for dx in 0..dw {
            let sx0 = dx * sw / dw;
            let sx1 = ((dx + 1) * sw + dw - 1) / dw;
            let mut sum = [0u32; 2];
            for sy in sy0..sy1 {
                let line = &src[sy * sw * comps..(sy + 1) * sw * comps];
                for sx in sx0..sx1 {
                    for c in 0..comps {
                        sum[c] += u32::from(line[sx * comps + c]);
                    }
                }
            }
            let count = ((sx1 - sx0) * (sy1 - sy0)) as u32;
            for c in 0..comps {
                dst[(dy * dw + dx) * comps + c] = (sum[c] / count) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_nv12(w: u32, h: u32) -> Vec<u8> {
        let (w, h) = (w as usize, h as usize);
        let mut buf = vec![0u8; w * h * 3 / 2];
        for r in 0..h {
            for c in 0..w {
                // Bewusst OHNE Wrap (min): Area-Mittel von monotonen Werten
                // bleibt monoton — Wrap würde den Monotonie-Test fälschen.
                buf[r * w + c] = (c + r).min(255) as u8;
            }
        }
        for r in 0..h / 2 {
            for c in 0..w {
                buf[w * h + r * w + c] = 128;
            }
        }
        buf
    }

    #[test]
    fn pack_entstrippt_strides() {
        // 4x4 Y mit Stride 8 (Padding-Spalten 255) + 4x2 UV mit Stride 8.
        let mut y = Vec::new();
        for r in 0..4u8 {
            y.extend_from_slice(&[r * 10 + 1, r * 10 + 2, r * 10 + 3, r * 10 + 4, 255, 255, 255, 255]);
        }
        let mut uv = Vec::new();
        for _ in 0..2 {
            uv.extend_from_slice(&[100, 110, 120, 130, 255, 255, 255, 255]);
        }
        let mut out = Vec::new();
        pack_nv12_strided(&y, 8, &uv, 8, 4, 4, &mut out).expect("packbar");
        assert_eq!(out.len(), 4 * 4 * 3 / 2);
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
        assert_eq!(&out[4..8], &[11, 12, 13, 14]);
        // UV: interleaved Paare, gepackt ohne Padding.
        assert_eq!(&out[16..24], &[100, 110, 120, 130, 100, 110, 120, 130]);
    }

    #[test]
    fn pack_fängt_kurze_planes_ab() {
        let y = vec![0u8; 3 * 8]; // letzte Row fehlt bei Stride 8
        let uv = vec![0u8; 2 * 8];
        let mut out = Vec::new();
        assert!(pack_nv12_strided(&y, 8, &uv, 8, 4, 4, &mut out).is_err());
    }

    #[test]
    fn downscale_identisch_und_2x_mittelt() {
        // 2x2 → 2x2 (dw<=sw, unverändert): Y [10,20;30,40], UV neutral.
        let src = vec![10u8, 20, 30, 40, 128, 128];
        let mut dst = Vec::new();
        downscale_nv12(&src, 2, 2, 2, 2, &mut dst).expect("identisch erlaubt");
        assert_eq!(dst, src);

        // 4x4 → 2x2: vier Blöcke mit je 4 gleichen Werten.
        let mut src4 = vec![0u8; 4 * 4 + 4 * 2];
        for (i, v) in [10u8, 20, 30, 40].iter().enumerate() {
            let (r0, c0) = ((i / 2) * 2, (i % 2) * 2);
            for r in r0..r0 + 2 {
                for c in c0..c0 + 2 {
                    src4[r * 4 + c] = *v;
                }
            }
        }
        // UV neutral.
        for px in src4[16..].iter_mut() {
            *px = 128;
        }
        let mut dst4 = Vec::new();
        downscale_nv12(&src4, 4, 4, 2, 2, &mut dst4).expect("4x4→2x2");
        assert_eq!(&dst4[..4], &[10, 20, 30, 40], "Blöcke gemittelt");
        assert!(dst4[4..].iter().all(|&p| p == 128), "UV neutral");
    }

    #[test]
    fn downscale_1080_zu_720_erhaelt_verlauf_und_neutrales_chroma() {
        let src = gradient_nv12(1920, 1080);
        let mut dst = Vec::new();
        downscale_nv12(&src, 1920, 1080, 1280, 720, &mut dst).expect("1080p→720p");
        assert_eq!(dst.len(), 1280 * 720 * 3 / 2);
        // Verlauf bleibt monoton steigend über eine Ziel-Row.
        let row: Vec<u8> = dst[..1280].to_vec();
        assert!(row.windows(2).all(|w| w[1] >= w[0]), "Zeile monotone (Verlauf)");
        // UV-Plane bleibt 128 (Quelle neutral).
        assert!(dst[1280 * 720..].iter().all(|&p| p == 128));
    }

    #[test]
    fn downscale_lehnt_upscale_ab() {
        let src = gradient_nv12(640, 360);
        let mut dst = Vec::new();
        assert!(downscale_nv12(&src, 640, 360, 1280, 720, &mut dst).is_err());
    }
}
