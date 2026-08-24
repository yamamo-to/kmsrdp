//! DRM/KMS screen capture without any compositor cooperation.
//!
//! Mirrors what `reframe-streamer/main.c` (upstream ReFrame) does: open the
//! card read-only, find the primary plane of an active CRTC, and export its
//! current framebuffer via `drmPrimeHandleToFD`. A Linear XRGB8888/ARGB8888
//! framebuffer is read back with a plain CPU mmap; a tiled (vendor-modifier)
//! one of the same formats goes through [`crate::gpu_detile`] instead.

use std::io;

mod display_mode;
mod dmabuf;
mod drm_capturer;
mod drm_discover;
mod pixel_diff;
mod types;

#[cfg(test)]
pub(crate) use display_mode::DisplayMode;
pub use display_mode::validate_display_env;
pub use drm_capturer::DrmCapturer;
#[cfg(test)]
pub(crate) use drm_capturer::{
    CapturedHead, HEAD_REFRESH_INTERVAL, compose_heads, should_refresh_heads,
};
#[cfg(test)]
pub(crate) use pixel_diff::blit_bgrx;
pub use types::{CaptureCompare, MonitorGeom, RawFrame};

/// Stateful screen capturer. The DRM card fd stays open for this object's
/// lifetime so the capture loop never repeatedly becomes DRM master while
/// Xorg is exiting and fbcon is trying to restore the text console.
pub struct Capturer {
    drm: Option<DrmCapturer>,
    drm_open_error: Option<String>,
    /// Set after the first successful frame so logs can name the backend.
    active_backend: Option<&'static str>,
}

impl Capturer {
    pub fn new() -> io::Result<Self> {
        match DrmCapturer::open() {
            Ok(drm) => Ok(Self {
                drm: Some(drm),
                drm_open_error: None,
                active_backend: None,
            }),
            Err(drm_err) if display_mode::display_mode()?.is_single() => {
                Err(annotate_capture_error(drm_err, CapturePhase::Open))
            }
            Err(drm_err) => {
                tracing::warn!(
                    "kmsrdp: DRM/KMS unavailable ({drm_err}); will try NVIDIA NvFBC on capture"
                );
                Ok(Self {
                    drm: None,
                    drm_open_error: Some(drm_err.to_string()),
                    active_backend: None,
                })
            }
        }
    }

    pub fn capture(&mut self) -> io::Result<RawFrame> {
        self.capture_with_hint(None)
    }

    /// Like [`capture`](Self::capture), but skips the framebuffer `Arc`
    /// allocation when `prev` is the same pixels (static desktop).
    pub fn capture_with_hint(&mut self, prev: Option<CaptureCompare<'_>>) -> io::Result<RawFrame> {
        let drm_error = match &mut self.drm {
            Some(drm) => match drm.capture(prev) {
                Ok(frame) => {
                    self.note_backend("DRM/KMS");
                    return Ok(frame);
                }
                Err(drm_err) if display_mode::display_mode()?.is_single() => {
                    return Err(annotate_capture_error(drm_err, CapturePhase::Frame));
                }
                Err(drm_err) => {
                    // Transient DRM failure with All mode: try NvFBC this tick.
                    drm_err.to_string()
                }
            },
            None => self
                .drm_open_error
                .clone()
                .unwrap_or_else(|| "DRM/KMS capturer unavailable".to_string()),
        };

        match crate::nvfbc::capture_bgrx() {
            Ok((width, height, grabbed)) => {
                let stride = match check_nvfbc_frame_len(width, height, grabbed.len()) {
                    Ok(stride) => stride,
                    Err(expected_len) => {
                        // NvFBC's ToSys grab is documented to return a
                        // tightly packed buffer (see `nvfbc::capture_bgrx`'s
                        // doc comment); we trust that contract for `stride`
                        // since there's no independent pitch field to
                        // cross-check it against. If a driver version or
                        // config ever violates it, treating the mismatched
                        // bytes as tightly-packed BGRX would misalign every
                        // row after the first - fail loudly instead of
                        // handing the encoder a sheared frame it has no way
                        // to detect.
                        tracing::warn!(
                            "kmsrdp: NvFBC returned {} bytes for a {width}x{height} tightly-packed \
                             BGRX frame (expected {expected_len}); treating as a capture failure",
                            grabbed.len()
                        );
                        return Err(annotate_capture_error(
                            io::Error::other(format!(
                                "NvFBC frame size mismatch: got {} bytes, expected {expected_len} \
                                 for {width}x{height} BGRX",
                                grabbed.len()
                            )),
                            CapturePhase::Frame,
                        ));
                    }
                };
                self.note_backend("NvFBC");
                let (data, unchanged, dirty_rects) =
                    pixel_diff::take_pixels(&grabbed, stride, width, height, false, prev);
                Ok(RawFrame {
                    width,
                    height,
                    stride,
                    data,
                    force_full: false,
                    unchanged,
                    dirty_rects,
                    monitors: vec![MonitorGeom {
                        left: 0,
                        top: 0,
                        right: width.saturating_sub(1) as i32,
                        bottom: height.saturating_sub(1) as i32,
                        primary: true,
                    }],
                })
            }
            Err(nvfbc_err) => Err(annotate_capture_error(
                io::Error::other(format!(
                    "DRM/KMS capture failed ({drm_error}); NvFBC fallback also failed ({nvfbc_err})"
                )),
                CapturePhase::Frame,
            )),
        }
    }

    fn note_backend(&mut self, backend: &'static str) {
        if self.active_backend != Some(backend) {
            tracing::info!("kmsrdp: screen capture backend: {backend}");
            self.active_backend = Some(backend);
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CapturePhase {
    Open,
    Frame,
}

/// Attach short, actionable hints so journal/console logs explain a black
/// screen instead of a bare I/O error.
pub(crate) fn annotate_capture_error(err: io::Error, phase: CapturePhase) -> io::Error {
    let msg = err.to_string();
    let mut hints: Vec<&str> = Vec::new();

    let lower = msg.to_lowercase();
    if lower.contains("no usable card")
        || lower.contains("not an active drm connector")
        || lower.contains("no active connector")
        || lower.contains("connected, inactive")
    {
        hints.push(
            "no CRTC is scanning out — wake the session, plug in a display, or unset a bad KMSRDP_DISPLAY",
        );
    }
    if lower.contains("no framebuffer") || lower.contains("screen off") {
        hints.push(
            "primary plane has no FB (VT switched away, screen locked/off, or compositor idle)",
        );
    }
    if lower.contains("no primary plane") {
        hints.push("CRTC has no primary plane — driver/modeset may still be bringing the head up");
    }
    if lower.contains("libnvidia-fbc") || lower.contains("nvfbc") {
        hints.push(
            "NvFBC needs an NVIDIA driver with libnvidia-fbc and a usable X/Wayland session on that GPU",
        );
    }
    if lower.contains("permission")
        || lower.contains("permission denied")
        || lower.contains("eacces")
    {
        hints.push("missing CAP_SYS_ADMIN / CAP_DAC_OVERRIDE (or root) to open DRM nodes");
    }
    if hints.is_empty() {
        match phase {
            CapturePhase::Open => hints.push(
                "could not open a capture source — check dmesg/journal for DRM errors and KMSRDP_DISPLAY",
            ),
            CapturePhase::Frame => hints.push(
                "frame grab failed — clients may stay black until capture recovers",
            ),
        }
    }

    io::Error::new(err.kind(), format!("{msg} (hint: {})", hints.join("; ")))
}

/// Checks that an NvFBC grab's byte length matches a tightly packed
/// `width`x`height` BGRX8888 buffer. Returns the stride (`width * 4`) on a
/// match, or the expected length on a mismatch.
fn check_nvfbc_frame_len(width: u32, height: u32, len: usize) -> Result<usize, usize> {
    let stride = width as usize * 4;
    let expected_len = stride.saturating_mul(height as usize);
    if len == expected_len {
        Ok(stride)
    } else {
        Err(expected_len)
    }
}

/// One-shot compatibility helper for demos and diagnostics. The production
/// display loop owns one [`Capturer`] and reuses it instead.
pub fn capture_raw_bgrx() -> io::Result<RawFrame> {
    Capturer::new()?.capture()
}

/// Same capture, decoded into an RGB image (for the PNG demo binaries).
pub fn capture_frame() -> io::Result<image::RgbImage> {
    let raw = capture_raw_bgrx()?;
    let mut img = image::RgbImage::new(raw.width, raw.height);
    for y in 0..raw.height as usize {
        let row = &raw.data[y * raw.stride..y * raw.stride + raw.width as usize * 4];
        for x in 0..raw.width as usize {
            let px = &row[x * 4..x * 4 + 4];
            // DRM_FORMAT_XRGB8888/ARGB8888 in memory (little-endian) is B,G,R,X/A.
            img.put_pixel(x as u32, y as u32, image::Rgb([px[2], px[1], px[0]]));
        }
    }
    Ok(img)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn check_nvfbc_frame_len_accepts_tightly_packed_bgrx() {
        assert_eq!(
            check_nvfbc_frame_len(1920, 1080, 1920 * 1080 * 4),
            Ok(1920 * 4)
        );
        assert_eq!(check_nvfbc_frame_len(0, 0, 0), Ok(0));
    }

    #[test]
    fn check_nvfbc_frame_len_rejects_short_or_padded_buffers() {
        // Shorter than expected (e.g. a driver returning a partial frame).
        assert_eq!(
            check_nvfbc_frame_len(1920, 1080, 1920 * 1080 * 4 - 1),
            Err(1920 * 1080 * 4)
        );
        // Longer than expected (e.g. a driver padding rows to some
        // alignment instead of the documented tightly-packed layout).
        assert_eq!(
            check_nvfbc_frame_len(1920, 1080, 1920 * 1080 * 4 + 1),
            Err(1920 * 1080 * 4)
        );
    }

    #[test]
    fn empty_or_all_display_mode_composites_all() {
        assert_eq!(DisplayMode::parse("").unwrap(), DisplayMode::All);
        assert_eq!(DisplayMode::parse("  ").unwrap(), DisplayMode::All);
        assert_eq!(DisplayMode::parse("all").unwrap(), DisplayMode::All);
        assert_eq!(DisplayMode::parse("ALL").unwrap(), DisplayMode::All);
    }

    #[test]
    fn connector_only_selector_matches_any_card() {
        let DisplayMode::Single(selector) = DisplayMode::parse(" DP-1 ").unwrap() else {
            panic!("expected single");
        };
        assert!(selector.matches("card0", "DP-1"));
        assert!(selector.matches("card1", "DP-1"));
        assert!(!selector.matches("card0", "HDMI-A-1"));
    }

    #[test]
    fn qualified_selector_matches_one_card_and_connector() {
        let DisplayMode::Single(selector) = DisplayMode::parse("card1:DP-1").unwrap() else {
            panic!("expected single");
        };
        assert!(selector.matches("card1", "DP-1"));
        assert!(!selector.matches("card0", "DP-1"));
        assert!(!selector.matches("card1", "DP-2"));
    }

    #[test]
    fn malformed_qualified_selector_is_rejected() {
        assert!(DisplayMode::parse(":DP-1").is_err());
        assert!(DisplayMode::parse("card0:").is_err());
        assert!(DisplayMode::parse("card0:DP-1:extra").is_err());
    }

    #[test]
    fn compose_two_heads_side_by_side() {
        let left = CapturedHead {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 0, 0, 0, 2, 0, 0, 0].into(),
            force_full: false,
            unchanged: false,
            dirty_rects: None,
            connector: "A".into(),
        };
        let right = CapturedHead {
            x: 2,
            y: 0,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![3, 0, 0, 0, 4, 0, 0, 0].into(),
            force_full: true,
            unchanged: false,
            dirty_rects: None,
            connector: "B".into(),
        };
        let frame = compose_heads(&[left, right], None);
        assert_eq!((frame.width, frame.height), (4, 1));
        assert!(frame.force_full);
        assert_eq!(frame.data[0], 1);
        assert_eq!(frame.data[4], 2);
        assert_eq!(frame.data[8], 3);
        assert_eq!(frame.data[12], 4);
        assert_eq!(frame.monitors.len(), 2);
        assert!(frame.monitors[0].primary);
        assert!(!frame.monitors[1].primary);
        assert_eq!(frame.monitors[1].left, 2);
        assert_eq!(frame.monitors[1].right, 3);
    }

    #[test]
    fn compose_heads_marks_unchanged_when_prev_matches() {
        let left = CapturedHead {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![1, 0, 0, 0, 2, 0, 0, 0].into(),
            force_full: false,
            unchanged: false,
            dirty_rects: None,
            connector: "A".into(),
        };
        let right = CapturedHead {
            x: 2,
            y: 0,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![3, 0, 0, 0, 4, 0, 0, 0].into(),
            force_full: false,
            unchanged: false,
            dirty_rects: None,
            connector: "B".into(),
        };
        let first = compose_heads(&[left.clone(), right.clone()], None);
        assert!(!first.unchanged);
        let hint = CaptureCompare {
            width: first.width,
            height: first.height,
            stride: first.stride,
            data: &first.data,
        };
        let second = compose_heads(&[left, right], Some(hint));
        assert!(second.unchanged);
        assert!(second.data.is_empty());
        assert_eq!(second.dirty_rects, Some(Vec::new()));
    }

    #[test]
    fn annotate_mentions_crtc_hint() {
        let err = annotate_capture_error(
            io::Error::new(
                io::ErrorKind::NotFound,
                "no usable card/connector/CRTC found (is a display actually active?); discovered DRM connectors: none",
            ),
            CapturePhase::Open,
        );
        let msg = err.to_string();
        assert!(msg.contains("hint:"), "{msg}");
        assert!(
            msg.contains("CRTC") || msg.contains("crtc") || msg.contains("KMSRDP_DISPLAY"),
            "{msg}"
        );
    }

    #[test]
    fn annotate_mentions_nvfbc_hint() {
        let err = annotate_capture_error(
            io::Error::other(
                "DRM/KMS capture failed (x); NvFBC fallback also failed (failed to load libnvidia-fbc: ...)",
            ),
            CapturePhase::Frame,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("libnvidia-fbc") || msg.contains("NvFBC"),
            "{msg}"
        );
        assert!(msg.contains("hint:"), "{msg}");
    }

    #[test]
    fn blit_bgrx_copies_visible_region() {
        let src = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = vec![0u8; 16];
        blit_bgrx(&mut dst, 8, 2, 2, &src, 8, 2, 1, 0, 0);
        assert_eq!(&dst[0..4], &[1, 2, 3, 4]);
        assert_eq!(&dst[4..8], &[5, 6, 7, 8]);
    }

    #[test]
    fn blit_bgrx_clips_negative_destination() {
        let src = vec![9u8, 8, 7, 6, 1, 2, 3, 4];
        let mut dst = vec![0u8; 8];
        blit_bgrx(&mut dst, 8, 2, 1, &src, 8, 2, 1, -1, 0);
        assert_eq!(&dst[0..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn blit_bgrx_skips_out_of_bounds_rows() {
        let src = vec![1u8; 4];
        let mut dst = vec![0u8; 8];
        blit_bgrx(&mut dst, 4, 1, 1, &src, 4, 1, 1, 0, 5);
        assert_eq!(dst, vec![0u8; 8]);
    }

    #[test]
    fn take_pixels_reuses_nothing_when_frames_match() {
        let frame = vec![7u8; 16];
        let prev = CaptureCompare {
            width: 2,
            height: 2,
            stride: 8,
            data: &frame,
        };
        let (data, unchanged, dirty) = pixel_diff::take_pixels(&frame, 8, 2, 2, false, Some(prev));
        assert!(unchanged);
        assert!(data.is_empty());
        assert_eq!(dirty, Some(Vec::new()));
    }

    #[test]
    fn take_pixels_copies_when_a_pixel_changes() {
        let prev_bytes = vec![0u8; 16];
        let mut src = prev_bytes.clone();
        src[0] = 1;
        let prev = CaptureCompare {
            width: 2,
            height: 2,
            stride: 8,
            data: &prev_bytes,
        };
        let (data, unchanged, dirty) = pixel_diff::take_pixels(&src, 8, 2, 2, false, Some(prev));
        assert!(!unchanged);
        assert_eq!(&data[..], &src[..]);
        assert!(dirty.as_ref().is_some_and(|r| !r.is_empty()));
    }

    #[test]
    fn take_pixels_force_full_always_copies() {
        let frame = vec![3u8; 16];
        let prev = CaptureCompare {
            width: 2,
            height: 2,
            stride: 8,
            data: &frame,
        };
        let (data, unchanged, dirty) = pixel_diff::take_pixels(&frame, 8, 2, 2, true, Some(prev));
        assert!(!unchanged);
        assert_eq!(&data[..], &frame[..]);
        assert!(dirty.is_none());
    }

    #[test]
    fn head_refresh_throttles_until_interval_elapses() {
        let t0 = Instant::now();
        assert!(!should_refresh_heads(
            Some(t0),
            t0 + Duration::from_millis(10),
            false,
            false
        ));
        assert!(should_refresh_heads(
            Some(t0),
            t0 + HEAD_REFRESH_INTERVAL,
            false,
            false
        ));
        assert!(should_refresh_heads(Some(t0), t0, true, false));
        assert!(should_refresh_heads(Some(t0), t0, false, true));
        assert!(should_refresh_heads(None, t0, false, false));
    }
}
