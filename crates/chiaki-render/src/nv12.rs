//! NV12→BGRA8-Konvertierung (BT.601, limited range — wie Standard-Video).
//!
//! NV12-Layout: Y-Plane (`height` Zeilen, `y_stride` Bytes je Zeile) gefolgt von
//! der interleaved UV-Plane (`height/2` Zeilen, `uv_stride` Bytes je Zeile,
//! pro 2x2-Block ein U- plus ein V-Byte). Das ist der Ausgabevertrag des
//! chiaki-media-Decoders (NVDEC/FFmpeg, `DecodedFrame` → Y+UV-Planes).
//!
//! Die Konvertierung ist bewusst eine schlichte Single-Thread-Implementierung
//! mit Fixed-Point-Koeffizienten und Luma-LUT: 1080p60 entspricht ~250 MB/s
//! Durchsatz — ob das unter 2 ms bleibt, misst Spike S1 (siehe
//! `spike-s1-results.md`). Bei 4K (33 MB/Frame) wird der gemessene Wert
//! berichten, ob Parallelisierung (rayon) nötig wird.

use crate::{Error, Result};

// BT.601 limited-range Koeffizienten, festkomma-skaliert mit 1/1024:
//   R = 1.164*(Y-16)              + 1.596*(V-128)
//   G = 1.164*(Y-16) - 0.392*(U-128) - 0.813*(V-128)
//   B = 1.164*(Y-16) + 2.017*(U-128)
const CY: i32 = 1192; // 1.164
const CRV: i32 = 1634; // 1.596
const CGU: i32 = 401; // 0.392
const CGV: i32 = 833; // 0.813
const CBU: i32 = 2066; // 2.017

/// Luma-Beitrag je Y-Byte: `CY * (y - 16)`.
static Y_LUT: [i32; 256] = {
    let mut table = [0i32; 256];
    let mut i = 0usize;
    while i < 256 {
        table[i] = CY * (i as i32 - 16);
        i += 1;
    }
    table
};

/// Ein NV12-Frame im RAM: Y-Plane gefolgt von UV-Plane in einem Puffer.
///
/// Für den Spike von einem Testpattern-Thread erzeugt; später füllt der
/// chiaki-media-Decoder-Thread denselben Typ (Kopie aus den NVDEC-Planes,
/// mit aligned_height-Korrektur).
#[derive(Debug, Clone)]
pub struct NV12Frame {
    pub width: u32,
    pub height: u32,
    /// Zeilenabstand der Y-Plane in Bytes.
    pub y_stride: usize,
    /// Zeilenabstand der UV-Plane in Bytes.
    pub uv_stride: usize,
    /// `y_stride * height` Bytes Y, danach `uv_stride * (height / 2)` Bytes UV.
    pub data: Vec<u8>,
}

impl NV12Frame {
    /// Frischer, leerer (nullgesetzter) Frame mit `stride == width`.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::with_strides(width, height, width as usize, width as usize)
    }

    /// Frischer, leerer Frame mit expliziten Strides (für aligned-height-Layouts).
    /// Zero-Copy-Konstruktor aus einem fremden, bereits NV12-layouteten
    /// Puffer (z. B. VSR-Output: Y@0 + UV@y_stride*h, uv_stride == y_stride).
    /// `data` wird ÜBERNOMMEN (kein Kopieren). Validiert die Länge.
    pub fn from_parts(
        width: u32,
        height: u32,
        y_stride: usize,
        uv_stride: usize,
        data: Vec<u8>,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::InvalidDimensions { width, height });
        }
        let y_len = y_stride
            .checked_mul(height as usize)
            .ok_or(Error::InvalidDimensions { width, height })?;
        let uv_len = uv_stride
            .checked_mul(height as usize / 2)
            .ok_or(Error::InvalidDimensions { width, height })?;
        if data.len() < y_len + uv_len {
            return Err(Error::InvalidDimensions { width, height });
        }
        Ok(Self { width, height, y_stride, uv_stride, data })
    }

    pub fn with_strides(
        width: u32,
        height: u32,
        y_stride: usize,
        uv_stride: usize,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::InvalidDimensions { width, height });
        }
        let y_len = y_stride
            .checked_mul(height as usize)
            .ok_or(Error::InvalidDimensions { width, height })?;
        let uv_len = uv_stride
            .checked_mul(height as usize / 2)
            .ok_or(Error::InvalidDimensions { width, height })?;
        Ok(Self {
            width,
            height,
            y_stride,
            uv_stride,
            data: vec![0u8; y_len + uv_len],
        })
    }

    fn plane_sizes(&self) -> (usize, usize) {
        let y_len = self.y_stride * self.height as usize;
        let uv_len = self.uv_stride * (self.height as usize / 2);
        (y_len, uv_len)
    }

    /// Y-Plane-Slice (`height` Zeilen à `y_stride` Bytes).
    pub fn y_plane(&self) -> &[u8] {
        let (y_len, _) = self.plane_sizes();
        &self.data[..y_len]
    }

    /// Interleaved UV-Plane-Slice (`height/2` Zeilen à `uv_stride` Bytes, U vor V).
    pub fn uv_plane(&self) -> &[u8] {
        let (y_len, uv_len) = self.plane_sizes();
        &self.data[y_len..y_len + uv_len]
    }

    /// Benötigte Zielgröße in Bytes für BGRA-Ausgabe.
    pub fn bgra_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// Konvertiert diesen Frame nach BGRA in `dst` (siehe [`nv12_to_bgra`]).
    pub fn to_bgra(&self, dst: &mut [u8]) -> Result<()> {
        nv12_to_bgra(
            self.y_plane(),
            self.y_stride,
            self.uv_plane(),
            self.uv_stride,
            self.width,
            self.height,
            dst,
        )
    }
}

/// Geprüfte Parameter für die Konvertierung (einmal validieren, in Bands teilen).
struct Layout<'a> {
    src_y: &'a [u8],
    src_uv: &'a [u8],
    w: usize,
    pairs: usize, // Anzahl 2x2-Zeilenpaare = height / 2
    y_stride: usize,
    uv_stride: usize,
    dst_row: usize,
}

fn validate<'a>(
    src_y: &'a [u8],
    y_stride: usize,
    src_uv: &'a [u8],
    uv_stride: usize,
    width: u32,
    height: u32,
    dst_len: usize,
) -> Result<Layout<'a>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(Error::InvalidDimensions { width, height });
    }
    let need_y = y_stride * h;
    let need_uv = uv_stride * (h / 2);
    let need_dst = w * h * 4;
    if src_y.len() < need_y {
        return Err(Error::SourceTooSmall { need: need_y, have: src_y.len() });
    }
    if src_uv.len() < need_uv {
        return Err(Error::SourceTooSmall { need: need_uv, have: src_uv.len() });
    }
    if dst_len < need_dst {
        return Err(Error::DestinationTooSmall { need: need_dst, have: dst_len });
    }
    Ok(Layout { src_y, src_uv, w, pairs: h / 2, y_stride, uv_stride, dst_row: w * 4 })
}

/// Konvertiert `band_pairs` 2x2-Zeilenpaare ab Paar-Index `first` in `dst_band`.
fn convert_band(layout: &Layout, first: usize, band_pairs: usize, dst_band: &mut [u8]) {
    let Layout { src_y, src_uv, w, y_stride, uv_stride, dst_row, .. } = *layout;
    for pair in 0..band_pairs {
        let uv_row = first + pair;
        let y_row0 = 2 * uv_row;
        let y0 = &src_y[y_row0 * y_stride..y_row0 * y_stride + w];
        let y1 = &src_y[(y_row0 + 1) * y_stride..(y_row0 + 1) * y_stride + w];
        let uv = &src_uv[uv_row * uv_stride..uv_row * uv_stride + w];
        let (d0, d1) = dst_band[pair * 2 * dst_row..(pair * 2 + 2) * dst_row].split_at_mut(dst_row);

        // Zeilenweise je 2 Pixel verarbeiten: Chroma gilt pro 2x2-Block,
        // Luma per Pixel. chunks_exact/zips halten die Bounds-Checks
        // pro Zeile konstant statt pro Pixel.
        let mut y0c = y0.chunks_exact(2);
        let mut y1c = y1.chunks_exact(2);
        let mut uvc = uv.chunks_exact(2);
        let mut d0c = d0.chunks_exact_mut(8);
        let mut d1c = d1.chunks_exact_mut(8);
        for ((((uv2, y0_pair), y1_pair), d0_pair), d1_pair) in
            uvc.by_ref().zip(y0c.by_ref()).zip(y1c.by_ref()).zip(d0c.by_ref()).zip(d1c.by_ref())
        {
            let u = uv2[0] as i32 - 128;
            let v = uv2[1] as i32 - 128;
            // Chroma-Beitrag ist für den 2x2-Block konstant.
            let cr = CRV * v;
            let cg = -(CGU * u) - CGV * v;
            let cb = CBU * u;

            put_pixel(d0_pair, Y_LUT[y0_pair[0] as usize], cr, cg, cb);
            put_pixel(&mut d0_pair[4..], Y_LUT[y0_pair[1] as usize], cr, cg, cb);
            put_pixel(d1_pair, Y_LUT[y1_pair[0] as usize], cr, cg, cb);
            put_pixel(&mut d1_pair[4..], Y_LUT[y1_pair[1] as usize], cr, cg, cb);
        }
    }
}

/// Konvertiert ein NV12-Bild (BT.601 limited range) nach BGRA8, single-thread.
///
/// * `src_y` — Y-Plane, `height` Zeilen à `y_stride` Bytes.
/// * `src_uv` — interleaved UV-Plane, `height/2` Zeilen à `uv_stride` Bytes (U,V je 2x2-Block).
/// * `dst` — Ausgabe, `width * height * 4` Bytes, 4 Bytes je Pixel in B,G,R,A-Reihenfolge
///   (GPUI lädt Polychrom-Texturen als `DXGI_FORMAT_B8G8R8A8_UNORM` — die Bytes
///   liegen also direkt in der Reihenfolge, die die GPU erwartet).
pub fn nv12_to_bgra(
    src_y: &[u8],
    y_stride: usize,
    src_uv: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
) -> Result<()> {
    let layout = validate(src_y, y_stride, src_uv, uv_stride, width, height, dst.len())?;
    convert_band(&layout, 0, layout.pairs, &mut dst[..layout.pairs * 2 * layout.dst_row]);
    Ok(())
}

/// Wie [`nv12_to_bgra`], aber die Zeilen werden auf `workers` Threads verteilt
/// (`std::thread::scope`, kein Blocken, keine Extra-Dependencies).
///
/// Sinnvoll, wenn die Single-Thread-Konvertierung zum Flaschenhals wird
/// (1080p ≈ 3,1 ms gemessen; 4K ≈ 4x). `workers == 1` ist ein direkter Aufruf
/// des Single-Thread-Pfads; `workers == 0` wird zu 1.
pub fn nv12_to_bgra_parallel(
    src_y: &[u8],
    y_stride: usize,
    src_uv: &[u8],
    uv_stride: usize,
    width: u32,
    height: u32,
    dst: &mut [u8],
    workers: usize,
) -> Result<()> {
    let layout = validate(src_y, y_stride, src_uv, uv_stride, width, height, dst.len())?;
    let workers = workers.max(1).min(layout.pairs);
    if workers == 1 {
        convert_band(&layout, 0, layout.pairs, &mut dst[..layout.pairs * 2 * layout.dst_row]);
        return Ok(());
    }

    let band = layout.pairs.div_ceil(workers);
    let total_bytes = layout.pairs * 2 * layout.dst_row;
    std::thread::scope(|scope| {
        let layout = &layout;
        let mut rest = &mut dst[..total_bytes];
        let mut first = 0usize;
        for _ in 0..workers {
            if first >= layout.pairs {
                break;
            }
            let pairs = band.min(layout.pairs - first);
            let bytes = pairs * 2 * layout.dst_row;
            let (chunk, tail) = rest.split_at_mut(bytes);
            rest = tail;
            scope.spawn(move || convert_band(layout, first, pairs, chunk));
            first += pairs;
        }
    });
    Ok(())
}

/// Schreibt ein BGRA-Pixel (B, G, R, 0xFF) aus Luma- und Chroma-Beiträgen.
#[inline(always)]
fn put_pixel(dst: &mut [u8], y_luma: i32, cr: i32, cg: i32, cb: i32) {
    dst[0] = ((y_luma + cb) >> 10).clamp(0, 255) as u8;
    dst[1] = ((y_luma + cg) >> 10).clamp(0, 255) as u8;
    dst[2] = ((y_luma + cr) >> 10).clamp(0, 255) as u8;
    dst[3] = 0xFF;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x4-Testpattern 1: Grau-Schachbrett aus Y=235 (Weißreferenz) und Y=16
    /// (Schwarzpegel), neutrales Chroma (U=V=128).
    #[test]
    fn gray_checkerboard_4x4() {
        // Y-Plane:
        // 235  16 235  16
        //  16 235  16 235
        // 235 235  16  16
        //  16  16 235 235
        let y: [u8; 16] = [
            235, 16, 235, 16, //
            16, 235, 16, 235, //
            235, 235, 16, 16, //
            16, 16, 235, 235,
        ];
        // UV-Plane: 2 Zeilen à 4 Bytes (2 U/V-Paare je Zeile), alles neutral.
        let uv: [u8; 8] = [128, 128, 128, 128, 128, 128, 128, 128];

        let mut dst = [0u8; 4 * 4 * 4];
        nv12_to_bgra(&y, 4, &uv, 4, 4, 4, &mut dst).unwrap();

        // Y=235: 1192*(235-16) = 261048, >> 10 = 254 → Grau (254, 254, 254).
        // Y=16:  0 → Schwarz (0, 0, 0). Alpha immer 255.
        let white = [254u8, 254, 254, 255];
        let black = [0u8, 0, 0, 255];
        let expected: [u8; 64] = [
            white, black, white, black, //
            black, white, black, white, //
            white, white, black, black, //
            black, black, white, white,
        ]
        .iter()
        .flat_map(|px| px.iter().copied())
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
        assert_eq!(dst, expected);
    }

    /// 4x4-Testpattern 2: durchgängiges Y=128 mit farbigem Chroma U=16, V=240.
    /// Golden-Vektor (handgerechnet aus denselben Fixed-Point-Formeln):
    ///   u = 16-128 = -112, v = 240-128 = 112
    ///   cr = 1634*112 = 183008, cg = -(401*(-112)) - 833*112 = -48384,
    ///   cb = 2066*(-112) = -231392, y_luma = 1192*112 = 133504
    ///   R = (133504+183008) >> 10 = 309 → 255 (clamp)
    ///   G = (133504-48384)  >> 10 = 83
    ///   B = (133504-231392) >> 10 = -96 → 0 (clamp)
    #[test]
    fn colored_frame_4x4_golden() {
        let y = [128u8; 16];
        // Zeile je 2 Blöcke: (U=16, V=240) zweimal, zwei Zeilen.
        let uv = [16u8, 240, 16, 240, 16, 240, 16, 240];
        let mut dst = [0u8; 64];
        nv12_to_bgra(&y, 4, &uv, 4, 4, 4, &mut dst).unwrap();

        let expected_px = [0u8, 83, 255, 255]; // BGRA
        let expected = [expected_px; 16]
            .iter()
            .flat_map(|px| px.iter().copied())
            .collect::<Vec<u8>>();
        assert_eq!(dst.as_slice(), expected.as_slice());
    }

    /// Strides größer als Breite: Padding-Bytes dürfen nicht einfließen.
    #[test]
    fn strides_with_padding_2x2() {
        // 2x2 Bild, y_stride = 4 (2 Padding-Bytes je Zeile), uv_stride = 4.
        let y = [235u8, 235, 0xAA, 0xAA, 16u8, 16, 0xAA, 0xAA];
        let uv = [128u8, 128, 0xAA, 0xAA];
        let mut dst = [0u8; 2 * 2 * 4];
        nv12_to_bgra(&y, 4, &uv, 4, 2, 2, &mut dst).unwrap();

        assert_eq!(&dst[..4], &[254, 254, 254, 255]);
        assert_eq!(&dst[4..8], &[254, 254, 254, 255]);
        assert_eq!(&dst[8..12], &[0, 0, 0, 255]);
        assert_eq!(&dst[12..16], &[0, 0, 0, 255]);
    }

    /// NV12Frame-Hilfstyp: Layout, Plane-Zugriff und to_bgra-Rückreise.
    #[test]
    fn nv12_frame_layout_and_conversion() {
        let mut frame = NV12Frame::new(4, 4).unwrap();
        assert_eq!(frame.y_plane().len(), 16);
        assert_eq!(frame.uv_plane().len(), 8);
        assert_eq!(frame.bgra_len(), 64);

        // Weißes Bild: Y=235 überall, neutrales Chroma.
        {
            let (y_len, _) = frame.plane_sizes();
            frame.data[..y_len].iter_mut().for_each(|b| *b = 235);
            frame.data[y_len..].iter_mut().for_each(|b| *b = 128);
        }
        let mut dst = vec![0u8; frame.bgra_len()];
        frame.to_bgra(&mut dst).unwrap();
        assert!(dst.chunks_exact(4).all(|px| px == [254, 254, 254, 255]));
    }

    /// Fehlerfälle: ungerade Dimensionen und zu kleine Puffer.
    #[test]
    fn error_cases() {
        assert!(NV12Frame::new(3, 4).is_err());
        assert!(NV12Frame::new(0, 4).is_err());

        let y = [128u8; 8];
        let uv = [128u8; 4];
        let mut dst = [0u8; 2 * 2 * 4 - 1]; // 1 Byte zu klein
        let err = nv12_to_bgra(&y, 4, &uv, 4, 2, 2, &mut dst).unwrap_err();
        assert!(matches!(err, Error::DestinationTooSmall { .. }));

        let mut dst = [0u8; 2 * 2 * 4];
        let err = nv12_to_bgra(&y[..4], 4, &uv, 4, 2, 2, &mut dst).unwrap_err();
        assert!(matches!(err, Error::SourceTooSmall { .. }));
    }

    /// Parallel-Pfad muss byteidentisch zum Single-Thread-Pfad sein,
    /// auch bei unaufteilbarer Band-Anzahl (16 Paare auf 5 Worker).
    #[test]
    fn parallel_matches_serial() {
        // Deterministisches Pseudo-Rauschen als Pattern (16x16).
        let w = 16usize;
        let h = 16usize;
        let mut y = [0u8; 16 * 16];
        let mut uv = [0u8; 16 * 8];
        for (i, b) in y.iter_mut().enumerate() {
            *b = (i * 37 % 251) as u8;
        }
        for (i, b) in uv.iter_mut().enumerate() {
            *b = (i * 53 % 249) as u8;
        }

        let mut serial = vec![0u8; w * h * 4];
        nv12_to_bgra(&y, w, &uv, w, w as u32, h as u32, &mut serial).unwrap();

        for workers in [1usize, 2, 3, 5, 64] {
            let mut par = vec![0u8; w * h * 4];
            nv12_to_bgra_parallel(&y, w, &uv, w, w as u32, h as u32, &mut par, workers).unwrap();
            assert_eq!(serial, par, "workers = {workers}");
        }
    }
}
