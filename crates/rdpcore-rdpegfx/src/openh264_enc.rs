//! OpenH264 software encoder backend.

use openh264::Timestamp;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, Profile, RateControlMode, UsageType,
};
use openh264::formats::YUVSource;

use crate::encoder::{EncodedAu, H264Encoder, align16, bgrx_to_i420};

/// Thin [`YUVSource`] over a contiguous I420 buffer produced by [`bgrx_to_i420`].
struct I420Frame<'a> {
    width: usize,
    height: usize,
    data: &'a [u8],
}

impl YUVSource for I420Frame<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        let y_size = self.width * self.height;
        &self.data[..y_size]
    }

    fn u(&self) -> &[u8] {
        let y_size = self.width * self.height;
        let uv_size = (self.width / 2) * (self.height / 2);
        &self.data[y_size..y_size + uv_size]
    }

    fn v(&self) -> &[u8] {
        let y_size = self.width * self.height;
        let uv_size = (self.width / 2) * (self.height / 2);
        &self.data[y_size + uv_size..y_size + 2 * uv_size]
    }
}

pub struct OpenH264Encoder {
    inner: Option<Encoder>,
    coded_w: u16,
    coded_h: u16,
    qp: u8,
    /// Monotonic frame time for OpenH264 RC (must not stay at ZERO).
    next_ts_ms: u64,
}

impl OpenH264Encoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            inner: None,
            coded_w: 0,
            coded_h: 0,
            qp: 22,
            next_ts_ms: 0,
        })
    }

    fn ensure_encoder(&mut self, coded_w: u16, coded_h: u16) -> Result<&mut Encoder, String> {
        if self.inner.is_none() || self.coded_w != coded_w || self.coded_h != coded_h {
            // Desktop bitrates: ~1.5 bpp/s at 30fps floor, min 2 Mbps.
            let bps = (u32::from(coded_w) * u32::from(coded_h)).max(2_000_000);
            let config = EncoderConfig::new()
                .max_frame_rate(FrameRate::from_hz(30.0))
                .bitrate(BitRate::from_bps(bps))
                .usage_type(UsageType::CameraVideoRealTime)
                .profile(Profile::Baseline)
                // Quality RC + no skip: avoid empty bitstreams and unbounded
                // RC_OFF frames that blow past client limits (protocol error).
                .rate_control_mode(RateControlMode::Quality)
                .skip_frames(false)
                .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(30));
            let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
                .map_err(|e| format!("openh264 init: {e}"))?;
            self.inner = Some(enc);
            self.coded_w = coded_w;
            self.coded_h = coded_h;
            self.next_ts_ms = 0;
        }
        self.inner
            .as_mut()
            .ok_or_else(|| "openh264 encoder missing".to_string())
    }
}

impl Default for OpenH264Encoder {
    fn default() -> Self {
        Self::new().expect("openh264 default init")
    }
}

impl H264Encoder for OpenH264Encoder {
    fn encode_bgrx(
        &mut self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
        force_idr: bool,
    ) -> Result<EncodedAu, String> {
        if width == 0 || height == 0 {
            return Err("empty frame".into());
        }
        let coded_w = align16(width).max(16);
        let coded_h = align16(height).max(16);
        let i420 = bgrx_to_i420(width, height, stride, pixels, coded_w, coded_h)?;
        let frame = I420Frame {
            width: usize::from(coded_w),
            height: usize::from(coded_h),
            data: &i420,
        };

        let ts_ms = self.next_ts_ms;
        self.next_ts_ms = self.next_ts_ms.saturating_add(33);

        let enc = self.ensure_encoder(coded_w, coded_h)?;
        if force_idr {
            enc.force_intra_frame();
        }
        let bitstream = enc
            .encode_at(&frame, Timestamp::from_millis(ts_ms))
            .map_err(|e| format!("openh264 encode: {e}"))?;

        // With skip_frames(false) this should be rare; still treat Skip as soft
        // failure so the session can force an IDR retry instead of Planar thrash.
        if matches!(bitstream.frame_type(), openh264::encoder::FrameType::Skip) {
            return Err("openh264 skipped frame".into());
        }

        let annex_b = bitstream.to_vec();
        if annex_b.is_empty() {
            return Err("openh264 returned empty bitstream".into());
        }
        Ok(EncodedAu {
            annex_b,
            qp: self.qp,
        })
    }

    fn reset(&mut self) {
        self.inner = None;
        self.coded_w = 0;
        self.coded_h = 0;
        self.next_ts_ms = 0;
    }
}
