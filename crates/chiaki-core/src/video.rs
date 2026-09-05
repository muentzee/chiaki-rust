// SPDX-License-Identifier: LicenseRef-AGPL-3.0-only-OpenSSL
// Port von lib/include/chiaki/video.h (chiaki-ng) samt der Video-Profile-Presets
// aus session.h/session.c (`chiaki_connect_video_profile_preset`), die inhaltlich
// hier hingehören (Auflösungs-/FPS-Presets und das Verbindungs-Profil).

use super::error::Codec;

/// `CHIAKI_VIDEO_BUFFER_PADDING_SIZE` — Padding für FFMPEG.
pub const VIDEO_BUFFER_PADDING_SIZE: usize = 64;

/// Port von `ChiakiVideoResolutionPreset` (session.h) — "values must not change".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum VideoResolutionPreset {
    Res360p = 1,
    Res540p = 2,
    Res720p = 3,
    Res1080p = 4,
}

impl VideoResolutionPreset {
    /// Aus Protokoll-/Settings-Wert rekonstruieren (nur exakte Diskriminanten).
    pub fn from_u32(v: u32) -> Option<VideoResolutionPreset> {
        match v {
            1 => Some(VideoResolutionPreset::Res360p),
            2 => Some(VideoResolutionPreset::Res540p),
            3 => Some(VideoResolutionPreset::Res720p),
            4 => Some(VideoResolutionPreset::Res1080p),
            _ => None,
        }
    }
}

/// Port von `ChiakiVideoFPSPreset` (session.h) — "values must not change".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u32)]
pub enum VideoFpsPreset {
    Fps30 = 30,
    Fps60 = 60,
}

impl VideoFpsPreset {
    /// Aus Protokoll-/Settings-Wert rekonstruieren.
    pub fn from_u32(v: u32) -> Option<VideoFpsPreset> {
        match v {
            30 => Some(VideoFpsPreset::Fps30),
            60 => Some(VideoFpsPreset::Fps60),
            _ => None,
        }
    }
}

/// Port von `ChiakiConnectVideoProfile` (session.h).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectVideoProfile {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub bitrate: u32,
    pub codec: Codec,
}

impl Default for ConnectVideoProfile {
    /// Wie der default-Zweig der Preset-Tabelle in session.c:
    /// alles 0, Codec H264.
    fn default() -> Self {
        ConnectVideoProfile {
            width: 0,
            height: 0,
            max_fps: 0,
            bitrate: 0,
            codec: Codec::H264,
        }
    }
}

impl ConnectVideoProfile {
    /// Rust-seitige Convenience: Preset direkt konstruieren.
    pub fn from_preset(resolution: VideoResolutionPreset, fps: VideoFpsPreset) -> Self {
        let mut profile = ConnectVideoProfile::default();
        connect_video_profile_preset(&mut profile, resolution, fps);
        profile
    }
}

/// Port von `chiaki_connect_video_profile_preset()` (session.c) — 1:1 die
/// Auflösungs-/Bitrate-Tabelle.
pub fn connect_video_profile_preset(
    profile: &mut ConnectVideoProfile,
    resolution: VideoResolutionPreset,
    fps: VideoFpsPreset,
) {
    profile.codec = Codec::H264;
    match resolution {
        VideoResolutionPreset::Res360p => {
            profile.width = 640;
            profile.height = 360;
            profile.bitrate = 2000;
        }
        VideoResolutionPreset::Res540p => {
            profile.width = 960;
            profile.height = 540;
            profile.bitrate = 6000;
        }
        VideoResolutionPreset::Res720p => {
            profile.width = 1280;
            profile.height = 720;
            profile.bitrate = 10000;
        }
        VideoResolutionPreset::Res1080p => {
            profile.width = 1920;
            profile.height = 1080;
            profile.bitrate = 15000;
        }
    }

    match fps {
        VideoFpsPreset::Fps30 => {
            profile.max_fps = 30;
        }
        VideoFpsPreset::Fps60 => {
            profile.max_fps = 60;
        }
    }
}

/// Port von `ChiakiVideoProfile` (video.h) — Profil inkl. Stream-Header.
///
/// Abweichung zum C: der `header`-Pointer mit `header_sz` wird zu einem
/// besitzenden `Vec<u8>` (keine rohen Pointer in öffentlichen APIs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub header: Vec<u8>,
}

impl VideoProfile {
    pub fn header_sz(&self) -> usize {
        self.header.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_values_must_not_change() {
        assert_eq!(VideoResolutionPreset::Res360p as u32, 1);
        assert_eq!(VideoResolutionPreset::Res540p as u32, 2);
        assert_eq!(VideoResolutionPreset::Res720p as u32, 3);
        assert_eq!(VideoResolutionPreset::Res1080p as u32, 4);
        assert_eq!(VideoFpsPreset::Fps30 as u32, 30);
        assert_eq!(VideoFpsPreset::Fps60 as u32, 60);

        for r in [
            VideoResolutionPreset::Res360p,
            VideoResolutionPreset::Res540p,
            VideoResolutionPreset::Res720p,
            VideoResolutionPreset::Res1080p,
        ] {
            assert_eq!(VideoResolutionPreset::from_u32(r as u32), Some(r));
        }
        assert_eq!(VideoResolutionPreset::from_u32(5), None);
        assert_eq!(VideoFpsPreset::from_u32(30), Some(VideoFpsPreset::Fps30));
        assert_eq!(VideoFpsPreset::from_u32(60), Some(VideoFpsPreset::Fps60));
        assert_eq!(VideoFpsPreset::from_u32(120), None);
    }

    /// Golden-Tabelle 1:1 aus chiaki_connect_video_profile_preset (session.c).
    #[test]
    fn resolution_preset_table_matches_session_c() {
        let cases = [
            (VideoResolutionPreset::Res360p, 640u32, 360u32, 2000u32),
            (VideoResolutionPreset::Res540p, 960, 540, 6000),
            (VideoResolutionPreset::Res720p, 1280, 720, 10000),
            (VideoResolutionPreset::Res1080p, 1920, 1080, 15000),
        ];
        for (res, width, height, bitrate) in cases {
            for fps in [VideoFpsPreset::Fps30, VideoFpsPreset::Fps60] {
                let mut profile = ConnectVideoProfile::default();
                connect_video_profile_preset(&mut profile, res, fps);
                assert_eq!(profile.width, width, "{res:?}");
                assert_eq!(profile.height, height, "{res:?}");
                assert_eq!(profile.bitrate, bitrate, "{res:?}");
                assert_eq!(profile.codec, Codec::H264);
                assert_eq!(
                    profile.max_fps,
                    if fps == VideoFpsPreset::Fps30 { 30 } else { 60 }
                );
            }
        }
    }

    #[test]
    fn from_preset_convenience() {
        let p = ConnectVideoProfile::from_preset(VideoResolutionPreset::Res1080p, VideoFpsPreset::Fps60);
        assert_eq!(p.width, 1920);
        assert_eq!(p.height, 1080);
        assert_eq!(p.bitrate, 15000);
        assert_eq!(p.max_fps, 60);
    }

    #[test]
    fn video_profile_padding_constant() {
        assert_eq!(VIDEO_BUFFER_PADDING_SIZE, 64);
    }

    #[test]
    fn video_profile_owned_header() {
        let mut p = VideoProfile {
            width: 1280,
            height: 720,
            header: Vec::new(),
        };
        p.header.extend_from_slice(&[0, 0, 0, 1, 0x67]); // exemplarischer H264-SPS-Start
        assert_eq!(p.header_sz(), 5);
        assert_eq!(p.header, vec![0, 0, 0, 1, 0x67]);
    }
}
