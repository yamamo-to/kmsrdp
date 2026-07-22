//! DRM/KMS screen capture without any compositor cooperation.
//!
//! Mirrors what `reframe-streamer/main.c` (upstream ReFrame) does: open the
//! card read-only, find the primary plane of an active CRTC, and export its
//! current framebuffer via `drmPrimeHandleToFD`. A Linear XRGB8888/ARGB8888
//! framebuffer is read back with a plain CPU mmap; a tiled (vendor-modifier)
//! one of the same formats goes through [`crate::gpu_detile`] instead.

use std::fs;
use std::io;
use std::os::unix::io::{AsFd, AsRawFd};
use std::sync::OnceLock;

use drm::Device;
use drm::control::{Device as ControlDevice, connector, crtc, plane, property};
use drm_fourcc::{DrmFourcc, DrmModifier};
use memmap2::MmapOptions;

use crate::gpu_detile;

#[derive(Debug)]
struct Card(fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> std::os::unix::io::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

impl Card {
    fn open_read_only(path: &str) -> io::Result<Self> {
        // Matches reframe-streamer: O_RDONLY is enough to query resources and
        // export framebuffers as PRIME fds; we never need to draw anything.
        let file = fs::OpenOptions::new().read(true).open(path)?;
        Ok(Card(file))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplaySelector {
    card: Option<String>,
    connector: String,
}

/// How `KMSRDP_DISPLAY` selects capture sources.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayMode {
    /// Unset or `all`: every connected CRTC, composited into one canvas.
    All,
    /// Named connector (`DP-1` or `card1:DP-1`): that head only.
    Single(DisplaySelector),
}

impl DisplaySelector {
    fn parse_connector(value: &str) -> Result<Self, String> {
        let (card, connector) = match value.split_once(':') {
            Some((card, connector)) => {
                let card = card.trim();
                let connector = connector.trim();
                if card.is_empty() || connector.is_empty() || connector.contains(':') {
                    return Err("expected CONNECTOR (for example DP-1) or CARD:CONNECTOR \
                         (for example card1:DP-1)"
                        .to_string());
                }
                (Some(card.to_string()), connector.to_string())
            }
            None => (None, value.to_string()),
        };
        Ok(Self { card, connector })
    }

    fn matches(&self, card: &str, connector: &str) -> bool {
        self.connector == connector && self.card.as_deref().is_none_or(|wanted| wanted == card)
    }

    fn configured_name(&self) -> String {
        match &self.card {
            Some(card) => format!("{card}:{}", self.connector),
            None => self.connector.clone(),
        }
    }
}

impl DisplayMode {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        Ok(Self::Single(DisplaySelector::parse_connector(value)?))
    }

    fn is_single(&self) -> bool {
        matches!(self, Self::Single(_))
    }
}

static DISPLAY_MODE: OnceLock<Result<DisplayMode, String>> = OnceLock::new();

fn display_mode() -> io::Result<&'static DisplayMode> {
    let configured = DISPLAY_MODE.get_or_init(|| {
        DisplayMode::parse(&std::env::var("KMSRDP_DISPLAY").unwrap_or_else(|_| String::new()))
    });
    match configured {
        Ok(mode) => Ok(mode),
        Err(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid KMSRDP_DISPLAY: {reason}"),
        )),
    }
}

/// Parse `KMSRDP_DISPLAY` early so startup checks can fail before opening DRM.
pub fn validate_display_env() -> io::Result<()> {
    display_mode().map(|_| ())
}

fn plane_type(card: &Card, handle: plane::Handle) -> io::Result<String> {
    let props = card.get_properties(handle)?;
    for (prop_handle, value) in &props {
        let info = card.get_property(*prop_handle)?;
        if info.name().to_str().unwrap_or("") != "type" {
            continue;
        }
        if let property::Value::Enum(Some(entry)) = info.value_type().convert_value(*value) {
            return Ok(entry.name().to_str().unwrap_or("?").to_string());
        }
    }
    Ok("unknown".to_string())
}

/// Read a connector's atomic `CRTC_ID` property directly.
///
/// The proprietary NVIDIA driver doesn't populate the legacy
/// encoder->crtc_id chain (`current_encoder()`/`Encoder::crtc()` stay
/// `None`) even while actively driving the connector, so
/// `find_usable_card_and_crtc`'s legacy walk always comes up empty on it;
/// the atomic `CRTC_ID` property is the one thing that driver does fill in.
fn connector_crtc_via_atomic_prop(
    card: &Card,
    conn_handle: connector::Handle,
) -> io::Result<Option<crtc::Handle>> {
    let props = card.get_properties(conn_handle)?;
    for (prop_handle, value) in &props {
        let info = card.get_property(*prop_handle)?;
        if info.name().to_str().unwrap_or("") != "CRTC_ID" {
            continue;
        }
        return Ok(drm::control::from_u32(*value as u32));
    }
    Ok(None)
}

struct CardCtx {
    card: Card,
    path: String,
    name: String,
}

struct EnumeratedHead {
    card_idx: usize,
    crtc: crtc::Handle,
    connector: String,
    /// CRTC position in the host virtual desktop.
    x: i32,
    y: i32,
}

/// Open DRM cards and collect active heads per [`display_mode`].
fn open_drm_cards_and_heads() -> io::Result<(Vec<CardCtx>, Vec<EnumeratedHead>)> {
    let mode = display_mode()?;
    let mut cards = Vec::new();
    let mut heads = Vec::new();
    let mut discovered = Vec::new();

    let mut entries: Vec<_> = fs::read_dir("/dev/dri")?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") {
            continue;
        }
        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();
        let card_name = name.as_ref();

        let card = match Card::open_read_only(&path_str) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("skip {path_str}: open failed: {e}");
                continue;
            }
        };

        let _ = card.set_client_capability(drm::ClientCapability::UniversalPlanes, true);
        let _ = card.set_client_capability(drm::ClientCapability::Atomic, true);
        let _ = card.release_master_lock();

        let card_idx = cards.len();
        let before = heads.len();
        match collect_heads_on_card(
            &card,
            card_name,
            card_idx,
            mode,
            &mut heads,
            &mut discovered,
        ) {
            Ok(()) => {
                if heads.len() > before {
                    cards.push(CardCtx {
                        card,
                        path: path_str,
                        name: card_name.to_owned(),
                    });
                }
            }
            Err(e) => {
                discovered.push(format!("{card_name}: {e}"));
            }
        }

        if matches!(mode, DisplayMode::Single(_)) && !heads.is_empty() {
            break;
        }
    }

    if heads.is_empty() {
        let reason = match mode {
            DisplayMode::All => {
                "no usable card/connector/CRTC found (is a display actually active?)".to_string()
            }
            DisplayMode::Single(sel) => format!(
                "requested display {} is not an active DRM connector",
                sel.configured_name()
            ),
        };
        let discovered = if discovered.is_empty() {
            "none".to_string()
        } else {
            discovered.join(", ")
        };
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{reason}; discovered DRM connectors: {discovered}"),
        ));
    }

    Ok((cards, heads))
}

fn collect_heads_on_card(
    card: &Card,
    card_name: &str,
    card_idx: usize,
    mode: &DisplayMode,
    heads: &mut Vec<EnumeratedHead>,
    discovered: &mut Vec<String>,
) -> io::Result<()> {
    let resources = card.resource_handles()?;
    for &conn_handle in resources.connectors() {
        let Ok(conn) = card.get_connector(conn_handle, false) else {
            continue;
        };
        let connector_name = conn.to_string();
        let qualified_name = format!("{card_name}:{connector_name}");
        if conn.state() != connector::State::Connected {
            discovered.push(format!("{qualified_name} (disconnected)"));
            continue;
        }
        let legacy_crtc = conn
            .current_encoder()
            .and_then(|encoder_handle| card.get_encoder(encoder_handle).ok())
            .and_then(|encoder| encoder.crtc());
        let crtc_handle = match legacy_crtc {
            Some(crtc_handle) => crtc_handle,
            None => match connector_crtc_via_atomic_prop(card, conn_handle) {
                Ok(Some(crtc_handle)) => crtc_handle,
                _ => {
                    discovered.push(format!("{qualified_name} (connected, inactive)"));
                    continue;
                }
            },
        };

        if let DisplayMode::Single(wanted) = mode
            && !wanted.matches(card_name, &connector_name)
        {
            discovered.push(format!("{qualified_name} (active, skipped)"));
            continue;
        }

        let info = card.get_crtc(crtc_handle)?;
        let (px, py) = info.position();
        discovered.push(format!("{qualified_name} (active @{px},{py})"));
        heads.push(EnumeratedHead {
            card_idx,
            crtc: crtc_handle,
            connector: connector_name,
            x: px as i32,
            y: py as i32,
        });

        if matches!(mode, DisplayMode::Single(_)) {
            break;
        }
    }
    Ok(())
}

/// Refresh head list on already-open cards (same fds).
fn refresh_heads(cards: &[CardCtx]) -> io::Result<Vec<EnumeratedHead>> {
    let mode = display_mode()?;
    let mut heads = Vec::new();
    let mut discovered = Vec::new();
    for (card_idx, ctx) in cards.iter().enumerate() {
        collect_heads_on_card(
            &ctx.card,
            &ctx.name,
            card_idx,
            mode,
            &mut heads,
            &mut discovered,
        )?;
        if matches!(mode, DisplayMode::Single(_)) && !heads.is_empty() {
            break;
        }
    }
    if heads.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no active connector on open cards; discovered: {}",
                if discovered.is_empty() {
                    "none".to_string()
                } else {
                    discovered.join(", ")
                }
            ),
        ));
    }
    Ok(heads)
}

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
    pub data: Vec<u8>,
    /// True when the DRM primary plane swapped to a different framebuffer
    /// object (e.g. Xorg exited and fbcon restored the console FB). Existing
    /// RDP clients only receive dirty-rect updates, so a scene change that
    /// happens without a large pixel-diff in one tick would leave them
    /// showing the previous tiles; the display hub treats this as a
    /// mandatory full-frame refresh.
    pub force_full: bool,
    /// Monitor layout relative to this frame's origin (always ≥1 entry).
    pub monitors: Vec<MonitorGeom>,
}

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
            Err(drm_err) if display_mode()?.is_single() => {
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
        let drm_error = match &mut self.drm {
            Some(drm) => match drm.capture() {
                Ok(frame) => {
                    self.note_backend("DRM/KMS");
                    return Ok(frame);
                }
                Err(drm_err) if display_mode()?.is_single() => {
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
            Ok((width, height, data)) => {
                self.note_backend("NvFBC");
                Ok(RawFrame {
                    width,
                    height,
                    stride: width as usize * 4,
                    data,
                    force_full: false,
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
enum CapturePhase {
    Open,
    Frame,
}

/// Attach short, actionable hints so journal/console logs explain a black
/// screen instead of a bare I/O error.
fn annotate_capture_error(err: io::Error, phase: CapturePhase) -> io::Error {
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

struct HeadFbState {
    connector: String,
    last_fb: Option<u32>,
}

struct DrmCapturer {
    cards: Vec<CardCtx>,
    /// Per-head last FB id, keyed by connector name (stable across refresh).
    head_fb: Vec<HeadFbState>,
}

struct CapturedHead {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    stride: usize,
    data: Vec<u8>,
    force_full: bool,
    connector: String,
}

impl DrmCapturer {
    fn open() -> io::Result<Self> {
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
        Ok(Self { cards, head_fb })
    }

    fn capture(&mut self) -> io::Result<RawFrame> {
        let heads = refresh_heads(&self.cards)?;
        // Drop FB state for connectors that disappeared; keep known ones.
        self.head_fb
            .retain(|s| heads.iter().any(|h| h.connector == s.connector));
        for h in &heads {
            if !self.head_fb.iter().any(|s| s.connector == h.connector) {
                self.head_fb.push(HeadFbState {
                    connector: h.connector.clone(),
                    last_fb: None,
                });
            }
        }

        let mut captured = Vec::with_capacity(heads.len());
        for head in &heads {
            let piece = self.capture_head(head)?;
            captured.push(piece);
        }

        if captured.len() == 1 {
            let c = captured.pop().unwrap();
            return Ok(RawFrame {
                width: c.width,
                height: c.height,
                stride: c.stride,
                data: c.data,
                force_full: c.force_full,
                monitors: vec![MonitorGeom {
                    left: 0,
                    top: 0,
                    right: c.width.saturating_sub(1) as i32,
                    bottom: c.height.saturating_sub(1) as i32,
                    primary: true,
                }],
            });
        }

        Ok(compose_heads(&captured))
    }

    fn capture_head(&mut self, head: &EnumeratedHead) -> io::Result<CapturedHead> {
        let card_ctx = &self.cards[head.card_idx];
        let (plane_handle, plane_info) = card_ctx
            .card
            .plane_handles()?
            .into_iter()
            .find_map(|handle| {
                let info = card_ctx.card.get_plane(handle).ok()?;
                if info.crtc() != Some(head.crtc) {
                    return None;
                }
                let ty = plane_type(&card_ctx.card, handle).ok()?;
                (ty == "Primary").then_some((handle, info))
            })
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no primary plane for CRTC"))?;
        let _ = plane_handle;

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

        let (stride, data) = if is_plain_bgrx {
            let pitch = pitches[0] as usize;
            let map_len = pitch * height as usize;
            let mmap = unsafe {
                MmapOptions::new()
                    .len(map_len)
                    .map(fd.as_raw_fd())
                    .map_err(|e| io::Error::other(format!("mmap failed: {e}")))?
            };
            (pitch, mmap.to_vec())
        } else if is_detileable_bgrx {
            let data = gpu_detile::detile_to_bgrx(
                &card_ctx.path,
                fd.as_raw_fd(),
                fourcc,
                modifier.expect("checked by is_detileable_bgrx"),
                width,
                height,
                offsets[0],
                pitches[0],
            )?;
            (width as usize * 4, data)
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
            connector: head.connector.clone(),
        })
    }
}

/// Compose multiple head captures into one bounding-box canvas.
fn compose_heads(heads: &[CapturedHead]) -> RawFrame {
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
    let mut data = vec![0u8; stride * canvas_h as usize];
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
            &mut data,
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

    RawFrame {
        width: canvas_w,
        height: canvas_h,
        stride,
        data,
        force_full,
        monitors,
    }
}

/// Copy a tightly-or-padded BGRX source rectangle into `dst` at (`dst_x`,`dst_y`).
#[allow(clippy::too_many_arguments)]
fn blit_bgrx(
    dst: &mut [u8],
    dst_stride: usize,
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_stride: usize,
    src_w: u32,
    src_h: u32,
    dst_x: i32,
    dst_y: i32,
) {
    let src_w = src_w as i32;
    let src_h = src_h as i32;
    let dst_w = dst_w as i32;
    let dst_h = dst_h as i32;
    for row in 0..src_h {
        let dy = dst_y + row;
        if dy < 0 || dy >= dst_h {
            continue;
        }
        let mut src_col0 = 0i32;
        let mut dst_col0 = dst_x;
        let mut copy_w = src_w;
        if dst_col0 < 0 {
            src_col0 = -dst_col0;
            copy_w += dst_col0;
            dst_col0 = 0;
        }
        if dst_col0 + copy_w > dst_w {
            copy_w = dst_w - dst_col0;
        }
        if copy_w <= 0 || src_col0 >= src_w {
            continue;
        }
        let bytes = copy_w as usize * 4;
        let s = row as usize * src_stride + src_col0 as usize * 4;
        let d = dy as usize * dst_stride + dst_col0 as usize * 4;
        if s + bytes <= src.len() && d + bytes <= dst.len() {
            dst[d..d + bytes].copy_from_slice(&src[s..s + bytes]);
        }
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
            data: vec![1, 0, 0, 0, 2, 0, 0, 0],
            force_full: false,
            connector: "A".into(),
        };
        let right = CapturedHead {
            x: 2,
            y: 0,
            width: 2,
            height: 1,
            stride: 8,
            data: vec![3, 0, 0, 0, 4, 0, 0, 0],
            force_full: true,
            connector: "B".into(),
        };
        let frame = compose_heads(&[left, right]);
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
}
