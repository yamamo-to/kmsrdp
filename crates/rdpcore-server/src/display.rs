//! Display/graphics traits - shaped closely after `ironrdp-server`'s own
//! `RdpServerDisplay`/`BitmapUpdate` so callers migrating from it (like
//! kmsrdp) only need to change import paths, not logic.

use core::num::{NonZeroU16, NonZeroUsize};

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopSize {
    pub width: u16,
    pub height: u16,
}

/// Phase 1 only ever produces raw BGRX8888 - the same in-memory layout
/// `kmsrdp::capture` already reads straight off DRM/KMS, so no per-pixel
/// conversion is needed on the way in (a real codec is a later, additive
/// phase - see the crate's design notes).
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
    /// Row-major, top-down, tightly packed (`stride == width * 4`) once
    /// this came out of [`BitmapUpdate::sub`] - the wire encoder relies on
    /// that (it only reverses rows for the bottom-up wire convention, it
    /// doesn't re-pack arbitrary strides).
    pub data: Vec<u8>,
    pub stride: NonZeroUsize,
}

impl BitmapUpdate {
    /// Extracts a sub-rectangle, re-packing it tightly (`stride = width *
    /// 4`) regardless of `self`'s own stride - so downstream wire encoding
    /// never has to deal with row padding.
    pub fn sub(&self, x: u16, y: u16, width: NonZeroU16, height: NonZeroU16) -> Option<BitmapUpdate> {
        const BYTES_PER_PIXEL: usize = 4;
        let (bx, by) = (usize::from(x), usize::from(y));
        let (bw, bh) = (usize::from(width.get()), usize::from(height.get()));
        if bx + bw > usize::from(self.width.get()) || by + bh > usize::from(self.height.get()) {
            return None;
        }

        let mut data = Vec::with_capacity(bw * BYTES_PER_PIXEL * bh);
        for row in 0..bh {
            let start = (by + row) * self.stride.get() + bx * BYTES_PER_PIXEL;
            data.extend_from_slice(&self.data[start..start + bw * BYTES_PER_PIXEL]);
        }

        Some(BitmapUpdate {
            x: self.x + x,
            y: self.y + y,
            width,
            height,
            format: self.format,
            data,
            stride: NonZeroUsize::new(bw * BYTES_PER_PIXEL)?,
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
    async fn updates(&self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>>;
}

#[async_trait]
pub trait RdpServerDisplayUpdates: Send {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>>;
}
