use std::collections::HashMap;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use drm::control::{Device as ControlDevice, crtc, plane};
use drm_fourcc::{DrmFourcc, DrmModifier};
use memmap2::MmapOptions;

use super::dmabuf::{dma_buf_sync_end, dma_buf_sync_start};
use super::drm_discover::{
    CardCtx, EnumeratedHead, open_drm_cards_and_heads, plane_type, refresh_heads,
};
use super::pixel_diff::{blit_bgrx, take_pixels};
use super::types::{CaptureCompare, MonitorGeom, RawFrame};
use crate::gpu_detile;

pub(crate) const HEAD_REFRESH_INTERVAL: Duration = Duration::from_millis(250);

pub fn should_refresh_heads(
    last: Option<Instant>,
    now: Instant,
    cached_empty: bool,
    force: bool,
) -> bool {
    force || cached_empty || last.is_none_or(|t| now.duration_since(t) >= HEAD_REFRESH_INTERVAL)
}

struct HeadFbState {
    connector: String,
    last_fb: Option<u32>,
}

pub struct DrmCapturer {
    cards: Vec<CardCtx>,
    head_fb: Vec<HeadFbState>,
    primary_plane_cache: HashMap<(usize, crtc::Handle), plane::Handle>,
    cached_heads: Vec<EnumeratedHead>,
    last_head_refresh: Option<Instant>,
}

#[derive(Clone)]
pub struct CapturedHead {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub data: Arc<[u8]>,
    pub force_full: bool,
    pub unchanged: bool,
    pub dirty_rects: Option<Vec<rdpcore_server::diff::Rect>>,
    pub connector: String,
}

impl DrmCapturer {
    pub fn open() -> io::Result<Self> {
        let (cards, heads) = open_drm_cards_and_heads()?;
        for h in &heads {
            let card = &cards[h.card_idx];
            tracing::info!(
                "kmsrdp: capturing DRM display {}:{} @{},{}",
                card.name,
                h.connector,
                h.x,
                h.y
            )
        }
        let head_fb = heads
            .iter()
            .map(|h| HeadFbState {
                connector: h.connector.clone(),
                last_fb: None,
            })
            .collect();
        Ok(Self {
            cards,
            head_fb,
            primary_plane_cache: HashMap::new(),
            cached_heads: heads,
            last_head_refresh: Some(Instant::now()),
        })
    }

    fn sync_head_fb(&mut self, heads: &[EnumeratedHead]) {
        self.head_fb
            .retain(|s| heads.iter().any(|h| h.connector == s.connector));
        for h in heads {
            if !self.head_fb.iter().any(|s| s.connector == h.connector) {
                self.head_fb.push(HeadFbState {
                    connector: h.connector.clone(),
                    last_fb: None,
                });
            }
        }
    }

    fn heads_for_tick(&mut self, force: bool) -> io::Result<Vec<EnumeratedHead>> {
        if !should_refresh_heads(
            self.last_head_refresh,
            Instant::now(),
            self.cached_heads.is_empty(),
            force,
        ) {
            return Ok(self.cached_heads.clone());
        }
        match refresh_heads(&self.cards) {
            Ok(heads) => {
                self.sync_head_fb(&heads);
                self.cached_heads = heads.clone();
                self.last_head_refresh = Some(Instant::now());
                Ok(heads)
            }
            Err(e) if !force && !self.cached_heads.is_empty() => {
                tracing::debug!("kmsrdp: head refresh failed, using cached heads: {e}");
                Ok(self.cached_heads.clone())
            }
            Err(e) => Err(e),
        }
    }

    fn primary_plane_for(
        &mut self,
        card_idx: usize,
        crtc: crtc::Handle,
    ) -> io::Result<plane::Info> {
        let card = &self.cards[card_idx].card;
        if let Some(&cached) = self.primary_plane_cache.get(&(card_idx, crtc))
            && let Ok(info) = card.get_plane(cached)
            && info.crtc() == Some(crtc)
        {
            return Ok(info);
        }

        let (handle, info) = card
            .plane_handles()?
            .into_iter()
            .find_map(|handle| {
                let info = card.get_plane(handle).ok()?;
                if info.crtc() != Some(crtc) {
                    return None;
                }
                let ty = plane_type(card, handle).ok()?;
                (ty == "Primary").then_some((handle, info))
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no primary plane for CRTC"))?;
        self.primary_plane_cache.insert((card_idx, crtc), handle);
        Ok(info)
    }

    pub fn capture(&mut self, prev: Option<CaptureCompare<'_>>) -> io::Result<RawFrame> {
        let heads = self.heads_for_tick(false)?;
        match self.capture_heads(&heads, prev) {
            Ok(frame) => Ok(frame),
            Err(first) => match self.heads_for_tick(true) {
                Ok(heads) => self.capture_heads(&heads, prev).map_err(|retry| {
                    io::Error::new(
                        first.kind(),
                        format!("{first}; retry after head refresh also failed: {retry}"),
                    )
                }),
                Err(refresh) => Err(io::Error::new(
                    first.kind(),
                    format!("{first}; head refresh after capture failure also failed: {refresh}"),
                )),
            },
        }
    }

    fn capture_heads(
        &mut self,
        heads: &[EnumeratedHead],
        prev: Option<CaptureCompare<'_>>,
    ) -> io::Result<RawFrame> {
        let single = heads.len() == 1;
        let mut captured = Vec::with_capacity(heads.len());
        for head in heads {
            let hint = if single { prev } else { None };
            captured.push(self.capture_head(head, hint)?);
        }

        if captured.len() == 1 {
            let Some(c) = captured.pop() else {
                return Err(io::Error::other("single-head capture produced no buffers"));
            };
            return Ok(RawFrame {
                width: c.width,
                height: c.height,
                stride: c.stride,
                data: c.data,
                force_full: c.force_full,
                unchanged: c.unchanged,
                dirty_rects: c.dirty_rects,
                monitors: vec![MonitorGeom {
                    left: 0,
                    top: 0,
                    right: c.width.saturating_sub(1) as i32,
                    bottom: c.height.saturating_sub(1) as i32,
                    primary: true,
                }],
            });
        }

        Ok(compose_heads(&captured, prev))
    }

    fn capture_head(
        &mut self,
        head: &EnumeratedHead,
        hint: Option<CaptureCompare<'_>>,
    ) -> io::Result<CapturedHead> {
        let plane_info = self.primary_plane_for(head.card_idx, head.crtc)?;
        let card_ctx = &self.cards[head.card_idx];

        let fb_handle = plane_info.framebuffer().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "primary plane has no framebuffer attached (screen off / locked?)",
            )
        })?;
        let fb_id = u32::from(fb_handle);
        let prev = self
            .head_fb
            .iter()
            .find(|s| s.connector == head.connector)
            .and_then(|s| s.last_fb);
        let force_full = prev.is_some_and(|p| p != fb_id);
        if force_full {
            tracing::warn!(
                "kmsrdp: primary-plane framebuffer changed on {}:{} ({prev:?} -> {fb_id}); \
                 forcing full-frame refresh for connected clients",
                card_ctx.name,
                head.connector
            );
        }
        if let Some(state) = self
            .head_fb
            .iter_mut()
            .find(|s| s.connector == head.connector)
        {
            state.last_fb = Some(fb_id);
        }

        let (size, fourcc, modifier, buffers, pitches, offsets) =
            match card_ctx.card.get_planar_framebuffer(fb_handle) {
                Ok(fb) => (
                    fb.size(),
                    fb.pixel_format(),
                    fb.modifier(),
                    fb.buffers(),
                    fb.pitches(),
                    fb.offsets(),
                ),
                Err(e) => {
                    tracing::warn!("GetFB2 failed ({e}), falling back to legacy GetFB");
                    let fb = card_ctx.card.get_framebuffer(fb_handle)?;
                    let mut buffers = [None; 4];
                    buffers[0] = fb.buffer();
                    let mut pitches = [0u32; 4];
                    pitches[0] = fb.pitch();
                    (
                        fb.size(),
                        DrmFourcc::Xrgb8888,
                        Some(DrmModifier::Linear),
                        buffers,
                        pitches,
                        [0u32; 4],
                    )
                }
            };

        let buf_handle = buffers[0].ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "framebuffer has no plane-0 buffer")
        })?;
        let fd = card_ctx.card.buffer_to_prime_fd(buf_handle, drm::CLOEXEC)?;
        let (width, height) = size;

        let is_plain_bgrx = matches!(fourcc, DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888)
            && matches!(modifier, None | Some(DrmModifier::Linear));
        let is_detileable_bgrx =
            matches!(fourcc, DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888) && modifier.is_some();

        let (stride, data, unchanged, dirty_rects) = if is_plain_bgrx {
            let pitch = pitches[0] as usize;
            let map_len = pitch * height as usize;
            let mmap = unsafe {
                MmapOptions::new()
                    .len(map_len)
                    .map(fd.as_raw_fd())
                    .map_err(|e| io::Error::other(format!("mmap failed: {e}")))?
            };
            dma_buf_sync_start(fd.as_raw_fd());
            let (data, unchanged, dirty_rects) =
                take_pixels(&mmap, pitch, width, height, force_full, hint);
            dma_buf_sync_end(fd.as_raw_fd());
            (pitch, data, unchanged, dirty_rects)
        } else if is_detileable_bgrx {
            let modifier = modifier.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tiled framebuffer missing DRM modifier",
                )
            })?;
            let detiled = gpu_detile::detile_to_bgrx(
                &card_ctx.path,
                fd.as_raw_fd(),
                fourcc,
                modifier,
                width,
                height,
                offsets[0],
                pitches[0],
            )?;
            let stride = width as usize * 4;
            let (data, unchanged, dirty_rects) =
                take_pixels(&detiled, stride, width, height, force_full, hint);
            (stride, data, unchanged, dirty_rects)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "format {fourcc:?} / modifier {modifier:?} isn't supported \
                     (need XRGB8888/ARGB8888)"
                ),
            ));
        };

        Ok(CapturedHead {
            x: head.x,
            y: head.y,
            width,
            height,
            stride,
            data,
            force_full,
            unchanged,
            dirty_rects,
            connector: head.connector.clone(),
        })
    }
}

/// Compose multiple head captures into one bounding-box canvas.
pub fn compose_heads(heads: &[CapturedHead], prev: Option<CaptureCompare<'_>>) -> RawFrame {
    let min_x = heads.iter().map(|h| h.x).min().unwrap_or(0);
    let min_y = heads.iter().map(|h| h.y).min().unwrap_or(0);
    let max_x = heads
        .iter()
        .map(|h| h.x + h.width as i32)
        .max()
        .unwrap_or(0);
    let max_y = heads
        .iter()
        .map(|h| h.y + h.height as i32)
        .max()
        .unwrap_or(0);
    let canvas_w = (max_x - min_x).max(1) as u32;
    let canvas_h = (max_y - min_y).max(1) as u32;
    let stride = canvas_w as usize * 4;
    let mut canvas = vec![0u8; stride * canvas_h as usize];
    let force_full = heads.iter().any(|h| h.force_full);

    // Primary: head closest to origin (then first).
    let primary_idx = heads
        .iter()
        .enumerate()
        .min_by_key(|(_, h)| (h.x * h.x + h.y * h.y, h.connector.as_str()))
        .map(|(i, _)| i)
        .unwrap_or(0);

    let mut monitors = Vec::with_capacity(heads.len());
    for (i, head) in heads.iter().enumerate() {
        let dx = head.x - min_x;
        let dy = head.y - min_y;
        blit_bgrx(
            &mut canvas,
            stride,
            canvas_w,
            canvas_h,
            &head.data,
            head.stride,
            head.width,
            head.height,
            dx,
            dy,
        );
        monitors.push(MonitorGeom {
            left: dx,
            top: dy,
            right: dx + head.width as i32 - 1,
            bottom: dy + head.height as i32 - 1,
            primary: i == primary_idx,
        });
    }

    let (data, unchanged, dirty_rects) =
        take_pixels(&canvas, stride, canvas_w, canvas_h, force_full, prev);
    RawFrame {
        width: canvas_w,
        height: canvas_h,
        stride,
        data,
        force_full,
        unchanged,
        dirty_rects,
        monitors,
    }
}
