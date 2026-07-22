//! Single-process DRM/KMS capture loop that fans display updates out to
//! every connected RDP session. Capture is a singleton system resource;
//! concurrent clients subscribe instead of each spawning their own loop.

use core::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use rdpcore_server::diff::{Rect, find_dirty_rects};
use rdpcore_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, MonitorLayoutEntry, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tokio::sync::broadcast;

use crate::capture;

/// Desktop dimensions mouse coordinates are normalized against - shared
/// with input so a resize updates both sides together.
pub type MouseScale = Arc<Mutex<(f64, f64)>>;

const BROADCAST_CAPACITY: usize = 256;

/// When more than this fraction of the frame is dirty, send one full-frame
/// update instead of many tile updates. Large scene changes (logout, VT
/// switch, app fullscreen) otherwise flood the broadcast channel; lagged
/// subscribers would then keep stale tiles forever. 1/16 (~6%) is low enough
/// that an X-session → console transition still forces a full refresh even
/// when only part of the wallpaper is overwritten in a single tick.
const FULL_FRAME_DIRTY_RATIO_NUM: usize = 1;
const FULL_FRAME_DIRTY_RATIO_DEN: usize = 16;

pub struct DisplayHub {
    size: Mutex<DesktopSize>,
    tx: broadcast::Sender<DisplayUpdate>,
    mouse_scale: MouseScale,
    /// Latest full-frame bitmap, so a late or lagged subscriber can paint
    /// a consistent canvas instead of waiting for the next dirty-rect change.
    latest_full: Arc<Mutex<Option<BitmapUpdate>>>,
    monitors: Mutex<Vec<MonitorLayoutEntry>>,
}

impl DisplayHub {
    /// Starts the capture loop immediately and returns a shareable hub.
    pub fn start(
        width: u16,
        height: u16,
        mouse_scale: MouseScale,
        capturer: capture::Capturer,
        monitors: Vec<MonitorLayoutEntry>,
    ) -> Arc<Self> {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let hub = Arc::new(Self {
            size: Mutex::new(DesktopSize { width, height }),
            tx,
            mouse_scale,
            latest_full: Arc::new(Mutex::new(None)),
            monitors: Mutex::new(monitors),
        });
        let capture_hub = Arc::clone(&hub);
        tokio::spawn(async move {
            capture_hub.run_capture_loop(capturer).await;
        });
        hub
    }

    pub fn subscribe(&self) -> DisplayUpdates {
        DisplayUpdates {
            initial: self.latest_full.lock().unwrap().clone(),
            latest_full: Arc::clone(&self.latest_full),
            rx: self.tx.subscribe(),
        }
    }

    async fn run_capture_loop(self: Arc<Self>, mut capturer: capture::Capturer) {
        /// Prior frame pixels shared with `latest_full` via [`Arc`] so the
        /// dirty-rect pass does not need a second framebuffer copy.
        struct PrevFrame {
            width: u32,
            height: u32,
            stride: usize,
            data: Arc<[u8]>,
        }
        let mut previous: Option<PrevFrame> = None;
        let mut negotiated_size = *self.size.lock().unwrap();
        let mut consecutive_failures: u32 = 0;
        loop {
            let task = tokio::task::spawn_blocking(move || {
                let result = capturer.capture();
                (capturer, result)
            })
            .await;
            let result = match task {
                Ok((returned, result)) => {
                    capturer = returned;
                    result
                }
                Err(e) => {
                    tracing::error!("kmsrdp: capture task panicked: {e}");
                    match tokio::task::spawn_blocking(capture::Capturer::new).await {
                        Ok(Ok(replacement)) => {
                            capturer = replacement;
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                        Ok(Err(open_err)) => {
                            tracing::error!(
                                "kmsrdp: failed to reopen capturer: {open_err}; \
                                 stopping capture loop (clients will stay black);"
                            );
                            return;
                        }
                        Err(open_err) => {
                            tracing::error!(
                                "kmsrdp: capturer reopen task panicked: {open_err}; \
                                 stopping capture loop"
                            );
                            return;
                        }
                    }
                }
            };

            match result {
                Ok(raw) => {
                    if consecutive_failures > 0 {
                        tracing::warn!(
                            "kmsrdp: capture recovered after {consecutive_failures} failure(s);"
                        );
                        consecutive_failures = 0;
                    }
                    *self.monitors.lock().unwrap() = raw
                        .monitors
                        .iter()
                        .map(|m| MonitorLayoutEntry {
                            left: m.left,
                            top: m.top,
                            right: m.right,
                            bottom: m.bottom,
                            primary: m.primary,
                        })
                        .collect();
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
                        *self.mouse_scale.lock().unwrap() = (
                            f64::from(current_size.width),
                            f64::from(current_size.height),
                        );
                        // Forget the previous frame so the next capture is
                        // forced through as a full-frame update after the
                        // client confirms the new desktop size.
                        previous = None;
                        *self.latest_full.lock().unwrap() = None;
                        let _ = self.tx.send(DisplayUpdate::Resized(current_size));
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }

                    let (Some(width), Some(height), Some(stride)) = (
                        NonZeroU16::new(raw.width as u16),
                        NonZeroU16::new(raw.height as u16),
                        NonZeroUsize::new(raw.stride),
                    ) else {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        if should_log_capture_failure(consecutive_failures) {
                            tracing::info!(
                                "kmsrdp: capture returned a zero-sized frame \
                                 ({}x{}, stride {}); clients stay black until a real frame arrives",
                                raw.width,
                                raw.height,
                                raw.stride
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };

                    let data: Arc<[u8]> = Arc::from(raw.data);
                    let full = BitmapUpdate {
                        x: 0,
                        y: 0,
                        width,
                        height,
                        format: PixelFormat::BgrX32,
                        data: Arc::clone(&data),
                        stride,
                    };

                    let dirty_rects = match &previous {
                        Some(prev)
                            if !raw.force_full
                                && prev.width == raw.width
                                && prev.height == raw.height =>
                        {
                            coalesce_dirty_rects(
                                find_dirty_rects(
                                    prev.data.as_ref(),
                                    prev.stride,
                                    data.as_ref(),
                                    raw.stride,
                                    raw.width as usize,
                                    raw.height as usize,
                                    4,
                                ),
                                raw.width as usize,
                                raw.height as usize,
                            )
                        }
                        _ => vec![Rect::new(0, 0, raw.width as usize, raw.height as usize)],
                    };

                    // Publish `latest_full` *before* broadcasting. A slow
                    // subscriber that lags mid-scene-change recovers from
                    // `latest_full`; if we updated it after the sends, that
                    // recovery would repaint the previous (e.g. X wallpaper)
                    // frame while the console update itself was among the
                    // dropped messages — then a static console produces no
                    // further dirty rects and the client stays stuck forever.
                    //
                    // `BitmapUpdate::data` is `Arc<[u8]>`, so this clone is
                    // cheap (no framebuffer memcpy).
                    *self.latest_full.lock().unwrap() = Some(full.clone());
                    previous = Some(PrevFrame {
                        width: raw.width,
                        height: raw.height,
                        stride: raw.stride,
                        data,
                    });

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
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if should_log_capture_failure(consecutive_failures) {
                        tracing::warn!(
                            "kmsrdp: capture failed ({consecutive_failures} consecutive);: {e}"
                        )
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Log the 1st failure, then every 20th, so a dead CRTC is obvious without
/// flooding the journal at ~20 Hz.
fn should_log_capture_failure(consecutive: u32) -> bool {
    consecutive == 1 || consecutive.is_multiple_of(20)
}

fn dirty_area(rects: &[Rect]) -> usize {
    rects
        .iter()
        .map(|rect| rect.width.saturating_mul(rect.height))
        .sum()
}

/// Collapse a heavily-dirty frame into a single full-frame rect so the
/// broadcast channel carries one update instead of dozens of tiles.
fn coalesce_dirty_rects(rects: Vec<Rect>, width: usize, height: usize) -> Vec<Rect> {
    if rects.is_empty() {
        return rects;
    }
    let frame_area = width.saturating_mul(height);
    if frame_area == 0 {
        return rects;
    }
    if dirty_area(&rects).saturating_mul(FULL_FRAME_DIRTY_RATIO_DEN)
        >= frame_area.saturating_mul(FULL_FRAME_DIRTY_RATIO_NUM)
    {
        vec![Rect::new(0, 0, width, height)]
    } else {
        rects
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
    /// Shared latest frame used to recover after broadcast lag.
    latest_full: Arc<Mutex<Option<BitmapUpdate>>>,
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
                // Missed updates leave the client canvas inconsistent.
                // Resync from `latest_full`, which the capture loop publishes
                // before broadcasting so a mid-send lag still recovers the
                // frame that was being sent (not the prior one).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(full) = self.latest_full.lock().unwrap().clone() {
                        return Ok(Some(DisplayUpdate::Bitmap(full)));
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(None),
            }
        }
    }

    fn latest_full_frame(&self) -> Option<BitmapUpdate> {
        self.latest_full.lock().unwrap().clone()
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

    fn monitor_layout(&self) -> Vec<MonitorLayoutEntry> {
        self.hub.monitors.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{coalesce_dirty_rects, dirty_area};
    use rdpcore_server::diff::Rect;

    #[test]
    fn light_dirty_frames_keep_individual_rects() {
        let rects = vec![Rect::new(0, 0, 64, 64), Rect::new(64, 0, 64, 64)];
        let coalesced = coalesce_dirty_rects(rects.clone(), 1920, 1080);
        assert_eq!(coalesced, rects);
    }

    #[test]
    fn heavily_dirty_frames_collapse_to_full_frame() {
        // One 1920x120 strip is ~6.25% of 1920x1080 — just above the 1/16
        // full-frame threshold.
        let rects = vec![Rect::new(0, 0, 1920, 120)];
        assert!(dirty_area(&rects) * 16 >= 1920 * 1080);
        assert_eq!(
            coalesce_dirty_rects(rects, 1920, 1080),
            vec![Rect::new(0, 0, 1920, 1080)]
        );
    }

    #[test]
    fn empty_dirty_frames_stay_empty() {
        assert!(coalesce_dirty_rects(Vec::new(), 1920, 1080).is_empty());
    }
}
