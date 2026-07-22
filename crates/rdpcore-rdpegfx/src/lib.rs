//! MS-RDPEGFX (Graphics Pipeline Extension) over the
//! `"Microsoft::Windows::RDS::Graphics"` dynamic virtual channel.
//!
//! Provides the wire-format codec ([`pdu`]), an encoder trait ([`encoder`]),
//! and a [`GfxSession`] that negotiates AVC420 and emits full-frame updates.

pub mod encoder;
pub mod h264_headers;
pub mod pdu;
pub mod select;
pub mod session;

#[cfg(feature = "openh264")]
pub mod openh264_enc;
#[cfg(feature = "vaapi")]
pub mod vaapi_enc;
#[cfg(feature = "nvenc")]
pub mod nvenc_enc;

pub use encoder::{EncodedAu, H264Encoder, MockH264Encoder};
pub use pdu::CHANNEL_NAME;
pub use select::{SelectedEncoder, select_h264_encoder};
pub use session::{GfxDvcHandler, GfxSession};

#[cfg(feature = "openh264")]
pub use openh264_enc::OpenH264Encoder;
#[cfg(feature = "vaapi")]
pub use vaapi_enc::VaapiH264Encoder;
#[cfg(feature = "nvenc")]
pub use nvenc_enc::NvencH264Encoder;
