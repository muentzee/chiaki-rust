//! chiaki-render — Video-Präsentation (Ergebnis von Spike S1).
//!
//! Enthält die CPU-seitige NV12→BGRA-Konvertierung ([`nv12`]) und den
//! wiederverwendbaren [`presenter::VideoPresenter`], der NV12-Frames als
//! GPUI-`RenderImage`-Texturen pro Frame darstellt.

pub mod gpu_sink;
pub mod nv12;
pub mod presenter;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid dimensions: {width}x{height} (must be non-zero and even)")]
    InvalidDimensions { width: u32, height: u32 },

    #[error("destination buffer too small: need {need} bytes, have {have}")]
    DestinationTooSmall { need: usize, have: usize },

    #[error("source plane too small: need {need} bytes, have {have}")]
    SourceTooSmall { need: usize, have: usize },
}

pub type Result<T> = std::result::Result<T, Error>;
