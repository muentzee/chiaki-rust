// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/include/chiaki/common.h + lib/src/common.c (chiaki-ng).

use std::fmt;
use std::sync::OnceLock;

/// Port von `ChiakiErrorCode` (lib/include/chiaki/common.h).
///
/// Die Diskriminanten (0..=21) werden exakt wie im C-Code behalten — sie
/// tauchen u. a. in Protokoll-/Session-Logs und beim Fehler-Mapping wieder auf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum ChiakiError {
    Success = 0,
    Unknown,
    ParseAddr,
    Thread,
    Memory,
    Overflow,
    Network,
    ConnectionRefused,
    HostDown,
    HostUnreach,
    Disconnected,
    InvalidData,
    BufTooSmall,
    MutexLocked,
    Canceled,
    Timeout,
    InvalidResponse,
    InvalidMac,
    Uninitialized,
    FecFailed,
    VersionMismatch,
    HttpNonok,
}

impl ChiakiError {
    /// Port von `chiaki_error_string()` (lib/src/common.c).
    ///
    /// Der C-Switch kennt `Unknown`, `Overflow`, `VersionMismatch` und
    /// `HttpNonok` nicht — sie fallen dort in den default-Zweig ("Unknown").
    /// Dieses Verhalten wird hier exakt übernommen.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChiakiError::Success => "Success",
            ChiakiError::ParseAddr => "Failed to parse host address",
            ChiakiError::Thread => "Thread error",
            ChiakiError::Memory => "Memory error",
            ChiakiError::Network => "Network error",
            ChiakiError::ConnectionRefused => "Connection Refused",
            ChiakiError::HostDown => "Host is down",
            ChiakiError::HostUnreach => "No route to host",
            ChiakiError::Disconnected => "Disconnected",
            ChiakiError::InvalidData => "Invalid data",
            ChiakiError::BufTooSmall => "Buffer too small",
            ChiakiError::MutexLocked => "Mutex is locked",
            ChiakiError::Canceled => "Canceled",
            ChiakiError::Timeout => "Timeout",
            ChiakiError::InvalidResponse => "Invalid Response",
            ChiakiError::InvalidMac => "Invalid MAC",
            ChiakiError::Uninitialized => "Uninitialized",
            ChiakiError::FecFailed => "FEC failed",
            ChiakiError::Unknown
            | ChiakiError::Overflow
            | ChiakiError::VersionMismatch
            | ChiakiError::HttpNonok => "Unknown",
        }
    }

    /// C-Diskriminante als u32 (z. B. für Logs/Fehlersuche).
    pub fn code(self) -> u32 {
        self as u32
    }
}

impl fmt::Display for ChiakiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for ChiakiError {}

/// Port des C-Rückgabekonvents: `ChiakiResult<T>` ersetzt `ChiakiErrorCode`
/// als Rückgabetyp, `Ok` entspricht `CHIAKI_ERR_SUCCESS`.
pub type ChiakiResult<T> = Result<T, ChiakiError>;

/// Port von `ChiakiTarget` (lib/include/chiaki/common.h).
/// "values must not change" — Diskriminanten sind protokollrelevant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum Target {
    Ps4Unknown = 0,
    Ps4_8 = 800,
    Ps4_9 = 900,
    Ps4_10 = 1000,
    Ps5Unknown = 1_000_000,
    Ps5_1 = 1_000_100,
}

impl Target {
    /// C-Diskriminante (z. B. für Serialisierung im ctrl/takion-Protokoll).
    pub fn value(self) -> u32 {
        self as u32
    }

    /// Port von `chiaki_target_is_unknown()`.
    pub fn is_unknown(self) -> bool {
        matches!(self, Target::Ps5Unknown | Target::Ps4Unknown)
    }

    /// Port von `chiaki_target_is_ps5()` (C: `target >= CHIAKI_TARGET_PS5_UNKNOWN`;
    /// für den geschlossenen Rust-Enum äquivalent zu diesem Match).
    pub fn is_ps5(self) -> bool {
        matches!(self, Target::Ps5Unknown | Target::Ps5_1)
    }

    /// Aus Protokollwert rekonstruieren (nur exakte Diskriminanten gültig).
    pub fn from_u32(v: u32) -> Option<Target> {
        match v {
            0 => Some(Target::Ps4Unknown),
            800 => Some(Target::Ps4_8),
            900 => Some(Target::Ps4_9),
            1000 => Some(Target::Ps4_10),
            1_000_000 => Some(Target::Ps5Unknown),
            1_000_100 => Some(Target::Ps5_1),
            _ => None,
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value())
    }
}

/// Port von `ChiakiCodec` (lib/include/chiaki/common.h).
/// "values must not change" — Diskriminanten sind protokollrelevant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
    H265Hdr = 2,
}

impl Codec {
    /// Port von `chiaki_codec_is_h265()`.
    pub fn is_h265(self) -> bool {
        matches!(self, Codec::H265 | Codec::H265Hdr)
    }

    /// Port von `chiaki_codec_is_hdr()`.
    pub fn is_hdr(self) -> bool {
        matches!(self, Codec::H265Hdr)
    }

    /// Port von `chiaki_codec_name()`.
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H264",
            Codec::H265 => "H265",
            Codec::H265Hdr => "H265/HDR",
        }
    }

    /// Aus Protokollwert rekonstruieren.
    pub fn from_u32(v: u32) -> Option<Codec> {
        match v {
            0 => Some(Codec::H264),
            1 => Some(Codec::H265),
            2 => Some(Codec::H265Hdr),
            _ => None,
        }
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

static LIB_INIT: OnceLock<ChiakiResult<()>> = OnceLock::new();

/// Port von `chiaki_lib_init()` (lib/src/common.c).
///
/// Abweichungen/Entscheidungen gegenüber C:
/// - `srand(chiaki_random_bytes_crypt(...))`: entfällt — die `rand`-Crate
///   initialisiert ihren Thread-RNG lazy und braucht kein globales Seeding.
/// - `galois_init_default_field(CHIAKI_FEC_WORDSIZE)`: gehört in Rust in das
///   `fec`-Modul, das seine Tabellen lazy (`OnceLock`) aufbaut.
/// - `WSAStartup` (Windows): `std::net` initialisiert Winsock selbst bei der
///   ersten Socket-Operation. Statt eines direkten `WSAStartup`-Aufrufs (unsafe
///   über die windows-crate) erzwingen/prüfen wir die Initialisierung hier mit
///   einem Probe-UDP-Socket auf localhost — gleiche Semantik: schlägt sie fehl,
///   wird `ChiakiError::Network` geliefert.
///
/// Idempotent: weitere Aufrufe liefern das Ergebnis des ersten Aufrufs.
pub fn lib_init() -> ChiakiResult<()> {
    let init = || -> ChiakiResult<()> {
        #[cfg(windows)]
        {
            match std::net::UdpSocket::bind("127.0.0.1:0") {
                // Probe-Socket wird direkt wieder geschlossen (Drop).
                Ok(_probe) => Ok(()),
                Err(e) => {
                    tracing::error!("lib_init: Winsock-Init fehlgeschlagen: {e}");
                    Err(ChiakiError::Network)
                }
            }
        }
        #[cfg(not(windows))]
        {
            Ok(())
        }
    };
    *LIB_INIT.get_or_init(init)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_discriminants_match_c() {
        assert_eq!(ChiakiError::Success as u32, 0);
        assert_eq!(ChiakiError::Unknown as u32, 1);
        assert_eq!(ChiakiError::ParseAddr as u32, 2);
        assert_eq!(ChiakiError::Thread as u32, 3);
        assert_eq!(ChiakiError::Memory as u32, 4);
        assert_eq!(ChiakiError::Overflow as u32, 5);
        assert_eq!(ChiakiError::Network as u32, 6);
        assert_eq!(ChiakiError::ConnectionRefused as u32, 7);
        assert_eq!(ChiakiError::HostDown as u32, 8);
        assert_eq!(ChiakiError::HostUnreach as u32, 9);
        assert_eq!(ChiakiError::Disconnected as u32, 10);
        assert_eq!(ChiakiError::InvalidData as u32, 11);
        assert_eq!(ChiakiError::BufTooSmall as u32, 12);
        assert_eq!(ChiakiError::MutexLocked as u32, 13);
        assert_eq!(ChiakiError::Canceled as u32, 14);
        assert_eq!(ChiakiError::Timeout as u32, 15);
        assert_eq!(ChiakiError::InvalidResponse as u32, 16);
        assert_eq!(ChiakiError::InvalidMac as u32, 17);
        assert_eq!(ChiakiError::Uninitialized as u32, 18);
        assert_eq!(ChiakiError::FecFailed as u32, 19);
        assert_eq!(ChiakiError::VersionMismatch as u32, 20);
        assert_eq!(ChiakiError::HttpNonok as u32, 21);
    }

    #[test]
    fn error_strings_match_chiaki_error_string() {
        assert_eq!(ChiakiError::Success.as_str(), "Success");
        assert_eq!(ChiakiError::Unknown.as_str(), "Unknown");
        assert_eq!(ChiakiError::ParseAddr.as_str(), "Failed to parse host address");
        assert_eq!(ChiakiError::Thread.as_str(), "Thread error");
        assert_eq!(ChiakiError::Memory.as_str(), "Memory error");
        assert_eq!(ChiakiError::Overflow.as_str(), "Unknown"); // C: default-Zweig
        assert_eq!(ChiakiError::Network.as_str(), "Network error");
        assert_eq!(ChiakiError::ConnectionRefused.as_str(), "Connection Refused");
        assert_eq!(ChiakiError::HostDown.as_str(), "Host is down");
        assert_eq!(ChiakiError::HostUnreach.as_str(), "No route to host");
        assert_eq!(ChiakiError::Disconnected.as_str(), "Disconnected");
        assert_eq!(ChiakiError::InvalidData.as_str(), "Invalid data");
        assert_eq!(ChiakiError::BufTooSmall.as_str(), "Buffer too small");
        assert_eq!(ChiakiError::MutexLocked.as_str(), "Mutex is locked");
        assert_eq!(ChiakiError::Canceled.as_str(), "Canceled");
        assert_eq!(ChiakiError::Timeout.as_str(), "Timeout");
        assert_eq!(ChiakiError::InvalidResponse.as_str(), "Invalid Response");
        assert_eq!(ChiakiError::InvalidMac.as_str(), "Invalid MAC");
        assert_eq!(ChiakiError::Uninitialized.as_str(), "Uninitialized");
        assert_eq!(ChiakiError::FecFailed.as_str(), "FEC failed");
        assert_eq!(ChiakiError::VersionMismatch.as_str(), "Unknown"); // C: default-Zweig
        assert_eq!(ChiakiError::HttpNonok.as_str(), "Unknown"); // C: default-Zweig
        // Display == chiaki_error_string-Text
        assert_eq!(ChiakiError::Timeout.to_string(), "Timeout");
    }

    #[test]
    fn target_discriminants_and_predicates() {
        assert_eq!(Target::Ps4Unknown as u32, 0);
        assert_eq!(Target::Ps4_8 as u32, 800);
        assert_eq!(Target::Ps4_9 as u32, 900);
        assert_eq!(Target::Ps4_10 as u32, 1000);
        assert_eq!(Target::Ps5Unknown as u32, 1_000_000);
        assert_eq!(Target::Ps5_1 as u32, 1_000_100);

        assert!(Target::Ps4Unknown.is_unknown());
        assert!(Target::Ps5Unknown.is_unknown());
        assert!(!Target::Ps4_8.is_unknown());
        assert!(!Target::Ps4_10.is_unknown());
        assert!(!Target::Ps5_1.is_unknown());

        assert!(!Target::Ps4Unknown.is_ps5());
        assert!(!Target::Ps4_8.is_ps5());
        assert!(!Target::Ps4_9.is_ps5());
        assert!(!Target::Ps4_10.is_ps5());
        assert!(Target::Ps5Unknown.is_ps5());
        assert!(Target::Ps5_1.is_ps5());

        for t in [
            Target::Ps4Unknown,
            Target::Ps4_8,
            Target::Ps4_9,
            Target::Ps4_10,
            Target::Ps5Unknown,
            Target::Ps5_1,
        ] {
            assert_eq!(Target::from_u32(t.value()), Some(t));
        }
        assert_eq!(Target::from_u32(1234), None);
    }

    #[test]
    fn codec_predicates_and_names() {
        assert_eq!(Codec::H264 as u32, 0);
        assert_eq!(Codec::H265 as u32, 1);
        assert_eq!(Codec::H265Hdr as u32, 2);

        assert!(!Codec::H264.is_h265());
        assert!(Codec::H265.is_h265());
        assert!(Codec::H265Hdr.is_h265());
        assert!(!Codec::H264.is_hdr());
        assert!(!Codec::H265.is_hdr());
        assert!(Codec::H265Hdr.is_hdr());

        assert_eq!(Codec::H264.name(), "H264");
        assert_eq!(Codec::H265.name(), "H265");
        assert_eq!(Codec::H265Hdr.name(), "H265/HDR");

        for c in [Codec::H264, Codec::H265, Codec::H265Hdr] {
            assert_eq!(Codec::from_u32(c as u32), Some(c));
        }
        assert_eq!(Codec::from_u32(3), None);
    }

    #[test]
    fn lib_init_is_idempotent() {
        assert_eq!(lib_init(), Ok(()));
        assert_eq!(lib_init(), Ok(()));
    }
}
