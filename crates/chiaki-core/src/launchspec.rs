// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
//
// Port of chiaki-ng lib/src/launchspec.c + lib/include/chiaki/launchspec.h.
//
// The launch spec is a JSON object that the StreamConnection hands to the
// console during the Takion big-payload handshake (see streamconnection.c).
// The C code renders it with a single snprintf over a fixed format string —
// the exact field order, spacing and hardcoded dummy values ("sessionId4321",
// "bravia_tv", ...) are protocol-relevant and reproduced verbatim here.
//
// Note: `mtu`/`rtt` are plain JSON numbers (%u in C); there is no NaN
// handling and no "expected_processing_time" field — the launchspec_fmt of
// chiaki-ng simply doesn't contain one.

use super::base64;
use super::error::{ChiakiResult, Codec, Target};

/// `CHIAKI_HANDSHAKE_KEY_SIZE`
pub const HANDSHAKE_KEY_SIZE: usize = 0x10;

/// Port von `ChiakiLaunchSpec`.
///
/// Im C ist `handshake_key` ein Pointer; in Rust ein festes 16-Byte-Array
/// (`CHIAKI_HANDSHAKE_KEY_SIZE`), gefüllt aus `session->handshake_key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub target: Target,
    pub mtu: u32,
    pub rtt: u32,
    pub handshake_key: [u8; HANDSHAKE_KEY_SIZE],
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub codec: Codec,
    pub bw_kbps_sent: u32,
}

/// Port von `chiaki_launchspec_format()`.
///
/// Der C-Code formatiert alles mit einem snprintf über `launchspec_fmt`:
/// - Format-Platzhalter 0-5 (%u): width, height, max_fps, bw_kbps_sent, mtu, rtt
/// - Platzhalter 6 (%s): `extras[0]` — adaptiveStreamMode (nur PS5)
/// - Platzhalter 7/8 (%s): `extras[1]`/`extras[2]` — videoCodec/dynamicRange
///   (nur PS5, sonst leere Strings)
/// - Platzhalter 9 (%s): base64-kodierter Handshake-Key
///
/// Für PS4 sind alle drei Extras leer (C: "TODO: probably also for ps4, but
/// only 12" — d. h. die Extras gelten potenziell auch für PS4-FW >= 12, das
/// C benutzt sie aber aktuell nur für PS5 → 1:1 übernommen).
pub fn launchspec_format(spec: &LaunchSpec) -> ChiakiResult<String> {
    // char handshake_key_b64[CHIAKI_HANDSHAKE_KEY_SIZE * 2]:
    // base64 von 16 Bytes = 24 Zeichen inkl. Padding — passt exakt.
    let handshake_key_b64 = base64::encode(&spec.handshake_key);

    let (extras_adaptive, extras_video_codec, extras_dynamic_range) = if spec.target.is_ps5() {
        (
            ",\"adaptiveStreamMode\": \"resize\"",
            if spec.codec.is_h265() {
                "\"videoCodec\":\"hevc\","
            } else {
                "\"videoCodec\":\"avc\","
            },
            if spec.codec.is_hdr() {
                "\"dynamicRange\":\"HDR\","
            } else {
                "\"dynamicRange\":\"SDR\","
            },
        )
    } else {
        // extras[0] = extras[1] = extras[2] = "";
        ("", "", "")
    };

    // launchspec_fmt, 1:1 inkl. aller festen Dummy-Werte. Die %-Platzhalter
    // des C sind als benannte Argumente eingesetzt; Reihenfolge wie im C:
    //   %u 0-5 -> width, height, max_fps, bw_kbps_sent, mtu, rtt
    //   %s 6   -> extras_adaptive      (nach "audioEncoderProfile":"audio1")
    //   %s 7/8 -> extras_video_codec / extras_dynamic_range (nach userProfile)
    //   %s 9   -> handshake_key_b64
    Ok(format!(
        "{{\
        \"sessionId\":\"sessionId4321\",\
        \"streamResolutions\":[\
        {{\
        \"resolution\":\
        {{\
        \"width\":{width},\
        \"height\":{height}\
        }},\
        \"maxFps\":{max_fps},\
        \"score\":10\
        }}\
        ],\
        \"network\":{{\
        \"bwKbpsSent\":{bw_kbps_sent},\
        \"bwLoss\":0.001000,\
        \"mtu\":{mtu},\
        \"rtt\":{rtt},\
        \"ports\":[53,2053]\
        }},\
        \"slotId\":1,\
        \"appSpecification\":{{\
        \"minFps\":30,\
        \"minBandwidth\":0,\
        \"extTitleId\":\"ps3\",\
        \"version\":1,\
        \"timeLimit\":1,\
        \"startTimeout\":100,\
        \"afkTimeout\":100,\
        \"afkTimeoutDisconnect\":100\
        }},\
        \"konan\":{{\
        \"ps3AccessToken\":\"accessToken\",\
        \"ps3RefreshToken\":\"refreshToken\"\
        }},\"requestGameSpecification\":{{\
        \"model\":\"bravia_tv\",\
        \"platform\":\"android\",\
        \"audioChannels\":\"5.1\",\
        \"language\":\"sp\",\
        \"acceptButton\":\"X\",\
        \"connectedControllers\":[\"xinput\",\"ds3\",\"ds4\"],\
        \"yuvCoefficient\":\"bt601\",\
        \"videoEncoderProfile\":\"hw4.1\",\
        \"audioEncoderProfile\":\"audio1\"\
        {extras_adaptive}\
        }},\
        \"userProfile\":{{\
        \"onlineId\":\"psnId\",\
        \"npId\":\"npId\",\
        \"region\":\"US\",\
        \"languagesUsed\":[\"en\",\"jp\"]\
        }},\
        {extras_video_codec}\
        {extras_dynamic_range}\
        \"handshakeKey\":\"{handshake_key_b64}\"\
        }}",
        width = spec.width,
        height = spec.height,
        max_fps = spec.max_fps,
        bw_kbps_sent = spec.bw_kbps_sent,
        mtu = spec.mtu,
        rtt = spec.rtt,
        extras_adaptive = extras_adaptive,
        extras_video_codec = extras_video_codec,
        extras_dynamic_range = extras_dynamic_range,
        handshake_key_b64 = handshake_key_b64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(target: Target, codec: Codec) -> LaunchSpec {
        LaunchSpec {
            target,
            mtu: 1454,
            rtt: 30,
            handshake_key: [
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
                0x0d, 0x0e, 0x0f,
            ],
            width: 1280,
            height: 720,
            max_fps: 60,
            codec,
            bw_kbps_sent: 10_000,
        }
    }

    /// Golden-Vektor PS4 (H264): Extras leer, Zahlen an Position 0-5.
    #[test]
    fn format_golden_ps4() {
        let expected = "{\"sessionId\":\"sessionId4321\",\
\"streamResolutions\":[{\"resolution\":{\"width\":1280,\"height\":720},\"maxFps\":60,\"score\":10}],\
\"network\":{\"bwKbpsSent\":10000,\"bwLoss\":0.001000,\"mtu\":1454,\"rtt\":30,\"ports\":[53,2053]},\
\"slotId\":1,\
\"appSpecification\":{\"minFps\":30,\"minBandwidth\":0,\"extTitleId\":\"ps3\",\"version\":1,\"timeLimit\":1,\"startTimeout\":100,\"afkTimeout\":100,\"afkTimeoutDisconnect\":100},\
\"konan\":{\"ps3AccessToken\":\"accessToken\",\"ps3RefreshToken\":\"refreshToken\"},\
\"requestGameSpecification\":{\"model\":\"bravia_tv\",\"platform\":\"android\",\"audioChannels\":\"5.1\",\"language\":\"sp\",\"acceptButton\":\"X\",\"connectedControllers\":[\"xinput\",\"ds3\",\"ds4\"],\"yuvCoefficient\":\"bt601\",\"videoEncoderProfile\":\"hw4.1\",\"audioEncoderProfile\":\"audio1\"},\
\"userProfile\":{\"onlineId\":\"psnId\",\"npId\":\"npId\",\"region\":\"US\",\"languagesUsed\":[\"en\",\"jp\"]},\
\"handshakeKey\":\"AAECAwQFBgcICQoLDA0ODw==\"}";

        assert_eq!(launchspec_format(&spec(Target::Ps4_10, Codec::H264)).unwrap(), expected);
        // PS4 8/9 identisch (nicht-PS5)
        assert_eq!(launchspec_format(&spec(Target::Ps4_9, Codec::H264)).unwrap(), expected);
    }

    /// Golden-Vektor PS5 mit H265+HDR: adaptiveStreamMode (mit Leerzeichen wie
    /// im C!), videoCodec "hevc", dynamicRange "HDR".
    #[test]
    fn format_golden_ps5_h265_hdr() {
        let json = launchspec_format(&spec(Target::Ps5_1, Codec::H265Hdr)).unwrap();

        // extras[0] hängt direkt hinter "audioEncoderProfile":"audio1"
        assert!(json.contains("\"audioEncoderProfile\":\"audio1\","));
        assert!(json.contains(",\"adaptiveStreamMode\": \"resize\"},"));
        // extras[1]/[2] stehen direkt nach dem userProfile-Objekt
        assert!(json.contains(
            "\"languagesUsed\":[\"en\",\"jp\"]},\"videoCodec\":\"hevc\",\"dynamicRange\":\"HDR\",\"handshakeKey\":\"AAECAwQFBgcICQoLDA0ODw==\"}"
        ));
    }

    /// PS5 mit H264: videoCodec "avc", dynamicRange "SDR", adaptiveStreamMode trotzdem gesetzt.
    #[test]
    fn format_ps5_h264_sdr() {
        let json = launchspec_format(&spec(Target::Ps5_1, Codec::H264)).unwrap();
        assert!(json.contains(",\"adaptiveStreamMode\": \"resize\"},"));
        assert!(json.contains("},\"videoCodec\":\"avc\",\"dynamicRange\":\"SDR\",\"handshakeKey\":"));
        assert!(!json.contains("hevc"));
    }

    /// PS5 mit H265 ohne HDR: "hevc" + "SDR".
    #[test]
    fn format_ps5_h265_sdr() {
        let json = launchspec_format(&spec(Target::Ps5Unknown, Codec::H265)).unwrap();
        assert!(json.contains("\"videoCodec\":\"hevc\",\"dynamicRange\":\"SDR\","));
    }
}
