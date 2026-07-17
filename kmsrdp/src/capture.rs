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

impl DisplaySelector {
    fn parse(value: &str) -> Result<Option<Self>, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }

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

        Ok(Some(Self { card, connector }))
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

static DISPLAY_SELECTOR: OnceLock<Result<Option<DisplaySelector>, String>> = OnceLock::new();

fn display_selector() -> io::Result<Option<&'static DisplaySelector>> {
    let configured = DISPLAY_SELECTOR.get_or_init(|| {
        DisplaySelector::parse(&std::env::var("KMSRDP_DISPLAY").unwrap_or_else(|_| String::new()))
    });
    match configured {
        Ok(selector) => Ok(selector.as_ref()),
        Err(reason) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid KMSRDP_DISPLAY: {reason}"),
        )),
    }
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

struct OpenedCard {
    card: Card,
    path: String,
    name: String,
    crtc: crtc::Handle,
    connector: String,
}

/// Find the configured connected connector with an active CRTC. When
/// `KMSRDP_DISPLAY` is unset, this preserves the original "first usable
/// connector" behavior.
fn open_usable_card() -> io::Result<OpenedCard> {
    let selector = display_selector()?;
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
                eprintln!("skip {path_str}: open failed: {e}");
                continue;
            }
        };

        match find_crtc_on_card(&card, card_name) {
            Ok((crtc, connector)) => {
                return Ok(OpenedCard {
                    card,
                    path: path_str,
                    name: card_name.to_owned(),
                    crtc,
                    connector,
                });
            }
            Err(e) => {
                discovered.push(format!("{card_name}: {e}"));
            }
        }
    }

    let reason = match selector {
        Some(selector) => format!(
            "requested display {} is not an active DRM connector",
            selector.configured_name()
        ),
        None => "no usable card/connector/CRTC found (is a display actually active?)".to_string(),
    };
    let discovered = if discovered.is_empty() {
        "none".to_string()
    } else {
        discovered.join(", ")
    };
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{reason}; discovered DRM connectors: {discovered}"),
    ))
}

/// Resolve the currently active CRTC on an already-open card without
/// reopening the DRM device. Keeping the same fd is important: repeatedly
/// opening the card while Xorg is dropping DRM master can prevent fbcon
/// from restoring the text console after logout.
fn find_crtc_on_card(card: &Card, card_name: &str) -> io::Result<(crtc::Handle, String)> {
    let selector = display_selector()?;
    let resources = card.resource_handles()?;
    let mut discovered = Vec::new();

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
        discovered.push(format!("{qualified_name} (active)"));
        if selector.is_some_and(|wanted| !wanted.matches(card_name, &connector_name)) {
            continue;
        }
        return Ok((crtc_handle, connector_name));
    }

    let reason = match selector {
        Some(selector) => format!(
            "requested display {} is not active on {card_name}",
            selector.configured_name()
        ),
        None => format!("no active connector found on {card_name}"),
    };
    let discovered = if discovered.is_empty() {
        "none".to_string()
    } else {
        discovered.join(", ")
    };
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{reason}; discovered DRM connectors: {discovered}"),
    ))
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
}

/// Stateful screen capturer. The DRM card fd stays open for this object's
/// lifetime so the capture loop never repeatedly becomes DRM master while
/// Xorg is exiting and fbcon is trying to restore the text console.
pub struct Capturer {
    drm: Option<DrmCapturer>,
    drm_open_error: Option<String>,
}

impl Capturer {
    pub fn new() -> io::Result<Self> {
        match DrmCapturer::open() {
            Ok(drm) => Ok(Self {
                drm: Some(drm),
                drm_open_error: None,
            }),
            Err(drm_err) if display_selector()?.is_some() => {
                // NvFBC captures the X screen as a whole and cannot honor a
                // connector selection. Falling back here would silently show
                // a different display than the administrator requested.
                Err(drm_err)
            }
            Err(drm_err) => Ok(Self {
                drm: None,
                drm_open_error: Some(drm_err.to_string()),
            }),
        }
    }

    pub fn capture(&mut self) -> io::Result<RawFrame> {
        let drm_error = match &mut self.drm {
            Some(drm) => match drm.capture() {
                Ok(frame) => return Ok(frame),
                Err(drm_err) if display_selector()?.is_some() => return Err(drm_err),
                Err(drm_err) => drm_err.to_string(),
            },
            None => self
                .drm_open_error
                .clone()
                .unwrap_or_else(|| "DRM/KMS capturer unavailable".to_string()),
        };

        match crate::nvfbc::capture_bgrx() {
            Ok((width, height, data)) => Ok(RawFrame {
                width,
                height,
                stride: width as usize * 4,
                data,
                force_full: false,
            }),
            Err(nvfbc_err) => Err(io::Error::other(format!(
                "DRM/KMS capture failed ({drm_error}), \
                 NvFBC fallback also failed ({nvfbc_err})"
            ))),
        }
    }
}

struct DrmCapturer {
    card: Card,
    card_path: String,
    card_name: String,
    crtc: crtc::Handle,
    connector: String,
    /// Last primary-plane framebuffer handle we successfully captured.
    /// `None` until the first frame; a change forces a full-frame refresh.
    last_fb: Option<u32>,
}

impl DrmCapturer {
    fn open() -> io::Result<Self> {
        let opened = open_usable_card()?;

        // Needed to see primary/cursor planes via plane_handles(), and to read
        // CRTC_X/Y-style properties later on.
        opened
            .card
            .set_client_capability(drm::ClientCapability::UniversalPlanes, true)?;
        opened
            .card
            .set_client_capability(drm::ClientCapability::Atomic, true)?;

        // We may have become DRM master by being the first opener. Drop it
        // once, then retain this non-master fd for all subsequent captures.
        let _ = opened.card.release_master_lock();
        eprintln!(
            "kmsrdp: capturing DRM display {}:{}",
            opened.name, opened.connector
        );

        Ok(Self {
            card: opened.card,
            card_path: opened.path,
            card_name: opened.name,
            crtc: opened.crtc,
            connector: opened.connector,
            last_fb: None,
        })
    }

    fn capture(&mut self) -> io::Result<RawFrame> {
        let (crtc, connector) = find_crtc_on_card(&self.card, &self.card_name)?;
        self.crtc = crtc;
        if connector != self.connector {
            eprintln!(
                "kmsrdp: capturing DRM display {}:{connector}",
                self.card_name
            );
            self.connector = connector;
        }

        let (plane_handle, plane_info) = self
            .card
            .plane_handles()?
            .into_iter()
            .find_map(|handle| {
                let info = self.card.get_plane(handle).ok()?;
                if info.crtc() != Some(self.crtc) {
                    return None;
                }
                let ty = plane_type(&self.card, handle).ok()?;
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
        let force_full = self.last_fb.is_some_and(|prev| prev != fb_id);
        if force_full {
            eprintln!(
                "kmsrdp: primary-plane framebuffer changed ({:?} -> {fb_id}); \
                 forcing full-frame refresh for connected clients",
                self.last_fb
            );
        }
        self.last_fb = Some(fb_id);

        // Prefer GetFB2 (fourcc + modifier + per-plane offsets/pitches), fall
        // back to legacy GetFB, exactly like export_fb2()/export_fb() upstream.
        let (size, fourcc, modifier, buffers, pitches, offsets) =
            match self.card.get_planar_framebuffer(fb_handle) {
                Ok(fb) => (
                    fb.size(),
                    fb.pixel_format(),
                    fb.modifier(),
                    fb.buffers(),
                    fb.pitches(),
                    fb.offsets(),
                ),
                Err(e) => {
                    eprintln!("GetFB2 failed ({e}), falling back to legacy GetFB");
                    let fb = self.card.get_framebuffer(fb_handle)?;
                    let mut buffers = [None; 4];
                    buffers[0] = fb.buffer();
                    let mut pitches = [0u32; 4];
                    pitches[0] = fb.pitch();
                    (
                        fb.size(),
                        // Legacy GetFB has no fourcc; DRM only ever used XRGB8888 here.
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
        let fd = self.card.buffer_to_prime_fd(buf_handle, drm::CLOEXEC)?;
        let (width, height) = size;

        // Plain Linear XRGB8888/ARGB8888 can be read back with a CPU mmap;
        // tiled (vendor-modifier) framebuffers of the same formats go through a
        // GBM/EGL detile pass instead. Anything else (e.g. multi-plane YUV)
        // isn't supported by either path.
        let is_plain_bgrx = matches!(fourcc, DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888)
            && matches!(modifier, None | Some(DrmModifier::Linear));
        let is_detileable_bgrx =
            matches!(fourcc, DrmFourcc::Xrgb8888 | DrmFourcc::Argb8888) && modifier.is_some();

        if is_plain_bgrx {
            let pitch = pitches[0] as usize;
            let map_len = pitch * height as usize;

            // Safety: `fd` is a dma-buf we just exported ourselves via PRIME,
            // backing a buffer at least `pitch * height` bytes; nothing else in
            // this process writes to it concurrently.
            let mmap = unsafe {
                MmapOptions::new()
                    .len(map_len)
                    .map(fd.as_raw_fd())
                    .map_err(|e| io::Error::other(format!("mmap failed: {e}")))?
            };

            return Ok(RawFrame {
                width,
                height,
                stride: pitch,
                data: mmap.to_vec(),
                force_full,
            });
        }

        if is_detileable_bgrx {
            let data = gpu_detile::detile_to_bgrx(
                &self.card_path,
                fd.as_raw_fd(),
                fourcc,
                modifier.expect("checked by is_detileable_bgrx"),
                width,
                height,
                offsets[0],
                pitches[0],
            )?;
            return Ok(RawFrame {
                width,
                height,
                stride: width as usize * 4,
                data,
                force_full,
            });
        }

        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "format {fourcc:?} / modifier {modifier:?} isn't supported \
                 (need XRGB8888/ARGB8888)"
            ),
        ))
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
    use super::DisplaySelector;

    #[test]
    fn empty_display_selector_uses_default() {
        assert_eq!(DisplaySelector::parse("").unwrap(), None);
        assert_eq!(DisplaySelector::parse("  ").unwrap(), None);
    }

    #[test]
    fn connector_only_selector_matches_any_card() {
        let selector = DisplaySelector::parse(" DP-1 ").unwrap().unwrap();
        assert!(selector.matches("card0", "DP-1"));
        assert!(selector.matches("card1", "DP-1"));
        assert!(!selector.matches("card0", "HDMI-A-1"));
    }

    #[test]
    fn qualified_selector_matches_one_card_and_connector() {
        let selector = DisplaySelector::parse("card1:DP-1").unwrap().unwrap();
        assert!(selector.matches("card1", "DP-1"));
        assert!(!selector.matches("card0", "DP-1"));
        assert!(!selector.matches("card1", "DP-2"));
    }

    #[test]
    fn malformed_qualified_selector_is_rejected() {
        assert!(DisplaySelector::parse(":DP-1").is_err());
        assert!(DisplaySelector::parse("card0:").is_err());
        assert!(DisplaySelector::parse("card0:DP-1:extra").is_err());
    }
}
