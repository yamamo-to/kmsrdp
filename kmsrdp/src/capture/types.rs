use std::sync::Arc;
use rdpcore_server::diff::Rect;

/// Inclusive monitor rectangle in the composited virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorGeom {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// A raw BGRX8888 frame straight out of DRM, before any pixel-format
/// conversion. `stride` may be larger than `width * 4` (row alignment
/// padding); the RDP path passes it straight through instead of repacking.
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Arc<[u8]>,
    /// True when the DRM primary plane swapped to a different framebuffer
    /// object (e.g. Xorg exited and fbcon restored the console FB). Existing
    /// RDP clients only receive dirty-rect updates, so a scene change that
    /// happens without a large pixel-diff in one tick would leave them
    /// showing the previous tiles; the display hub treats this as a
    /// mandatory full-frame refresh.
    pub force_full: bool,
    /// Monitor layout relative to this frame's origin (always ≥1 entry).
    pub monitors: Vec<MonitorGeom>,
    /// True when pixels match `Capturer::capture_with_hint`'s previous
    /// frame; `data` is empty and the caller should keep its last buffer.
    pub unchanged: bool,
    /// Dirty rectangles computed while comparing against the previous
    /// frame (so the display hub does not scan again). `None` means the
    /// caller should treat the frame as a full-desktop update.
    pub dirty_rects: Option<Vec<Rect>>,
}

/// Previous-frame pixels the capturer can compare against before copying.
#[derive(Clone, Copy)]
pub struct CaptureCompare<'a> {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: &'a [u8],
}
