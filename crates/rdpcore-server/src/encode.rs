//! Bitmap / NSCodec Fast-Path encoding and Mac-client compatibility policy.
//! Extracted from the accept/session loop so tiling, compression, and
//! resize-pending frame choice can be tested without a live socket.

use rdpcore_pdu::capability_sets::NsCodecNegotiated;
use rdpcore_pdu::fastpath::{self, UPDATE_CODE_BITMAP, UPDATE_CODE_SURFACE_COMMANDS};

use crate::display::BitmapUpdate;

pub(crate) fn client_needs_compat_workarounds(client_name: &str) -> bool {
    let n = client_name.to_ascii_lowercase();
    n.contains("mac") || n.contains("darwin") || n.contains("iphone") || n.contains("ipad")
}

/// `TS_BITMAP_DATA.bitmapLength` is a 16-bit field, so a single rectangle
/// can carry at most ~65535 bytes of raw pixel data (about 128x128 at
/// 32bpp) - a whole-frame update must be tiled into rectangles this small
/// or smaller before encoding, not just fragmented at the wire level
/// afterward (fragmentation splits already-encoded bytes; it can't fix a
/// `bitmapLength` field that overflowed before fragmentation even runs).
const TILE_SIZE: u16 = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct BitmapEncodePolicy {
    use_rdp6_planar: bool,
    max_rects_per_update: usize,
    pub(crate) nscodec: Option<(u8, u8)>,
    pub(crate) max_request_size: usize,
}

const COMPAT_MAX_RECTS_PER_UPDATE: usize = 32;

fn max_raw_strip_height(width: u16) -> u16 {
    let row_bytes = usize::from(width).saturating_mul(4);
    if row_bytes == 0 {
        return 1;
    }
    (65535usize / row_bytes).max(1) as u16
}

pub(crate) fn bitmap_encode_policy(
    client_name: &str,
    nscodec: Option<NsCodecNegotiated>,
    max_request_size: usize,
) -> BitmapEncodePolicy {
    let compat_mode = client_needs_compat_workarounds(client_name);
    // macOS Windows App: prefer NSCodec SurfaceCommands (IronRDP path). Raw
    // fast-path bitmaps work but are ~9MB/frame; RDP6 planar disconnects.
    let nscodec = if compat_mode {
        nscodec.map(|n| (n.codec_id, n.color_loss_level))
    } else {
        None
    };
    // Keep each reassembled Fast-Path Update under MaxRequestSize. A 64x64
    // compressed tile is typically a few KB; use a conservative per-rect budget.
    let size_limited_rects = (max_request_size / 8192).max(1);
    let max_rects_per_update = if compat_mode {
        COMPAT_MAX_RECTS_PER_UPDATE.min(size_limited_rects)
    } else {
        size_limited_rects
    };
    BitmapEncodePolicy {
        use_rdp6_planar: !compat_mode,
        max_rects_per_update,
        nscodec,
        max_request_size,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BitmapWireStats {
    tiles: u32,
    compressed_tiles: u32,
    raw_tiles: u32,
    encoded_bytes: usize,
    update_batches: u32,
}

/// Splits one `BitmapUpdate` into wire-ready `FastPathOutput` byte buffers,
/// batched for strict clients (macOS Windows App).
pub(crate) fn encode_bitmap_update(
    bitmap: &BitmapUpdate,
    policy: &BitmapEncodePolicy,
) -> (Vec<Vec<Vec<u8>>>, BitmapWireStats) {
    let width = bitmap.width.get();
    let height = bitmap.height.get();
    let row_bytes = usize::from(width) * 4;

    let mut rectangles = Vec::new();
    let mut stats = BitmapWireStats::default();

    if policy.use_rdp6_planar {
        let mut tile_y = 0u16;
        while tile_y < height {
            let tile_height = TILE_SIZE.min(height - tile_y);
            let mut tile_x = 0u16;
            while tile_x < width {
                let tile_width = TILE_SIZE.min(width - tile_x);
                push_bitmap_rect(
                    bitmap,
                    row_bytes,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                    policy,
                    &mut rectangles,
                    &mut stats,
                );
                tile_x += TILE_SIZE;
            }
            tile_y += TILE_SIZE;
        }
    } else {
        // Raw: tile into full-width strips so each rect carries as many scanlines
        // as the 16-bit `bitmapLength` field allows (IronRDP-style chunking).
        let strip_height = max_raw_strip_height(width);
        let mut tile_y = 0u16;
        while tile_y < height {
            let th = strip_height.min(height - tile_y);
            push_bitmap_rect(
                bitmap,
                row_bytes,
                0,
                tile_y,
                width,
                th,
                policy,
                &mut rectangles,
                &mut stats,
            );
            tile_y += th;
        }
    }

    let max_rects = policy.max_rects_per_update.min(rectangles.len().max(1));
    let mut batches = Vec::new();
    for chunk in rectangles.chunks(max_rects) {
        let wire = encode_rectangles_to_wire_frames(chunk, policy.max_request_size);
        stats.encoded_bytes += chunk.iter().map(|r| r.data.len()).sum::<usize>();
        stats.update_batches += 1;
        batches.push(wire);
    }
    (batches, stats)
}

#[allow(clippy::too_many_arguments)]
fn push_bitmap_rect(
    bitmap: &BitmapUpdate,
    row_bytes: usize,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
    policy: &BitmapEncodePolicy,
    rectangles: &mut Vec<fastpath::BitmapRect>,
    stats: &mut BitmapWireStats,
) {
    let tile_row_bytes = usize::from(tile_width) * 4;

    let mut tile_data = Vec::with_capacity(tile_row_bytes * usize::from(tile_height));
    for row in (0..tile_height).rev() {
        let src_row = usize::from(tile_y + row);
        let src_start = src_row * row_bytes + usize::from(tile_x) * 4;
        tile_data.extend_from_slice(&bitmap.data[src_start..src_start + tile_row_bytes]);
    }

    let planar_ok = policy.use_rdp6_planar && tile_width.is_multiple_of(4);
    let (data, compressed_scan_width) = if planar_ok {
        let compressed = rdpcore_pdu::rdp6::encode(
            &tile_data,
            usize::from(tile_width),
            usize::from(tile_height),
        );
        if compressed.len() < tile_data.len() {
            // Bytes, not pixels — see BitmapRect docs (MS-RDPBCGR vs mstsc).
            (compressed, Some(tile_width * 4))
        } else {
            (tile_data, None)
        }
    } else {
        (tile_data, None)
    };

    stats.tiles += 1;
    if compressed_scan_width.is_some() {
        stats.compressed_tiles += 1;
    } else {
        stats.raw_tiles += 1;
    }

    rectangles.push(fastpath::BitmapRect {
        dest_left: bitmap.x + tile_x,
        dest_top: bitmap.y + tile_y,
        dest_right: bitmap.x + tile_x + tile_width - 1,
        dest_bottom: bitmap.y + tile_y + tile_height - 1,
        width: tile_width,
        height: tile_height,
        bits_per_pixel: 32,
        data,
        compressed_scan_width,
    });
}

pub(crate) fn encode_nscodec_update(
    bitmap: &BitmapUpdate,
    codec_id: u8,
    color_loss_level: u8,
    max_request_size: usize,
) -> (Vec<Vec<Vec<u8>>>, BitmapWireStats) {
    let data = rdpcore_pdu::nscodec::encode(
        &bitmap.data,
        bitmap.width.get(),
        bitmap.height.get(),
        bitmap.stride.get(),
        color_loss_level,
    );
    let body = rdpcore_pdu::surface_commands::encode_set_surface_bits(
        bitmap.x,
        bitmap.y,
        bitmap.width.get(),
        bitmap.height.get(),
        codec_id,
        &data,
    );
    let wire = encode_update_to_wire_frames(UPDATE_CODE_SURFACE_COMMANDS, &body, max_request_size);
    let stats = BitmapWireStats {
        tiles: 1,
        compressed_tiles: 1,
        raw_tiles: 0,
        encoded_bytes: data.len(),
        update_batches: 1,
    };
    (vec![wire], stats)
}

pub(crate) fn encode_update_to_wire_frames(
    update_code: u8,
    body: &[u8],
    max_request_size: usize,
) -> Vec<Vec<u8>> {
    // Cap per-fragment payload so reassembly cannot exceed MaxRequestSize.
    let chunk = fastpath::MAX_FASTPATH_CHUNK_SIZE.min(max_request_size.max(1));
    let chunks: Vec<&[u8]> = body.chunks(chunk).collect();
    let count = chunks.len().max(1);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let fragmentation = if count == 1 {
                fastpath::Fragmentation::Single
            } else if i == 0 {
                fastpath::Fragmentation::First
            } else if i == count - 1 {
                fastpath::Fragmentation::Last
            } else {
                fastpath::Fragmentation::Next
            };
            fastpath::FastPathOutput {
                updates: vec![fastpath::FastPathUpdatePdu {
                    update_code,
                    fragmentation,
                    data: chunk.to_vec(),
                }],
            }
            .encode()
        })
        .collect()
}

fn encode_rectangles_to_wire_frames(
    rectangles: &[fastpath::BitmapRect],
    max_request_size: usize,
) -> Vec<Vec<u8>> {
    let bitmap_bytes = fastpath::BitmapUpdateData {
        rectangles: rectangles.to_vec(),
    }
    .encode();
    encode_update_to_wire_frames(UPDATE_CODE_BITMAP, &bitmap_bytes, max_request_size)
}

fn covers_desktop(bitmap: &BitmapUpdate, width: u16, height: u16) -> bool {
    bitmap.x == 0 && bitmap.y == 0 && bitmap.width.get() == width && bitmap.height.get() == height
}

/// Keep the best frame seen while a resize handshake is in flight.
/// Prefer a full-desktop bitmap (mstsc's canvas is blank after Deactivate-All);
/// only replace an existing full frame with a newer full frame.
pub(crate) fn retain_bitmap_during_resize(
    pending: &mut Option<BitmapUpdate>,
    bitmap: BitmapUpdate,
    desktop_width: u16,
    desktop_height: u16,
) {
    let incoming_full = covers_desktop(&bitmap, desktop_width, desktop_height);
    let have_full = pending
        .as_ref()
        .is_some_and(|p| covers_desktop(p, desktop_width, desktop_height));
    if incoming_full || !have_full {
        *pending = Some(bitmap);
    }
}

#[cfg(test)]
mod tests {
    use super::{covers_desktop, retain_bitmap_during_resize};
    use crate::display::{BitmapUpdate, PixelFormat};
    use core::num::{NonZeroU16, NonZeroUsize};

    use super::{bitmap_encode_policy, encode_bitmap_update, max_raw_strip_height};

    fn bitmap(x: u16, y: u16, width: u16, height: u16, fill: u8) -> BitmapUpdate {
        let w = NonZeroU16::new(width).unwrap();
        let h = NonZeroU16::new(height).unwrap();
        let stride = NonZeroUsize::new(usize::from(width) * 4).unwrap();
        BitmapUpdate {
            x,
            y,
            width: w,
            height: h,
            format: PixelFormat::BgrX32,
            data: std::sync::Arc::from(vec![fill; stride.get() * usize::from(height)]),
            stride,
        }
    }

    #[test]
    fn covers_desktop_requires_origin_and_exact_size() {
        assert!(covers_desktop(&bitmap(0, 0, 100, 50, 1), 100, 50));
        assert!(!covers_desktop(&bitmap(1, 0, 100, 50, 1), 100, 50));
        assert!(!covers_desktop(&bitmap(0, 0, 64, 50, 1), 100, 50));
    }

    #[test]
    fn resize_pending_prefers_full_frame_over_later_tile() {
        let mut pending = None;
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 100, 50, 1), 100, 50);
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 64, 64, 2), 100, 50);
        let kept = pending.unwrap();
        assert!(covers_desktop(&kept, 100, 50));
        assert_eq!(kept.data[0], 1);
    }

    #[test]
    fn resize_pending_upgrades_tile_to_full_frame() {
        let mut pending = None;
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 64, 64, 2), 100, 50);
        retain_bitmap_during_resize(&mut pending, bitmap(0, 0, 100, 50, 3), 100, 50);
        let kept = pending.unwrap();
        assert!(covers_desktop(&kept, 100, 50));
        assert_eq!(kept.data[0], 3);
    }

    #[test]
    fn raw_strip_height_fits_bitmap_length_field() {
        assert_eq!(max_raw_strip_height(1920), 8);
        assert!(1920usize * 4 * usize::from(max_raw_strip_height(1920)) <= 65535usize);
    }

    #[test]
    fn mac_compat_full_frame_uses_few_strip_tiles() {
        let policy = bitmap_encode_policy("m1-mac-mini", None, 8 * 1024 * 1024);
        assert!(!policy.use_rdp6_planar);
        assert_eq!(policy.max_rects_per_update, 32);

        let frame = bitmap(0, 0, 1920, 1200, 0);
        let (_wire, stats) = encode_bitmap_update(&frame, &policy);
        assert_eq!(stats.tiles, 150); // 1200 / 8 scanline strips
        assert_eq!(stats.update_batches, 5); // ceil(150 / 32)
        assert_eq!(stats.raw_tiles, 150);
    }
}
