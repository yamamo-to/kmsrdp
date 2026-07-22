//! Runtime H.264 encoder selection: NVENC → VAAPI → OpenH264.

use tracing::info;
#[cfg(any(feature = "nvenc", feature = "vaapi"))]
use tracing::debug;

use crate::encoder::H264Encoder;

#[cfg(feature = "nvenc")]
use crate::nvenc_enc::NvencH264Encoder;
#[cfg(feature = "openh264")]
use crate::openh264_enc::OpenH264Encoder;
#[cfg(feature = "vaapi")]
use crate::vaapi_enc::VaapiH264Encoder;

/// Chosen encoder backend plus a stable name for logs (`nvenc`/`vaapi`/`openh264`).
pub struct SelectedEncoder {
    pub encoder: Box<dyn H264Encoder>,
    pub name: &'static str,
}

/// Probe optional hardware backends then fall back to OpenH264.
pub fn select_h264_encoder() -> Result<SelectedEncoder, String> {
    #[cfg(feature = "nvenc")]
    match NvencH264Encoder::probe() {
        Ok(encoder) => {
            info!("GFX encoder=nvenc");
            return Ok(SelectedEncoder {
                encoder: Box::new(encoder),
                name: "nvenc",
            });
        }
        Err(e) => debug!(error = %e, "NVENC probe failed"),
    }

    #[cfg(feature = "vaapi")]
    match VaapiH264Encoder::probe() {
        Ok(encoder) => {
            info!("GFX encoder=vaapi");
            return Ok(SelectedEncoder {
                encoder: Box::new(encoder),
                name: "vaapi",
            });
        }
        Err(e) => debug!(error = %e, "VAAPI probe failed"),
    }

    #[cfg(feature = "openh264")]
    {
        let encoder = OpenH264Encoder::new()?;
        info!("GFX encoder=openh264");
        Ok(SelectedEncoder {
            encoder: Box::new(encoder),
            name: "openh264",
        })
    }

    #[cfg(not(feature = "openh264"))]
    {
        Err("no H.264 encoder backends compiled in (enable openh264/vaapi/nvenc)".into())
    }
}
