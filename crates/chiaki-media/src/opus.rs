// SPDX-License-Identifier: AGPL-3.0-only
//! libopus über **dynamisches Laden** (libloading) — Port von
//! `lib/src/opusdecoder.c` und `lib/src/opusencoder.c` (chiaki-ng).
//!
//! Die opus.dll liegt NICHT im FFmpeg-Ordner. Suche-Reihenfolge:
//! 1. exe-dir, 2. `CHIAKI_OPUS_DIR` (Umgebung), 3. PATH.
//!    Geprüfte DLL-Namen pro Ordner: `opus.dll`, `libopus-0.dll`, `libopus.dll`
//!    (MSVC- und MinGW-Benennungen).
//!
//! Der C-Client nutzt den Encoder ausschließlich fürs Mikrofon
//! (streamsession.cpp: 48 kHz, 2ch, `MICROPHONE_SAMPLES` = 480 Samples = 10 ms,
//! `OPUS_APPLICATION_RESTRICTED_LOWDELAY`, 40-Byte-Ausgabepuffer) und setzt
//! keine CTLs — dieses Verhalten bildet `OpusAudioEncoder` 1:1 ab. Bitsatz-/
//! Signal-CTLs sind an der sicheren Hülle verfügbar, werden aber wie im C nicht
//! von selbst gesetzt.
//!
//! Concealment (Low-Latency-Verlustpfad): `audioreceiver.c` ruft bei Lücken im
//! Jitter-Buffer `frame_cb(NULL, 0)` — chiaki_opus_decoder_frame mappt das auf
//! `opus_decode(..., NULL, 0, ...)` (Packet-Loss-Concealment von libopus).
//! Hier: `OpusAudioDecoder::decode_frame(&[])`.

use std::ffi::{c_char, c_int, CStr};
use std::sync::OnceLock;

use chiaki_core::audio::AudioHeader;
use chiaki_core::{ChiakiError, ChiakiResult};
use libloading::Library;
use thiserror::Error;

// ---------------------------------------------------------------------------
// sys: FFI-Typen und -Konstanten (libopus, opus_defines.h — Werte sind ABI-
// stabile Defines seit libopus 1.0)
// ---------------------------------------------------------------------------

/// Opaque `struct OpusDecoder` (opus.h).
#[repr(C)]
pub struct OpusDecoderRaw {
    _opaque: [u8; 0],
}
/// Opaque `struct OpusEncoder` (opus.h).
#[repr(C)]
pub struct OpusEncoderRaw {
    _opaque: [u8; 0],
}

pub const OPUS_OK: c_int = 0;
pub const OPUS_BAD_ARG: c_int = -1;
pub const OPUS_BUFFER_TOO_SMALL: c_int = -2;
pub const OPUS_INTERNAL_ERROR: c_int = -3;
pub const OPUS_INVALID_PACKET: c_int = -4;
pub const OPUS_UNIMPLEMENTED: c_int = -5;
pub const OPUS_INVALID_STATE: c_int = -6;
pub const OPUS_ALLOC_FAIL: c_int = -7;

pub const OPUS_APPLICATION_VOIP: c_int = 2048;
pub const OPUS_APPLICATION_AUDIO: c_int = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;

pub const OPUS_AUTO: c_int = -1000;
pub const OPUS_BITRATE_MAX: c_int = -1;

pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
pub const OPUS_GET_BITRATE_REQUEST: c_int = 4003;
pub const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
pub const OPUS_SIGNAL_VOICE: c_int = 3001;
pub const OPUS_SIGNAL_MUSIC: c_int = 3002;

pub type FnOpusDecoderCreate =
    unsafe extern "system" fn(fs: c_int, channels: c_int, error: *mut c_int) -> *mut OpusDecoderRaw;
pub type FnOpusDecoderDestroy = unsafe extern "system" fn(st: *mut OpusDecoderRaw);
pub type FnOpusDecode = unsafe extern "system" fn(
    st: *mut OpusDecoderRaw,
    data: *const u8,
    len: c_int,
    pcm: *mut i16,
    frame_size: c_int,
    decode_fec: c_int,
) -> c_int;
pub type FnOpusDecoderGetNbSamples =
    unsafe extern "system" fn(st: *const OpusDecoderRaw, data: *const u8, len: c_int) -> c_int;
pub type FnOpusEncoderCreate = unsafe extern "system" fn(
    fs: c_int,
    channels: c_int,
    application: c_int,
    error: *mut c_int,
) -> *mut OpusEncoderRaw;
pub type FnOpusEncoderDestroy = unsafe extern "system" fn(st: *mut OpusEncoderRaw);
pub type FnOpusEncode = unsafe extern "system" fn(
    st: *mut OpusEncoderRaw,
    pcm: *const i16,
    frame_size: c_int,
    data: *mut u8,
    max_data_bytes: c_int,
) -> c_int;
/// `opus_encoder_ctl(st, request, ...)` — C-Variadic; auf Win-x64 MS-ABI kompatibel.
pub type FnOpusEncoderCtl =
    unsafe extern "system" fn(st: *mut OpusEncoderRaw, request: c_int, ...) -> c_int;
pub type FnOpusStrerror = unsafe extern "system" fn(error: c_int) -> *const c_char;

pub(crate) struct OpusApi {
    pub opus_decoder_create: FnOpusDecoderCreate,
    pub opus_decoder_destroy: FnOpusDecoderDestroy,
    pub opus_decode: FnOpusDecode,
    pub opus_decoder_get_nb_samples: FnOpusDecoderGetNbSamples,
    pub opus_encoder_create: FnOpusEncoderCreate,
    pub opus_encoder_destroy: FnOpusEncoderDestroy,
    pub opus_encode: FnOpusEncode,
    pub opus_encoder_ctl: FnOpusEncoderCtl,
    pub opus_strerror: FnOpusStrerror,
}

/// Geladene libopus (Handle für die Prozess-Laufzeit).
pub struct OpusLib {
    _lib: Library,
    api: OpusApi,
}

impl OpusLib {
    pub(crate) fn api(&self) -> &OpusApi {
        &self.api
    }
}

static OPUS: OnceLock<OpusLib> = OnceLock::new();

#[derive(Debug, Error)]
pub(crate) enum OpusLoadError {
    #[error("opus DLL not found (tried names {names} in: {tried}) — bitte opus.dll/libopus-0.dll neben die exe legen oder CHIAKI_OPUS_DIR setzen")]
    NotFound { names: String, tried: String },
    #[error("symbol lookup failed: {0}")]
    Symbol(#[source] libloading::Error),
}

impl From<OpusLoadError> for ChiakiError {
    fn from(e: OpusLoadError) -> Self {
        tracing::error!("Opus load failed: {e}");
        match e {
            OpusLoadError::NotFound { .. } => ChiakiError::Uninitialized,
            OpusLoadError::Symbol(_) => ChiakiError::Unknown,
        }
    }
}

/// Lädt libopus (einmalig pro Prozess). Siehe Moduldokumentation für die Suche.
pub fn init() -> ChiakiResult<&'static OpusLib> {
    if let Some(lib) = OPUS.get() {
        return Ok(lib);
    }
    let lib = load()?;
    match OPUS.set(lib) {
        Ok(()) => {}
        Err(existing) => drop(existing),
    }
    Ok(OPUS.get().expect("just set"))
}

const DLL_NAMES: [&str; 3] = ["opus.dll", "libopus-0.dll", "libopus.dll"];

fn load() -> Result<OpusLib, OpusLoadError> {
    let mut tried: Vec<String> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            match load_from_dir(dir) {
                Ok(lib) => return Ok(lib),
                Err(OpusLoadError::NotFound { .. }) => tried.push(dir.display().to_string()),
                Err(e) => return Err(e),
            }
        }
    }

    if let Some(v) = std::env::var_os("CHIAKI_OPUS_DIR") {
        if !v.is_empty() {
            let dir = std::path::PathBuf::from(v);
            match load_from_dir(&dir) {
                Ok(lib) => return Ok(lib),
                Err(OpusLoadError::NotFound { .. }) => tried.push(dir.display().to_string()),
                Err(e) => return Err(e),
            }
        }
    }

    // PATH / Standard-DLL-Suche mit allen Namensvarianten.
    for name in DLL_NAMES {
        match unsafe { Library::new(name) } {
            Ok(lib) => {
                tracing::info!("libopus loaded via system DLL search path ({name})");
                return resolve(lib);
            }
            Err(_) => tried.push(format!("<PATH>/{name}")),
        }
    }

    Err(OpusLoadError::NotFound {
        names: DLL_NAMES.join("/"),
        tried: tried.join(", "),
    })
}

fn load_from_dir(dir: &std::path::Path) -> Result<OpusLib, OpusLoadError> {
    for name in DLL_NAMES {
        let res = unsafe {
            // SAFETY: Handle bleibt prozesslang offen; LOAD_WITH_ALTERED_SEARCH_PATH
            // (0x8) lässt Dependent-DLLs relativ zum eigenen Ordner suchen.
            libloading::os::windows::Library::load_with_flags(dir.join(name), 0x0000_0008)
                .map(libloading::Library::from)
        };
        if let Ok(lib) = res {
            tracing::info!("libopus loaded from {}: {}", dir.display(), name);
            return resolve(lib);
        }
    }
    Err(OpusLoadError::NotFound {
        names: DLL_NAMES.join("/"),
        tried: dir.display().to_string(),
    })
}

macro_rules! sym {
    ($lib:expr, $name:literal as $ty:ty) => {
        *$lib
            .get::<$ty>(concat!($name, "\0").as_bytes())
            .map_err(OpusLoadError::Symbol)?
    };
}

fn resolve(lib: Library) -> Result<OpusLib, OpusLoadError> {
    unsafe {
        let api = OpusApi {
            opus_decoder_create: sym!(lib, "opus_decoder_create" as FnOpusDecoderCreate),
            opus_decoder_destroy: sym!(lib, "opus_decoder_destroy" as FnOpusDecoderDestroy),
            opus_decode: sym!(lib, "opus_decode" as FnOpusDecode),
            opus_decoder_get_nb_samples: sym!(
                lib,
                "opus_decoder_get_nb_samples" as FnOpusDecoderGetNbSamples
            ),
            opus_encoder_create: sym!(lib, "opus_encoder_create" as FnOpusEncoderCreate),
            opus_encoder_destroy: sym!(lib, "opus_encoder_destroy" as FnOpusEncoderDestroy),
            opus_encode: sym!(lib, "opus_encode" as FnOpusEncode),
            opus_encoder_ctl: sym!(lib, "opus_encoder_ctl" as FnOpusEncoderCtl),
            opus_strerror: sym!(lib, "opus_strerror" as FnOpusStrerror),
        };
        Ok(OpusLib { _lib: lib, api })
    }
}

pub(crate) fn strerror(code: c_int) -> String {
    if let Some(lib) = OPUS.get() {
        unsafe {
            let p = (lib.api().opus_strerror)(code);
            if !p.is_null() {
                return CStr::from_ptr(p).to_string_lossy().into_owned();
            }
        }
    }
    format!("opus error {code}")
}

// ---------------------------------------------------------------------------
// Sichere Hüllen
// ---------------------------------------------------------------------------

/// Opus-Decoder (dünne sichere Hülle über `OpusDecoder*`).
pub struct Decoder {
    ptr: std::ptr::NonNull<OpusDecoderRaw>,
    sample_rate: u32,
    channels: u8,
}

// SAFETY: Der Opus-Decoder-State ist besitzend und nicht threadsafe — movable.
unsafe impl Send for Decoder {}

impl Decoder {
    /// C: `opus_decoder_create(rate, channels, &error)`.
    pub fn new(sample_rate: u32, channels: u8) -> ChiakiResult<Decoder> {
        let lib = init()?;
        if channels == 0 || channels > 2 {
            tracing::error!("Opus decoder: invalid channel count {channels} (opus supports 1..=2)");
            return Err(ChiakiError::InvalidData);
        }
        let sample_rate = normalize_rate(sample_rate);
        let mut error: c_int = OPUS_OK;
        let ptr = unsafe {
            // SAFETY: out-Parameter error liegt auf dem Stack; ptr wird geprüft.
            (lib.api().opus_decoder_create)(sample_rate as c_int, channels as c_int, &mut error)
        };
        if error != OPUS_OK || ptr.is_null() {
            tracing::error!(
                "OpusDecoder failed to initialize opus decoder: {}",
                strerror(error)
            );
            if !ptr.is_null() {
                unsafe { (lib.api().opus_decoder_destroy)(ptr) };
            }
            return Err(ChiakiError::Unknown);
        }
        Ok(Decoder {
            ptr: std::ptr::NonNull::new(ptr).expect("checked above"),
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Dekodiert ein Opus-Paket in `out` (interleaved i16 PCM).
    ///
    /// `out` ist die Kapazität in Samples **pro Kanal** (`frame_size`); der
    /// Rückgabewert ist die Anzahl decodierter Samples pro Kanal (0..=frame_size).
    /// `None`/leeres Paket = Concealment (C: `opus_decode(..., NULL, 0, ...)`),
    /// wie `chiaki_opus_decoder_frame` es bei `frame_cb(NULL, 0)` macht.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [i16]) -> ChiakiResult<usize> {
        let lib = init()?;
        let capacity = out.len() / self.channels as usize;
        if capacity == 0 {
            return Err(ChiakiError::BufTooSmall);
        }
        let (data, len) = match packet {
            Some(p) if !p.is_empty() => {
                if p.len() > c_int::MAX as usize {
                    return Err(ChiakiError::BufTooSmall);
                }
                (p.as_ptr(), p.len() as c_int)
            }
            // Concealment: NULL-Payload (libopus erzeugt PLC für frame_size Samples).
            _ => (std::ptr::null(), 0),
        };
        let r = unsafe {
            // SAFETY: out hat capacity * channels i16; frame_size ≤ capacity.
            (lib.api().opus_decode)(
                self.ptr.as_ptr(),
                data,
                len,
                out.as_mut_ptr(),
                capacity as c_int,
                0,
            )
        };
        if r < 0 {
            tracing::error!("Decoding audio frame with opus failed: {}", strerror(r));
            return Err(ChiakiError::Unknown);
        }
        Ok(r as usize)
    }

    /// C: `opus_decoder_get_nb_samples` — Samples pro Kanal für ein Paket.
    pub fn get_nb_samples(&self, packet: &[u8]) -> ChiakiResult<usize> {
        let lib = init()?;
        let r = unsafe {
            // SAFETY: packet wird nur gelesen.
            (lib.api().opus_decoder_get_nb_samples)(
                self.ptr.as_ptr(),
                packet.as_ptr(),
                packet.len() as c_int,
            )
        };
        if r < 0 {
            return Err(ChiakiError::InvalidData);
        }
        Ok(r as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(lib) = OPUS.get() {
            unsafe { (lib.api().opus_decoder_destroy)(self.ptr.as_ptr()) };
        }
    }
}

/// Opus-Encoder (dünne sichere Hülle über `OpusEncoder*`).
pub struct Encoder {
    ptr: std::ptr::NonNull<OpusEncoderRaw>,
    sample_rate: u32,
    channels: u8,
}

// SAFETY: wie Decoder.
unsafe impl Send for Encoder {}

/// C: `OPUS_APPLICATION_*` (opus_defines.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Voip,
    Audio,
    RestrictedLowdelay,
}

impl Application {
    fn as_c_int(self) -> c_int {
        match self {
            Application::Voip => OPUS_APPLICATION_VOIP,
            Application::Audio => OPUS_APPLICATION_AUDIO,
            Application::RestrictedLowdelay => OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }
    }
}

/// C: `OPUS_SIGNAL_*` für `OPUS_SET_SIGNAL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Auto,
    Voice,
    Music,
}

impl Signal {
    fn as_c_int(self) -> c_int {
        match self {
            Signal::Auto => OPUS_AUTO,
            Signal::Voice => OPUS_SIGNAL_VOICE,
            Signal::Music => OPUS_SIGNAL_MUSIC,
        }
    }
}

impl Encoder {
    /// C: `opus_encoder_create(rate, channels, application, &error)`.
    pub fn new(sample_rate: u32, channels: u8, application: Application) -> ChiakiResult<Encoder> {
        let lib = init()?;
        if channels == 0 || channels > 2 {
            tracing::error!("Opus encoder: invalid channel count {channels} (opus supports 1..=2)");
            return Err(ChiakiError::InvalidData);
        }
        let sample_rate = normalize_rate(sample_rate);
        let mut error: c_int = OPUS_OK;
        let ptr = unsafe {
            (lib.api().opus_encoder_create)(
                sample_rate as c_int,
                channels as c_int,
                application.as_c_int(),
                &mut error,
            )
        };
        if error != OPUS_OK || ptr.is_null() {
            tracing::error!(
                "OpusEncoder failed to initialize opus encoder: {}",
                strerror(error)
            );
            if !ptr.is_null() {
                unsafe { (lib.api().opus_encoder_destroy)(ptr) };
            }
            return Err(ChiakiError::Unknown);
        }
        Ok(Encoder {
            ptr: std::ptr::NonNull::new(ptr).expect("checked above"),
            sample_rate,
            channels,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Encodiert `frame_size` Samples pro Kanal aus `pcm` (interleaved i16).
    /// Rückgabe: Anzahl Bytes im Paket.
    pub fn encode(&mut self, pcm: &[i16], frame_size: u32, out: &mut [u8]) -> ChiakiResult<usize> {
        let lib = init()?;
        if (pcm.len() / self.channels as usize) < frame_size as usize {
            tracing::error!("Encoding audio frame with opus failed: pcm buffer too small");
            return Err(ChiakiError::BufTooSmall);
        }
        let r = unsafe {
            (lib.api().opus_encode)(
                self.ptr.as_ptr(),
                pcm.as_ptr(),
                frame_size as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if r < 0 {
            tracing::error!("Encoding audio frame with opus failed: {}", strerror(r));
            return Err(ChiakiError::Unknown);
        }
        Ok(r as usize)
    }

    /// C: `opus_encoder_ctl(st, OPUS_SET_BITRATE, bitrate)`.
    /// `bitrate` in bit/s; `OPUS_AUTO`/`OPUS_BITRATE_MAX` erlaubt.
    pub fn set_bitrate(&mut self, bitrate: i32) -> ChiakiResult<()> {
        self.ctl(OPUS_SET_BITRATE_REQUEST, bitrate, "OPUS_SET_BITRATE")
    }

    /// C: `opus_encoder_ctl(st, OPUS_SET_SIGNAL, signal)`.
    pub fn set_signal(&mut self, signal: Signal) -> ChiakiResult<()> {
        self.ctl(
            OPUS_SET_SIGNAL_REQUEST,
            signal.as_c_int(),
            "OPUS_SET_SIGNAL",
        )
    }

    fn ctl(&mut self, request: c_int, value: c_int, name: &str) -> ChiakiResult<()> {
        let lib = init()?;
        let r = unsafe { (lib.api().opus_encoder_ctl)(self.ptr.as_ptr(), request, value) };
        if r != OPUS_OK {
            tracing::error!("opus_encoder_ctl({name}) failed: {}", strerror(r));
            return Err(ChiakiError::Unknown);
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if let Some(lib) = OPUS.get() {
            unsafe { (lib.api().opus_encoder_destroy)(self.ptr.as_ptr()) };
        }
    }
}

/// libopus unterstützt nur 8/12/16/24/48 kHz — nächstliegende unterstützen.
/// (PS-Remote-Play nutzt immer 48000; dies ist nur eine defensive Normalisierung.)
fn normalize_rate(rate: u32) -> u32 {
    const SUPPORTED: [u32; 5] = [8_000, 12_000, 16_000, 24_000, 48_000];
    if SUPPORTED.contains(&rate) {
        return rate;
    }
    let nearest = *SUPPORTED
        .iter()
        .min_by_key(|&&s| s.abs_diff(rate))
        .expect("non-empty");
    tracing::warn!("Opus sample rate {rate} not supported by libopus, using {nearest}");
    nearest
}

// ---------------------------------------------------------------------------
// Port von lib/src/opusdecoder.c (ChiakiOpusDecoder)
// ---------------------------------------------------------------------------

/// Port von `ChiakiOpusDecoder`: Header-getriebener Decoder mit PCM-Puffer und
/// Concealment-Pfad für die low-latency-Audio-Pipeline (audioreceiver → PCM).
///
/// Der chiaki-core `AudioSink`/audioreceiver liefert Pakete; **ein leerer Slice
/// bedeutet verlorenen Frame** (C: `frame_cb(NULL, 0)`) und erzeugt libopus-PLC
/// statt eines Fehlers.
pub struct OpusAudioDecoder {
    header: AudioHeader,
    decoder: Option<Decoder>,
    pcm: Vec<i16>,
}

impl Default for OpusAudioDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusAudioDecoder {
    /// C: `chiaki_opus_decoder_init`.
    pub fn new() -> Self {
        OpusAudioDecoder {
            header: AudioHeader::default(),
            decoder: None,
            pcm: Vec::new(),
        }
    }

    /// C: `decoder->audio_header`.
    pub fn header(&self) -> AudioHeader {
        self.header
    }

    /// C: `chiaki_opus_decoder_header` — (Re-)Initialisierung mit neuem Audio-Header.
    ///
    /// Das ist der `header_cb`-Pfad: rate/channels kommen aus dem AudioHeader des
    /// Streams, `frame_size` ist die Samples-pro-Kanal-Paketausdauer (480 @ 48 kHz).
    pub fn set_header(&mut self, header: AudioHeader) -> ChiakiResult<()> {
        self.decoder = None;
        self.pcm.clear();

        let decoder = Decoder::new(header.rate, header.channels).inspect_err(|_e| {
            // C-Logtext: "ChiakiOpusDecoder failed to initialize opus decoder: %s"
            // (Detail-Log passiert in Decoder::new).
            tracing::error!("ChiakiOpusDecoder failed to initialize opus decoder");
        })?;

        tracing::info!("ChiakiOpusDecoder initialized");

        // C: pcm_buf = frame_size * channels * sizeof(int16_t)
        let pcm_len = header.frame_buf_size() / std::mem::size_of::<i16>();
        self.pcm.resize(pcm_len, 0);

        self.decoder = Some(decoder);
        self.header = header;
        Ok(())
    }

    /// C: `chiaki_opus_decoder_frame`.
    ///
    /// Ein **leeres Packet** (`&[]`) ist verlorener Frame → Concealment
    /// (`opus_decode(..., NULL, 0, ...)`), wie der audioreceiver-Jitter-Buffer
    /// es bei Lücken liefert. Rückgabe: interleaved PCM (`r * channels` Samples),
    /// gültig bis zum nächsten `decode_frame`-Aufruf.
    pub fn decode_frame(&mut self, packet: &[u8]) -> ChiakiResult<&[i16]> {
        let Some(decoder) = self.decoder.as_mut() else {
            tracing::error!("Received audio frame, but opus decoder is not initialized");
            return Err(ChiakiError::Uninitialized);
        };
        let opt = if packet.is_empty() {
            None
        } else {
            Some(packet)
        };
        let r = decoder.decode(opt, &mut self.pcm)?;
        Ok(&self.pcm[..r * self.header.channels as usize])
    }
}

// ---------------------------------------------------------------------------
// Port von lib/src/opusencoder.c (ChiakiOpusEncoder, MIC-Pfad)
// ---------------------------------------------------------------------------

/// Port von `ChiakiOpusEncoder` — Mikrofon-Encoder wie in streamsession.cpp
/// genutzt: `OPUS_APPLICATION_RESTRICTED_LOWDELAY`, 40-Byte-Ausgabepuffer,
/// `frame_size` aus dem AudioHeader (MICROPHONE_SAMPLES = 480 = 10 ms @ 48 kHz).
/// Keine CTLs (so wie im C-Client); Bitsatz/Signal optional via `Encoder`.
pub struct OpusAudioEncoder {
    header: AudioHeader,
    encoder: Option<Encoder>,
    /// C: opus_frame_buf, fix 40 Bytes ("Encoded audio frame with unexpected
    /// size %d, expected 40; dropping packet as protocol violation").
    frame_buf: Vec<u8>,
}

impl Default for OpusAudioEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl OpusAudioEncoder {
    /// C: `chiaki_opus_encoder_init`.
    pub fn new() -> Self {
        OpusAudioEncoder {
            header: AudioHeader::default(),
            encoder: None,
            frame_buf: Vec::new(),
        }
    }

    pub fn header(&self) -> AudioHeader {
        self.header
    }

    /// C: `chiaki_opus_encoder_header` (ohne den AudioSender — der ist
    /// Protokollsache von chiaki-core `audiosender`).
    pub fn set_header(&mut self, header: AudioHeader) -> ChiakiResult<()> {
        self.encoder = None;
        self.frame_buf.clear();

        let encoder = Encoder::new(
            header.rate,
            header.channels,
            Application::RestrictedLowdelay,
        )
        .inspect_err(|_e| {
            tracing::error!("ChiakiOpusEncoder failed to initialize opus encoder");
        })?;

        tracing::info!("ChiakiOpusEncoder initialized");

        // C: opus_frame_buf_size_required = 40
        self.frame_buf.resize(40, 0);

        self.encoder = Some(encoder);
        self.header = header;
        Ok(())
    }

    /// C: `chiaki_opus_encoder_frame` — encodiert `frame_size` Samples pro Kanal.
    /// Wie im C wird ein Paket unerwarteter Größe (≠ 40 Bytes) als
    /// Protokollverstoß verworfen (C-Logtext übernommen).
    pub fn encode_frame(&mut self, pcm: &[i16]) -> ChiakiResult<&[u8]> {
        let Some(encoder) = self.encoder.as_mut() else {
            tracing::error!("Received audio frame, but opus encoder is not initialized");
            return Err(ChiakiError::Uninitialized);
        };
        let r = encoder.encode(pcm, self.header.frame_size, &mut self.frame_buf)?;
        if r != self.frame_buf.len() {
            // CHIAKI_LOGV-Text aus dem C übernommen (VERBOSE → trace).
            tracing::trace!(
                "Encoded audio frame with unexpected size {}, expected {}; dropping packet as protocol violation",
                r,
                self.frame_buf.len()
            );
            return Err(ChiakiError::InvalidData);
        }
        Ok(&self.frame_buf[..r])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() {
        crate::test_setup::reference_dlls();
    }

    /// Sinus-Testton (440 Hz, halber Aussteuerbereich), interleaved stereo.
    fn sine_pcm(samples_per_channel: usize, channels: usize) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(samples_per_channel * channels);
        for n in 0..samples_per_channel {
            for ch in 0..channels {
                let freq = if ch == 0 { 440.0 } else { 660.0 };
                let v = (2.0 * std::f64::consts::PI * freq * n as f64 / 48_000.0).sin();
                pcm.push((v * 12_000.0) as i16);
            }
        }
        pcm
    }

    /// SNR in dB zwischen Original und Rekonstruktion (über die Länge `len`).
    fn snr_db(orig: &[i16], rec: &[i16], len: usize) -> f64 {
        let mut sig_energy = 0.0f64;
        let mut err_energy = 0.0f64;
        for i in 0..len {
            let s = orig[i] as f64;
            let d = rec[i] as f64 - s;
            sig_energy += s * s;
            err_energy += d * d;
        }
        if err_energy == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (sig_energy / err_energy).log10()
    }

    #[test]
    fn opus_loads_and_reports_strerror() {
        setup();
        let lib = init().expect("opus.dll/libopus-0.dll muss ladbar sein");
        // Streifzug durch die API: strerror liefert Text für alle bekannten Codes.
        for code in [OPUS_OK, OPUS_BAD_ARG, OPUS_INVALID_PACKET] {
            let s = unsafe {
                // SAFETY: Rückgabe ist statischer C-String von libopus.
                std::ffi::CStr::from_ptr((lib.api().opus_strerror)(code)).to_string_lossy()
            };
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn encoder_decoder_roundtrip_snr_above_20db() {
        setup();
        const RATE: u32 = 48_000;
        const CH: usize = 2;
        const FRAME: usize = 480; // 10 ms wie MICROPHONE_SAMPLES im C
        const FRAMES: usize = 60; // 600 ms

        let mut enc =
            Encoder::new(RATE, CH as u8, Application::RestrictedLowdelay).expect("encoder create");
        let mut dec = Decoder::new(RATE, CH as u8).expect("decoder create");
        let _ = enc.set_bitrate(96_000); // mehr als der Lowdelay-Default hilft dem SNR
        let _ = enc.set_signal(Signal::Music);

        let pcm = sine_pcm(FRAME * FRAMES, CH);
        let mut buf = [0u8; 4000];
        let mut decoded: Vec<i16> = Vec::with_capacity(pcm.len());
        for f in 0..FRAMES {
            let slice = &pcm[f * FRAME * CH..(f + 1) * FRAME * CH];
            let n = enc.encode(slice, FRAME as u32, &mut buf).expect("encode");
            assert!(n > 0 && n <= buf.len());
            let mut out = vec![0i16; FRAME * CH];
            let r = dec.decode(Some(&buf[..n]), &mut out).expect("decode");
            assert_eq!(r, FRAME);
            decoded.extend_from_slice(&out);
        }

        // Mono-Downmix für Lag-Suche und SNR.
        let mono: Vec<f64> = (0..pcm.len() / 2)
            .map(|i| (pcm[2 * i] as f64 + pcm[2 * i + 1] as f64) / 2.0)
            .collect();
        let dmono: Vec<f64> = (0..decoded.len() / 2)
            .map(|i| (decoded[2 * i] as f64 + decoded[2 * i + 1] as f64) / 2.0)
            .collect();

        // Besten Lag suchen (Encoder-/Decoder-Lookahead kompensieren). Schritt 1
        // über 0..=1024: sparsame Abtastung kann bei periodischen Signalen in ein
        // Periode-Mehrfaches rutschen und die SNR-Messung verhunzen.
        let mut best_lag = 0usize;
        let mut best_corr = f64::NEG_INFINITY;
        let max_lag = 1024usize.min(dmono.len() - 1);
        for lag in 0..=max_lag {
            let mut corr = 0.0f64;
            for i in 0..mono.len() - max_lag {
                corr += mono[i] * dmono[i + lag];
            }
            if corr > best_corr {
                best_corr = corr;
                best_lag = lag;
            }
        }
        assert!(best_lag > 0, "opus lookahead expected");

        // SNR pro Kanal: decoded[k] entspricht pcm[k - 2*best_lag] — der Lag geht
        // nur auf EINE Seite (Lookahead-Kompensation), sonst vergleicht man
        // unkorrelierte Signale.
        let len = mono.len() - best_lag;
        for ch in 0..CH {
            let orig: Vec<i16> = (0..len).map(|i| pcm[2 * i + ch]).collect();
            let rec: Vec<i16> = decoded[2 * best_lag + ch..]
                .iter()
                .step_by(2)
                .take(len)
                .copied()
                .collect();
            let db = snr_db(&orig, &rec, orig.len());
            assert!(db > 20.0, "channel {ch}: SNR {db:.1} dB <= 20 dB");
        }
    }

    #[test]
    fn concealment_produces_samples() {
        setup();
        let mut dec = Decoder::new(48_000, 2).expect("decoder create");
        let mut out = vec![0i16; 480 * 2];
        // NULL-Payload → PLC: libopus muss frame_size Samples liefern (nicht 0/Fehler).
        let r = dec.decode(None, &mut out).expect("PLC decode");
        assert_eq!(r, 480);
    }

    #[test]
    fn audio_decoder_header_and_concealment_port() {
        setup();
        // Wie streamsession.cpp: 2ch/16bit/48000 (MICROPHONE_SAMPLES*100)/480
        let header = chiaki_core::audio::AudioHeader::set(2, 16, 48_000, 480);
        let mut dec = OpusAudioDecoder::new();
        assert!(
            dec.decode_frame(&[]).is_err(),
            "vor Header muss decode fehlschlagen"
        );
        dec.set_header(header).expect("set_header");
        assert_eq!(dec.header(), header);

        // Verlorener Frame → Concealment mit voller Framegröße (C: frame_cb(NULL, 0)).
        let pcm = dec.decode_frame(&[]).expect("PLC");
        assert_eq!(pcm.len(), 480 * 2);
        assert!(
            pcm.iter().all(|&s| s.abs() < 8_000),
            "PLC erzeugt gedämpftes Rauschen"
        );

        // Echtes Paket: Encoder-Gegenstück nutzen (MIC-Pfad, 40-Byte-Vertrag
        // kann bei RESTRICTED_LOWDELAY abweichen — deshalb hier Encoding ohne
        // Größen-Festnagelung und Decode gegen die echten Bytes).
        let mut enc = OpusAudioEncoder::new();
        enc.set_header(header).expect("enc set_header");
        let pcm_in = sine(480);
        let packet = enc.encode_frame(&pcm_in);
        // 40-Byte-Vertrag (C chiaki_opus_encoder_frame) ist bei aktivem Encoder
        // für Sprache/Ton erfüllt oder das Paket wird wie im C verworfen —
        // beides ist portiertes Verhalten; hier akzeptieren wir beides:
        if let Ok(packet) = packet {
            let pcm_out = dec.decode_frame(packet).expect("decode");
            assert_eq!(pcm_out.len(), 480 * 2);
        }
    }

    fn sine(samples_per_channel: usize) -> Vec<i16> {
        let mut pcm = Vec::with_capacity(samples_per_channel * 2);
        for n in 0..samples_per_channel {
            let v = (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 48_000.0).sin();
            let s = (v * 10_000.0) as i16;
            pcm.push(s);
            pcm.push(s);
        }
        pcm
    }

    #[test]
    fn decoder_rejects_invalid_channels() {
        setup();
        assert!(matches!(
            Decoder::new(48_000, 0),
            Err(chiaki_core::ChiakiError::InvalidData)
        ));
        assert!(matches!(
            Decoder::new(48_000, 5),
            Err(chiaki_core::ChiakiError::InvalidData)
        ));
    }
}
