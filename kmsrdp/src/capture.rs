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

/// Find the first card with a connected connector that has an active CRTC,
/// same "first usable connector" search as upstream `get_usable_card_and_connector`.
fn find_usable_card_and_crtc() -> io::Result<(Card, String, crtc::Handle)> {
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

        let card = match Card::open_read_only(&path_str) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skip {path_str}: open failed: {e}");
                continue;
            }
        };

        let resources = match card.resource_handles() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skip {path_str}: get resources failed: {e}");
                continue;
            }
        };

        for &conn_handle in resources.connectors() {
            let Ok(conn) = card.get_connector(conn_handle, false) else {
                continue;
            };
            if conn.state() != connector::State::Connected {
                continue;
            }
            let legacy_crtc = conn
                .current_encoder()
                .and_then(|encoder_handle| card.get_encoder(encoder_handle).ok())
                .and_then(|encoder| encoder.crtc());
            let crtc_handle = match legacy_crtc {
                Some(crtc_handle) => crtc_handle,
                // Legacy encoder->crtc chain is empty (e.g. the proprietary
                // NVIDIA driver never fills it in) - fall back to the
                // connector's atomic CRTC_ID property.
                None => match connector_crtc_via_atomic_prop(&card, conn_handle) {
                    Ok(Some(crtc_handle)) => crtc_handle,
                    _ => continue,
                },
            };
            return Ok((card, path_str, crtc_handle));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no usable card/connector/CRTC found (is a display actually active?)",
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
}

/// Grabs the current primary-plane framebuffer of the first usable
/// card/connector/CRTC as raw BGRX8888 bytes (this is
/// `ironrdp_server::PixelFormat::BgrX32`'s exact memory layout, so the RDP
/// path can hand it to the encoder with no per-pixel conversion).
///
/// Requires `CAP_SYS_ADMIN` (see reframe-streamer's systemd unit for why).
///
/// Falls back to NvFBC ([`crate::nvfbc`]) when the DRM/KMS path fails to
/// find a bound CRTC at all, which happens on the proprietary NVIDIA
/// driver under a classic Xorg session (see README). NvFBC only ever runs
/// as this fallback since it's NVIDIA-only; DRM/KMS is the general path
/// for everything else.
pub fn capture_raw_bgrx() -> io::Result<RawFrame> {
    match capture_raw_bgrx_drm() {
        Ok(frame) => Ok(frame),
        Err(drm_err) => match crate::nvfbc::capture_bgrx() {
            Ok((width, height, data)) => Ok(RawFrame {
                width,
                height,
                stride: width as usize * 4,
                data,
            }),
            Err(nvfbc_err) => {
                eprintln!(
                    "DRM/KMS capture failed ({drm_err}), NvFBC fallback also failed ({nvfbc_err})"
                );
                Err(drm_err)
            }
        },
    }
}

fn capture_raw_bgrx_drm() -> io::Result<RawFrame> {
    let (card, card_path, crtc_handle) = find_usable_card_and_crtc()?;

    // Needed to see primary/cursor planes via plane_handles(), and to read
    // CRTC_X/Y-style properties later on.
    card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)?;
    card.set_client_capability(drm::ClientCapability::Atomic, true)?;

    // We may have become DRM master by being the first opener; drop it so a
    // compositor can still start normally, same as upstream's drmDropMaster().
    let _ = card.release_master_lock();

    let (plane_handle, plane_info) = card
        .plane_handles()?
        .into_iter()
        .find_map(|handle| {
            let info = card.get_plane(handle).ok()?;
            if info.crtc() != Some(crtc_handle) {
                return None;
            }
            let ty = plane_type(&card, handle).ok()?;
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

    // Prefer GetFB2 (fourcc + modifier + per-plane offsets/pitches), fall
    // back to legacy GetFB, exactly like export_fb2()/export_fb() upstream.
    let (size, fourcc, modifier, buffers, pitches, offsets) =
        match card.get_planar_framebuffer(fb_handle) {
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
                let fb = card.get_framebuffer(fb_handle)?;
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
    let fd = card.buffer_to_prime_fd(buf_handle, drm::CLOEXEC)?;
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
        });
    }

    if is_detileable_bgrx {
        let data = gpu_detile::detile_to_bgrx(
            &card_path,
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
        });
    }

    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "format {fourcc:?} / modifier {modifier:?} isn't supported (need XRGB8888/ARGB8888)"
        ),
    ))
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
