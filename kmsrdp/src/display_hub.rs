//! Single-process DRM/KMS capture loop that fans display updates out to
//! every connected RDP session. Capture is a singleton system resource;
//! concurrent clients subscribe instead of each spawning their own loop.

use core::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rdpcore_server::diff::find_dirty_rects;
use rdpcore_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
};
use tokio::sync::broadcast;

use crate::capture;

/// Desktop dimensions mouse coordinates are normalized against - shared
/// with input so a resize updates both sides together.
pub type MouseScale = Arc<Mutex<(f64, f64)>>;

const BROADCAST_CAPACITY: usize = 64;

pub struct DisplayHub {
    size: Mutex<DesktopSize>,
    tx: broadcast::Sender<DisplayUpdate>,
    mouse_scale: MouseScale,
    /// Latest full-frame bitmap, so a late subscriber can paint immediately
    /// instead of waiting for the next dirty-rect change.
    latest_full: Mutex<Option<BitmapUpdate>>,
}

impl DisplayHub {
    /// Starts the capture loop immediately and returns a shareable hub.
    pub fn start(width: u16, height: u16, mouse_scale: MouseScale) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let hub = Arc::new(Self {
            size: Mutex::new(DesktopSize { width, height }),
            tx,
            mouse_scale,
            latest_full: Mutex::new(None),
        });
        let capture_hub = Arc::clone(&hub);
        tokio::spawn(async move {
            capture_hub.run_capture_loop().await;
        });
        hub
    }

    pub fn subscribe(&self) -> DisplayUpdates {
        DisplayUpdates {
            initial: self.latest_full.lock().unwrap().clone(),
            rx: self.tx.subscribe(),
        }
    }

    async fn run_capture_loop(self: Arc<Self>) {
        let mut previous: Option<capture::RawFrame> = None;
        let mut negotiated_size = *self.size.lock().unwrap();
        loop {
            match tokio::task::spawn_blocking(capture::capture_raw_bgrx).await {
                Ok(Ok(raw)) => {
                    let current_size = DesktopSize {
                        width: raw.width as u16,
                        height: raw.height as u16,
                    };
                    if current_size != negotiated_size {
                        // The real desktop resized (e.g. a VM console
                        // got resized) - tell every subscriber and wait
                        // briefly before sending bitmaps at the new
                        // dimensions. Update the mouse scale now too.
                        negotiated_size = current_size;
                        *self.size.lock().unwrap() = current_size;
                        *self.mouse_scale.lock().unwrap() =
                            (f64::from(current_size.width), f64::from(current_size.height));
                        previous = Some(raw);
                        let _ = self.tx.send(DisplayUpdate::Resized(current_size));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }

                    let (Some(width), Some(height), Some(stride)) = (
                        NonZeroU16::new(raw.width as u16),
                        NonZeroU16::new(raw.height as u16),
                        NonZeroUsize::new(raw.stride),
                    ) else {
                        continue;
                    };

                    let full = BitmapUpdate {
                        x: 0,
                        y: 0,
                        width,
                        height,
                        format: PixelFormat::BgrX32,
                        data: raw.data.clone(),
                        stride,
                    };

                    let dirty_rects = match &previous {
                        Some(prev) if prev.width == raw.width && prev.height == raw.height => {
                            find_dirty_rects(
                                &prev.data,
                                prev.stride,
                                &raw.data,
                                raw.stride,
                                raw.width as usize,
                                raw.height as usize,
                                4,
                            )
                        }
                        _ => vec![rdpcore_server::diff::Rect::new(
                            0,
                            0,
                            raw.width as usize,
                            raw.height as usize,
                        )],
                    };

                    for rect in &dirty_rects {
                        let (Some(w), Some(h)) = (
                            NonZeroU16::new(rect.width as u16),
                            NonZeroU16::new(rect.height as u16),
                        ) else {
                            continue;
                        };
                        let Some(sub) = full.sub(rect.x as u16, rect.y as u16, w, h) else {
                            continue;
                        };
                        let _ = self.tx.send(DisplayUpdate::Bitmap(sub));
                    }
                    *self.latest_full.lock().unwrap() = Some(full);
                    previous = Some(raw);
                }
                Ok(Err(e)) => eprintln!("capture failed: {e}"),
                Err(e) => eprintln!("capture task panicked: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Thin `RdpServerDisplay` handle around a shared [`DisplayHub`].
pub struct Display {
    hub: Arc<DisplayHub>,
}

impl Display {
    pub fn new(hub: Arc<DisplayHub>) -> Self {
        Self { hub }
    }
}

pub struct DisplayUpdates {
    /// One-shot full frame for late joiners (taken at subscribe time).
    initial: Option<BitmapUpdate>,
    rx: broadcast::Receiver<DisplayUpdate>,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for DisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if let Some(full) = self.initial.take() {
            return Ok(Some(DisplayUpdate::Bitmap(full)));
        }
        loop {
            match self.rx.recv().await {
                Ok(update) => return Ok(Some(update)),
                // Lagged subscribers skip missed frames and keep going;
                // a closed channel ends the connection's display stream.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&self) -> DesktopSize {
        *self.hub.size.lock().unwrap()
    }

    async fn updates(&self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(self.hub.subscribe()))
    }
}
