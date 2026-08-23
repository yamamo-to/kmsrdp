//! Display/graphics traits - shaped closely after `ironrdp-server`'s own
//! `RdpServerDisplay`/`BitmapUpdate` so callers migrating from it (like
//! kmsrdp) only need to change import paths, not logic.

use core::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopSize {
    pub width: u16,
    pub height: u16,
}

/// Inclusive monitor rectangle in the virtual desktop (for Monitor Layout PDU).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorLayoutEntry {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// Currently only raw BGRX8888 - the same in-memory layout
/// `kmsrdp::capture` already reads straight off DRM/KMS, so no per-pixel
/// conversion is needed on the way in (compressed codecs would be additive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    BgrX32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapUpdate {
    pub x: u16,
    pub y: u16,
    pub width: NonZeroU16,
    pub height: NonZeroU16,
    pub format: PixelFormat,
    /// Row-major, top-down pixel bytes. Shared via [`Arc`] so the capture
    /// loop can publish one full frame to `latest_full` and to subscribers
    /// without cloning megabytes of framebuffer data.
    ///
    /// [`BitmapUpdate::sub`] clones this `Arc` and keeps the parent
    /// `stride`; the view's top-left pixel is at ([`src_x`], [`src_y`]).
    /// The wire encoder indexes through those fields instead of
    /// re-packing the rectangle.
    pub data: Arc<[u8]>,
    pub stride: NonZeroUsize,
    /// Horizontal pixel offset of this update's origin inside [`data`].
    pub src_x: u16,
    /// Vertical pixel offset of this update's origin inside [`data`].
    pub src_y: u16,
}

impl BitmapUpdate {
    /// Byte offset of the pixel `x`/`y` relative to this update's origin.
    pub(crate) fn src_byte_offset(&self, x: u16, y: u16) -> usize {
        const BYTES_PER_PIXEL: usize = 4;
        (usize::from(self.src_y) + usize::from(y))
            .saturating_mul(self.stride.get())
            .saturating_add(
                (usize::from(self.src_x) + usize::from(x)).saturating_mul(BYTES_PER_PIXEL),
            )
    }

    /// View of a sub-rectangle that shares [`data`] (no framebuffer copy).
    ///
    /// The returned update keeps the parent stride and records the
    /// region's origin in [`src_x`]/[`src_y`] so encoders can address
    /// padded rows without a tightly-packed clone.
    pub fn sub(
        &self,
        x: u16,
        y: u16,
        width: NonZeroU16,
        height: NonZeroU16,
    ) -> Option<BitmapUpdate> {
        const BYTES_PER_PIXEL: usize = 4;
        let (bx, by) = (usize::from(x), usize::from(y));
        let (bw, bh) = (usize::from(width.get()), usize::from(height.get()));
        if bx + bw > usize::from(self.width.get()) || by + bh > usize::from(self.height.get()) {
            return None;
        }

        let src_x = self.src_x.checked_add(x)?;
        let src_y = self.src_y.checked_add(y)?;
        let last = (usize::from(src_y) + bh.saturating_sub(1))
            .saturating_mul(self.stride.get())
            .saturating_add((usize::from(src_x) + bw).saturating_mul(BYTES_PER_PIXEL));
        if last > self.data.len() {
            return None;
        }

        Some(BitmapUpdate {
            x: self.x.checked_add(x)?,
            y: self.y.checked_add(y)?,
            width,
            height,
            format: self.format,
            data: Arc::clone(&self.data),
            stride: self.stride,
            src_x,
            src_y,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayUpdate {
    Bitmap(BitmapUpdate),
    /// The real desktop this server mirrors changed size (e.g. a VM
    /// console got resized) - not a client request. The steady-state loop
    /// reacts by driving a server-initiated Deactivate-All + re-activation
    /// (see `rdpcore_connector::Acceptor::begin_resize`) so the client's
    /// view follows.
    Resized(DesktopSize),
}

#[async_trait]
pub trait RdpServerDisplay: Send + Sync {
    async fn size(&self) -> DesktopSize;
    async fn updates(&self)
    -> Result<Box<dyn RdpServerDisplayUpdates>, crate::error::DisplayError>;

    /// Host monitor layout for the current virtual desktop (≥1 when known).
    fn monitor_layout(&self) -> Vec<MonitorLayoutEntry> {
        Vec::new()
    }
}

#[async_trait]
pub trait RdpServerDisplayUpdates: Send {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>, crate::error::DisplayError>;

    /// Latest full-desktop frame, if the backend keeps one (for Refresh Rect).
    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn full_frame(width: u16, height: u16, fill: u8) -> BitmapUpdate {
        let stride = NonZeroUsize::new(usize::from(width) * 4).unwrap();
        BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            format: PixelFormat::BgrX32,
            data: Arc::from(vec![fill; stride.get() * usize::from(height)]),
            stride,
            src_x: 0,
            src_y: 0,
        }
    }

    #[test]
    fn sub_shares_backing_buffer_and_records_source_origin() {
        let full = full_frame(128, 64, 7);
        let sub = full
            .sub(
                64,
                16,
                NonZeroU16::new(32).unwrap(),
                NonZeroU16::new(16).unwrap(),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&full.data, &sub.data));
        assert_eq!(sub.stride, full.stride);
        assert_eq!((sub.x, sub.y), (64, 16));
        assert_eq!((sub.src_x, sub.src_y), (64, 16));
        assert_eq!(sub.width.get(), 32);
        assert_eq!(sub.height.get(), 16);
        assert_eq!(sub.src_byte_offset(0, 0), 16 * full.stride.get() + 64 * 4);
    }

    #[test]
    fn nested_sub_accumulates_source_origin() {
        let full = full_frame(128, 64, 1);
        let mid = full
            .sub(
                64,
                16,
                NonZeroU16::new(48).unwrap(),
                NonZeroU16::new(32).unwrap(),
            )
            .unwrap();
        let inner = mid
            .sub(
                8,
                8,
                NonZeroU16::new(16).unwrap(),
                NonZeroU16::new(8).unwrap(),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&full.data, &inner.data));
        assert_eq!((inner.src_x, inner.src_y), (72, 24));
        assert_eq!((inner.x, inner.y), (72, 24));
    }

    #[test]
    fn sub_rejects_out_of_bounds_rect() {
        let full = full_frame(64, 64, 0);
        assert!(
            full.sub(
                32,
                32,
                NonZeroU16::new(48).unwrap(),
                NonZeroU16::new(16).unwrap()
            )
            .is_none()
        );
    }
}
