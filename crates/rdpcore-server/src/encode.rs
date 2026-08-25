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
    // Capability-first: if the client negotiated NSCodec, use it. RDP6
    // planar is the default for everyone else. Name matching is only a
    // last-resort fallback — some Mac clients disconnect on planar even
    // when they omit NSCodec from Confirm Active.
    let nscodec_params = nscodec.map(|n| (n.codec_id, n.color_loss_level));
    let name_compat = client_needs_compat_workarounds(client_name);
    let use_rdp6_planar = nscodec_params.is_none() && !name_compat;
    let compat_limits = nscodec_params.is_some() || name_compat;
    // Keep each reassembled Fast-Path Update under MaxRequestSize. A 64x64
    // compressed tile is typically a few KB; use a conservative per-rect budget.
    let size_limited_rects = (max_request_size / 8192).max(1);
    let max_rects_per_update = if compat_limits {
        COMPAT_MAX_RECTS_PER_UPDATE.min(size_limited_rects)
    } else {
        size_limited_rects
    };
    BitmapEncodePolicy {
        use_rdp6_planar,
        max_rects_per_update,
        nscodec: nscodec_params,
        max_request_size,
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct BitmapWireStats {
    pub(crate) tiles: u32,
    pub(crate) compressed_tiles: u32,
    pub(crate) raw_tiles: u32,
    pub(crate) encoded_bytes: usize,
    pub(crate) update_batches: u32,
}

/// Reusable scratch buffers for [`encode_bitmap_update`], avoiding
/// per-frame heap allocations on the hot path.
#[derive(Default)]
pub(crate) struct EncodeScratch {
    pub(crate) rectangles: Vec<fastpath::BitmapRect>,
    pub(crate) tile_scratch: Vec<u8>,
    pub(crate) batches: Vec<Vec<Vec<u8>>>,
    /// Freelist of previous frames' `BitmapRect::data` buffers. Each tile
    /// needs its own owned buffer (many tiles are alive at once in
    /// `rectangles` before being wired out), so `tile_scratch` alone can't
    /// be reused for this - recycling last frame's now-unused buffers
    /// avoids a fresh heap allocation per tile instead.
    pub(crate) buffer_pool: Vec<Vec<u8>>,
}

/// Returns an owned copy of `src`, preferring a recycled buffer from `pool`
/// (cleared and refilled, reusing its capacity) over a fresh allocation.
fn pooled_copy(pool: &mut Vec<Vec<u8>>, src: &[u8]) -> Vec<u8> {
    let mut buf = pool.pop().unwrap_or_default();
    buf.clear();
    buf.extend_from_slice(src);
    buf
}

/// Splits one `BitmapUpdate` into wire-ready `FastPathOutput` byte buffers,
/// batched for strict clients (macOS Windows App).
///
/// The caller-provided `scratch` is cleared and reused across frames to
/// avoid per-frame heap allocations (see AGENTS.md Rule 2.6).
pub(crate) fn encode_bitmap_update(
    bitmap: &BitmapUpdate,
    policy: &BitmapEncodePolicy,
    scratch: &mut EncodeScratch,
) -> BitmapWireStats {
    let width = bitmap.width.get();
    let height = bitmap.height.get();

    // Recycle last frame's per-tile buffers before dropping the rects that
    // own them, instead of letting `clear()` deallocate each one.
    scratch
        .buffer_pool
        .extend(scratch.rectangles.drain(..).map(|r| r.data));
    scratch.batches.clear();
    let mut stats = BitmapWireStats::default();
    scratch.tile_scratch.clear();

    if policy.use_rdp6_planar {
        let mut tile_y = 0u16;
        while tile_y < height {
            let tile_height = TILE_SIZE.min(height - tile_y);
            let mut tile_x = 0u16;
            while tile_x < width {
                let tile_width = TILE_SIZE.min(width - tile_x);
                push_bitmap_rect(
                    bitmap,
                    tile_x,
                    tile_y,
                    tile_width,
                    tile_height,
                    policy,
                    &mut scratch.tile_scratch,
                    &mut scratch.buffer_pool,
                    &mut scratch.rectangles,
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
                0,
                tile_y,
                width,
                th,
                policy,
                &mut scratch.tile_scratch,
                &mut scratch.buffer_pool,
                &mut scratch.rectangles,
                &mut stats,
            );
            tile_y += th;
        }
    }

    let max_rects = policy
        .max_rects_per_update
        .min(scratch.rectangles.len().max(1));
    for chunk in scratch.rectangles.chunks(max_rects) {
        let wire = encode_rectangles_to_wire_frames(chunk, policy.max_request_size);
        stats.encoded_bytes += chunk.iter().map(|r| r.data.len()).sum::<usize>();
        stats.update_batches += 1;
        scratch.batches.push(wire);
    }
    stats
}

#[allow(clippy::too_many_arguments)]
fn push_bitmap_rect(
    bitmap: &BitmapUpdate,
    tile_x: u16,
    tile_y: u16,
    tile_width: u16,
    tile_height: u16,
    policy: &BitmapEncodePolicy,
    tile_scratch: &mut Vec<u8>,
    buffer_pool: &mut Vec<Vec<u8>>,
    rectangles: &mut Vec<fastpath::BitmapRect>,
    stats: &mut BitmapWireStats,
) {
    let tile_row_bytes = usize::from(tile_width) * 4;
    let needed_len = tile_row_bytes * usize::from(tile_height);
    tile_scratch.clear();
    tile_scratch.reserve(needed_len);

    for row in (0..tile_height).rev() {
        let src_start = bitmap.src_byte_offset(tile_x, tile_y + row);
        if let Some(slice) = bitmap.data.get(src_start..src_start + tile_row_bytes) {
            tile_scratch.extend_from_slice(slice);
        } else {
            tile_scratch.extend(std::iter::repeat_n(0u8, tile_row_bytes));
        }
    }

    let planar_ok = policy.use_rdp6_planar && tile_width.is_multiple_of(4);
    let (data, compressed_scan_width) = if planar_ok {
        let mut compressed = buffer_pool.pop().unwrap_or_default();
        rdpcore_pdu::rdp6::encode_to(
            tile_scratch,
            usize::from(tile_width),
            usize::from(tile_height),
            &mut compressed,
        );
        if compressed.len() < tile_scratch.len() {
            // Bytes, not pixels — see BitmapRect docs (MS-RDPBCGR vs mstsc).
            (compressed, Some(tile_width * 4))
        } else {
            buffer_pool.push(compressed);
            (pooled_copy(buffer_pool, tile_scratch), None)
        }
    } else {
        (pooled_copy(buffer_pool, tile_scratch), None)
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
    let start = bitmap.src_byte_offset(0, 0);
    let pixels = bitmap.data.get(start..).unwrap_or(&[]);
    let data = rdpcore_pdu::nscodec::encode(
        pixels,
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
    let bitmap_bytes = fastpath::BitmapUpdateData::encode_rectangles(rectangles);
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

    use super::{EncodeScratch, bitmap_encode_policy, encode_bitmap_update, max_raw_strip_height};

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
            src_x: 0,
            src_y: 0,
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
    fn mac_name_without_nscodec_falls_back_to_raw_strips() {
        let policy = bitmap_encode_policy("m1-mac-mini", None, 8 * 1024 * 1024);
        assert!(!policy.use_rdp6_planar);
        assert_eq!(policy.max_rects_per_update, 32);

        let frame = bitmap(0, 0, 1920, 1200, 0);
        let mut scratch = EncodeScratch::default();
        let stats = encode_bitmap_update(&frame, &policy, &mut scratch);
        assert!(!scratch.batches.is_empty());
        assert_eq!(stats.tiles, 150); // 1200 / 8 scanline strips
        assert_eq!(stats.update_batches, 5); // ceil(150 / 32)
        assert_eq!(stats.raw_tiles, 150);
    }

    #[test]
    fn buffer_pool_recycles_the_same_allocation_across_frames() {
        // Raw path (m1-mac-mini forces it): every tile goes through
        // pooled_copy, never the compressed-planar branch.
        let policy = bitmap_encode_policy("m1-mac-mini", None, 8 * 1024 * 1024);
        let mut scratch = EncodeScratch::default();

        // 4x4 fits in a single raw strip, so each frame produces exactly
        // one BitmapRect - simplest case to check allocation identity.
        let frame1 = bitmap(0, 0, 4, 4, 0xAA);
        encode_bitmap_update(&frame1, &policy, &mut scratch);
        assert_eq!(scratch.rectangles.len(), 1);
        let first_ptr = scratch.rectangles[0].data.as_ptr();

        let frame2 = bitmap(0, 0, 4, 4, 0xBB);
        encode_bitmap_update(&frame2, &policy, &mut scratch);
        assert_eq!(scratch.rectangles.len(), 1);
        assert_eq!(
            scratch.rectangles[0].data.as_ptr(),
            first_ptr,
            "second frame's tile buffer should reuse the first frame's allocation"
        );
        assert!(
            scratch.rectangles[0].data.iter().all(|&b| b == 0xBB),
            "reused buffer must not leak stale bytes from the previous frame"
        );
    }

    #[test]
    fn buffer_pool_recycles_compressed_allocation_across_frames() {
        // Planar compressed path (MSTSC uses it for 64x64 tiles with compressible data).
        let policy = bitmap_encode_policy("MSTSC", None, 8 * 1024 * 1024);
        assert!(policy.use_rdp6_planar);
        let mut scratch = EncodeScratch::default();

        // 64x64 single-color tile compresses down to ~10-20 bytes (much smaller than 64*64*4 raw).
        let frame1 = bitmap(0, 0, 64, 64, 0x11);
        let stats1 = encode_bitmap_update(&frame1, &policy, &mut scratch);
        assert_eq!(stats1.compressed_tiles, 1);
        assert_eq!(scratch.rectangles.len(), 1);
        let first_ptr = scratch.rectangles[0].data.as_ptr();

        let frame2 = bitmap(0, 0, 64, 64, 0x22);
        let stats2 = encode_bitmap_update(&frame2, &policy, &mut scratch);
        assert_eq!(stats2.compressed_tiles, 1);
        assert_eq!(scratch.rectangles.len(), 1);
        assert_eq!(
            scratch.rectangles[0].data.as_ptr(),
            first_ptr,
            "second frame's compressed tile buffer should reuse the first frame's allocation"
        );
    }

    #[test]
    fn mstsc_without_nscodec_uses_rdp6_planar() {
        let policy = bitmap_encode_policy("MSTSC", None, 8 * 1024 * 1024);
        assert!(policy.use_rdp6_planar);
        assert_eq!(policy.nscodec, None);
    }

    #[test]
    fn stride_view_encodes_the_same_wire_as_a_tight_pack() {
        let width = 128u16;
        let height = 64u16;
        let stride = usize::from(width) * 4;
        let mut canvas = vec![0u8; stride * usize::from(height)];
        for y in 0..usize::from(height) {
            for x in 64..128 {
                let i = y * stride + x * 4;
                canvas[i] = 10;
                canvas[i + 1] = 20;
                canvas[i + 2] = 30;
                canvas[i + 3] = 0xFF;
            }
        }
        let full = BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            format: PixelFormat::BgrX32,
            data: std::sync::Arc::from(canvas),
            stride: NonZeroUsize::new(stride).unwrap(),
            src_x: 0,
            src_y: 0,
        };
        let view = full
            .sub(
                64,
                0,
                NonZeroU16::new(64).unwrap(),
                NonZeroU16::new(height).unwrap(),
            )
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&view.data, &full.data));

        let mut packed = vec![0u8; 64 * usize::from(height) * 4];
        let packed_stride = 64 * 4;
        for y in 0..usize::from(height) {
            let src = y * stride + 64 * 4;
            packed[y * packed_stride..(y + 1) * packed_stride]
                .copy_from_slice(&full.data[src..src + packed_stride]);
        }
        let packed_bmp = BitmapUpdate {
            x: 64,
            y: 0,
            width: NonZeroU16::new(64).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            format: PixelFormat::BgrX32,
            data: std::sync::Arc::from(packed),
            stride: NonZeroUsize::new(packed_stride).unwrap(),
            src_x: 0,
            src_y: 0,
        };

        let policy = bitmap_encode_policy("MSTSC", None, 8 * 1024 * 1024);
        let mut scratch_view = EncodeScratch::default();
        let view_stats = encode_bitmap_update(&view, &policy, &mut scratch_view);
        let mut scratch_packed = EncodeScratch::default();
        let packed_stats = encode_bitmap_update(&packed_bmp, &policy, &mut scratch_packed);
        assert_eq!(scratch_view.batches, scratch_packed.batches);
        assert_eq!(view_stats.tiles, packed_stats.tiles);
        assert_eq!(view_stats.compressed_tiles, packed_stats.compressed_tiles);
    }

    #[test]
    fn negotiated_nscodec_wins_over_windows_client_name() {
        let nscodec = rdpcore_pdu::capability_sets::NsCodecNegotiated {
            codec_id: 1,
            color_loss_level: 3,
        };
        let policy = bitmap_encode_policy("MSTSC", Some(nscodec), 8 * 1024 * 1024);
        assert!(!policy.use_rdp6_planar);
        assert_eq!(policy.nscodec, Some((1, 3)));
        assert_eq!(policy.max_rects_per_update, 32);
    }
}
