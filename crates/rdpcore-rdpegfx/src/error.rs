//! Typed errors for MS-RDPEGFX and H.264 video encoders.

use thiserror::Error;

/// Errors produced during H.264 encoder selection, initialization, geometry validation, or frame encoding.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EncoderError {
    /// Desktop frame dimensions or buffer length do not match requirements.
    #[error("invalid frame geometry: {0}")]
    InvalidGeometry(String),

    /// Encoder backend failed to initialize or probe.
    #[error("encoder initialization failed: {0}")]
    InitFailed(String),

    /// Frame encoding failed during bitstream generation or buffer manipulation.
    #[error("frame encoding failed: {0}")]
    EncodeFailed(String),

    /// No suitable H.264 encoder backend is compiled in or available.
    #[error("no supported H.264 encoder backends available (enable openh264/vaapi/nvenc)")]
    NoBackendAvailable,
}
