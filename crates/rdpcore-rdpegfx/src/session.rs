//! GFX session state machine + DVC handler.
//!
//! After CapsConfirm the server immediately sends ResetGraphics / CreateSurface
//! / MapSurfaceToOutput using the known capture size (recreated on resize).
//! H.264 on the wire is Annex B per MS-RDPEGFX `RFX_AVC420_BITMAP_STREAM`.

use std::sync::{Arc, Mutex, MutexGuard};

use rdpcore_dvc::DvcHandler;
use tracing::{debug, info, warn};

use crate::encoder::{EncodedAu, H264Encoder, MockH264Encoder};
use crate::pdu::{self, ClientMessage, MonitorDef, RawCapabilitySet, select_avc420_capability};

/// Recover from a poisoned `Mutex` so one panicked encoder/ack path cannot
/// take down every later GFX frame on this connection.
fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

const DEFAULT_SURFACE_ID: u16 = 1;
/// Soft cap — mstsc often delays FrameAcknowledge.
const MAX_FRAMES_IN_FLIGHT: u32 = 32;
const QUEUE_DEPTH_UNAVAILABLE: u32 = 0xffff_ffff;
/// Force an IDR at least this often so a lost/corrupt frame cannot leave
/// the client stuck on a black surface forever.
const IDR_INTERVAL_FRAMES: u64 = 30;
/// Consecutive empty/failed encodes before tearing down GFX and letting
/// the server paint Planar/NSCodec instead of leaving a black surface.
const ENCODE_FAIL_FALLBACK: u32 = 3;

/// Result of one GFX encode attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GfxFrameResult {
    /// Wire PDUs to send on the GFX channel.
    Frames(Vec<Vec<u8>>),
    /// Transient skip; stay on GFX (do not paint Planar over the surface).
    Skip,
    /// Abandon GFX for this connection. `teardown` is DeleteSurface (if any)
    /// so the client can drop the H.264 surface before Planar resumes.
    Fallback { teardown: Vec<Vec<u8>> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    WaitCaps,
    Ready,
    Failed,
}

struct Inner {
    state: State,
    /// False until ResetGraphics/CreateSurface/Map have been sent for `width`×`height`.
    surface_configured: bool,
    surface_id: u16,
    width: u16,
    height: u16,
    next_frame_id: u32,
    frames_in_flight: u32,
    frames_sent: u64,
    force_next_idr: bool,
    timestamp_ms: u32,
    encode_failures: u32,
}

impl Inner {
    fn new(width: u16, height: u16) -> Self {
        Self {
            state: State::WaitCaps,
            surface_configured: false,
            surface_id: DEFAULT_SURFACE_ID,
            width,
            height,
            next_frame_id: 1,
            frames_in_flight: 0,
            frames_sent: 0,
            force_next_idr: true,
            timestamp_ms: 0,
            encode_failures: 0,
        }
    }

    fn is_ready(&self) -> bool {
        self.state == State::Ready
    }

    fn on_caps(
        &mut self,
        sets: &[RawCapabilitySet],
        encoder: &Mutex<Box<dyn H264Encoder>>,
    ) -> Vec<Vec<u8>> {
        let Some(selected) = select_avc420_capability(sets) else {
            self.state = State::Failed;
            warn!("GFX CapsAdvertise has no AVC420; falling back to Planar/NSCodec");
            return Vec::new();
        };

        let mut out = Vec::new();

        // Mid-session CapsAdvertise is a real re-negotiate (mstsc does this).
        // Tear down the live surface cleanly — creating the same surfaceId
        // again without DeleteSurface, or ignoring Caps while still streaming
        // WireToSurface, both trigger client protocol errors.
        if self.state == State::Ready && self.surface_configured {
            let old = self.surface_id;
            out.push(pdu::encode_segmented_single(&pdu::encode_delete_surface(
                old,
            )));
            self.surface_id = self.surface_id.wrapping_add(1).max(1);
            info!(
                old_surface = old,
                new_surface = self.surface_id,
                "GFX Caps re-negotiate: deleted surface"
            );
        }

        self.state = State::Ready;
        self.force_next_idr = true;
        self.frames_in_flight = 0;
        self.frames_sent = 0;
        self.encode_failures = 0;
        recover_lock(encoder).reset();
        info!(
            version = format_args!("0x{:08x}", selected.version),
            width = self.width,
            height = self.height,
            "GFX CapsConfirm: AVC420 negotiated"
        );
        out.push(pdu::encode_segmented_single(&pdu::encode_caps_confirm(
            &selected,
        )));
        // Typical sequence: CapsConfirm then Reset/Create/Map before any frames.
        // Use the capture size known at session construction (updated on resize).
        if self.width > 0 && self.height > 0 {
            for pdu in self.setup_pdus() {
                out.push(pdu::encode_segmented_single(&pdu));
            }
            self.surface_configured = true;
            info!(
                width = self.width,
                height = self.height,
                surface_id = self.surface_id,
                "GFX surface configured after CapsConfirm"
            );
        } else {
            self.surface_configured = false;
        }
        out
    }

    fn setup_pdus(&self) -> Vec<Vec<u8>> {
        let monitors = [MonitorDef {
            left: 0,
            top: 0,
            right: i32::from(self.width).saturating_sub(1),
            bottom: i32::from(self.height).saturating_sub(1),
            primary: true,
        }];
        vec![
            pdu::encode_reset_graphics(u32::from(self.width), u32::from(self.height), &monitors),
            pdu::encode_create_surface(self.surface_id, self.width, self.height),
            pdu::encode_map_surface_to_output(self.surface_id, 0, 0),
        ]
    }

    fn on_frame_ack(&mut self, queue_depth: u32, frame_id: u32) {
        if queue_depth == QUEUE_DEPTH_UNAVAILABLE {
            self.frames_in_flight = 0;
        } else {
            self.frames_in_flight = queue_depth.min(MAX_FRAMES_IN_FLIGHT);
        }
        debug!(
            frame_id,
            queue_depth,
            in_flight = self.frames_in_flight,
            "GFX FrameAcknowledge"
        );
    }

    fn resize(
        &mut self,
        width: u16,
        height: u16,
        encoder: &Mutex<Box<dyn H264Encoder>>,
    ) -> Option<Vec<Vec<u8>>> {
        if width == 0 || height == 0 {
            return None;
        }
        if self.surface_configured && self.width == width && self.height == height {
            return None;
        }
        recover_lock(encoder).reset();
        let old_surface = self.surface_id;
        let had_surface = self.surface_configured;
        self.width = width;
        self.height = height;
        if had_surface {
            self.surface_id = self.surface_id.wrapping_add(1).max(1);
        }
        self.force_next_idr = true;
        self.frames_in_flight = 0;
        self.surface_configured = false;
        if self.state != State::Ready {
            return None;
        }
        let mut out = Vec::new();
        if had_surface {
            out.push(pdu::encode_segmented_single(&pdu::encode_delete_surface(
                old_surface,
            )));
        }
        for pdu in self.setup_pdus() {
            out.push(pdu::encode_segmented_single(&pdu));
        }
        self.surface_configured = true;
        info!(
            width,
            height,
            surface_id = self.surface_id,
            "GFX surface configured"
        );
        Some(out)
    }

    fn abandon(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.surface_configured {
            out.push(pdu::encode_segmented_single(&pdu::encode_delete_surface(
                self.surface_id,
            )));
            self.surface_configured = false;
        }
        self.state = State::Failed;
        out
    }

    fn record_encode_failure(&mut self) -> GfxFrameResult {
        self.encode_failures = self.encode_failures.saturating_add(1);
        if self.encode_failures >= ENCODE_FAIL_FALLBACK {
            warn!(
                failures = self.encode_failures,
                "GFX H.264 encode failed repeatedly; falling back to Planar/NSCodec"
            );
            return GfxFrameResult::Fallback {
                teardown: self.abandon(),
            };
        }
        GfxFrameResult::Skip
    }

    /// Session-state prep for one frame: readiness/resize/backpressure
    /// checks and the `force_idr` decision. Split out from the actual
    /// encode so the caller doesn't need to hold this session's lock for
    /// the CPU-bound `H264Encoder::encode_bgrx` call - see
    /// [`GfxSession::encode_frame`]'s doc comment for why that matters.
    /// Returns `Err(result)` for an early return (skip/fallback), or
    /// `Ok((prefix, force_idr))` when the caller should go on to encode.
    fn begin_encode(
        &mut self,
        width: u16,
        height: u16,
        encoder: &Mutex<Box<dyn H264Encoder>>,
    ) -> Result<(Vec<Vec<u8>>, bool), GfxFrameResult> {
        if self.state != State::Ready {
            return Err(GfxFrameResult::Skip);
        }
        if width == 0 || height == 0 {
            return Err(GfxFrameResult::Skip);
        }

        let mut prefix = Vec::new();
        if !self.surface_configured || self.width != width || self.height != height {
            match self.resize(width, height, encoder) {
                Some(pdus) => prefix = pdus,
                None if self.state != State::Ready => return Err(GfxFrameResult::Skip),
                None => {}
            }
        }
        if !self.surface_configured {
            return Err(GfxFrameResult::Skip);
        }

        if self.frames_in_flight >= MAX_FRAMES_IN_FLIGHT {
            self.frames_in_flight = MAX_FRAMES_IN_FLIGHT / 2;
            self.force_next_idr = true;
        }

        let force_idr = self.force_next_idr
            || self.frames_sent == 0
            || self.frames_sent.is_multiple_of(IDR_INTERVAL_FRAMES);
        Ok((prefix, force_idr))
    }

    /// Finalizes bookkeeping and builds the wire PDUs for a successful
    /// encode. Called after the encoder lock (held only for the encode
    /// itself) has already been released.
    fn finish_encode(
        &mut self,
        width: u16,
        height: u16,
        encoded: &EncodedAu,
        force_idr: bool,
    ) -> Vec<Vec<u8>> {
        self.encode_failures = 0;
        self.force_next_idr = false;

        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
        self.timestamp_ms = self.timestamp_ms.wrapping_add(33);
        self.frames_in_flight = self.frames_in_flight.saturating_add(1);
        self.frames_sent = self.frames_sent.saturating_add(1);

        // MS-RDPEGFX RFX_AVC420_BITMAP_STREAM requires Annex B on the wire.
        let bitmap =
            pdu::encode_avc420_bitmap_stream(width, height, encoded.qp, 100, &encoded.annex_b);

        let frames = vec![
            pdu::encode_segmented_single(&pdu::encode_start_frame(self.timestamp_ms, frame_id)),
            pdu::encode_segmented_single(&pdu::encode_wire_to_surface_1_avc420(
                self.surface_id,
                width,
                height,
                &bitmap,
            )),
            pdu::encode_segmented_single(&pdu::encode_end_frame(frame_id)),
        ];
        if self.frames_sent == 1 || force_idr || self.frames_sent.is_multiple_of(300) {
            debug!(
                frames_sent = self.frames_sent,
                frame_id,
                annex_b_len = encoded.annex_b.len(),
                force_idr,
                "GFX frame sent"
            );
        }
        frames
    }
}

/// Shared GFX session used both as a [`DvcHandler`] (inbound Caps/Ack) and
/// from the connection loop (outbound frames).
///
/// The encoder lives in its own `Mutex`, separate from `inner`'s session
/// state. `encode_frame` releases `inner`'s lock before running the
/// CPU-bound `H264Encoder::encode_bgrx` - otherwise a `FrameAcknowledge`
/// (arriving on the async connection task, not the blocking pool the
/// encode itself runs on) would block on `inner`'s `std::sync::Mutex` for
/// the full encode duration, stalling whichever tokio worker thread
/// handles it - and worker threads are shared across connections.
#[derive(Clone)]
pub struct GfxSession {
    inner: Arc<Mutex<Inner>>,
    encoder: Arc<Mutex<Box<dyn H264Encoder>>>,
}

impl core::fmt::Debug for GfxSession {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GfxSession").finish_non_exhaustive()
    }
}

impl GfxSession {
    pub fn new(encoder: Box<dyn H264Encoder>, width: u16, height: u16) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(width, height))),
            encoder: Arc::new(Mutex::new(encoder)),
        }
    }

    pub fn mock(width: u16, height: u16) -> Self {
        Self::new(Box::new(MockH264Encoder::default()), width, height)
    }

    pub fn is_ready(&self) -> bool {
        recover_lock(&self.inner).is_ready()
    }

    pub fn failed(&self) -> bool {
        recover_lock(&self.inner).state == State::Failed
    }

    pub fn encode_frame(
        &self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
    ) -> GfxFrameResult {
        let (prefix, force_idr) =
            match recover_lock(&self.inner).begin_encode(width, height, &self.encoder) {
                Ok(v) => v,
                Err(result) => return result,
            };

        // The encoder lock is held only for the encode itself - `inner`'s
        // lock is not held here, so FrameAcknowledge/CapsAdvertise handling
        // on the connection task isn't blocked behind this.
        let mut retried_idr = false;
        let encode_once =
            |idr: bool| recover_lock(&self.encoder).encode_bgrx(width, height, stride, pixels, idr);
        let encoded = match encode_once(force_idr) {
            Ok(au) if !au.annex_b.is_empty() => Ok(au),
            Ok(_) | Err(_) => {
                // Soft skip / transient RC failure: force an IDR and retry once
                // instead of falling through to Planar (which leaves the GFX
                // surface black while FrameAcks keep arriving).
                retried_idr = true;
                match encode_once(true) {
                    Ok(au) if !au.annex_b.is_empty() => Ok(au),
                    Ok(_) => Err(None),
                    Err(e) => Err(Some(e)),
                }
            }
        };

        let mut inner = recover_lock(&self.inner);
        let encoded = match encoded {
            Ok(au) => au,
            Err(e) => {
                match e {
                    Some(e) => warn!(error = %e, "GFX H.264 encode failed"),
                    None => debug!("GFX H.264 encode skipped (empty bitstream)"),
                }
                if retried_idr {
                    inner.force_next_idr = true;
                }
                return inner.record_encode_failure();
            }
        };
        let mut frames = prefix;
        frames.extend(inner.finish_encode(width, height, &encoded, force_idr));
        GfxFrameResult::Frames(frames)
    }

    pub fn resize(&self, width: u16, height: u16) -> Option<Vec<Vec<u8>>> {
        recover_lock(&self.inner).resize(width, height, &self.encoder)
    }

    pub fn dvc_handler(&self) -> GfxDvcHandler {
        GfxDvcHandler {
            inner: Arc::clone(&self.inner),
            encoder: Arc::clone(&self.encoder),
        }
    }
}

pub struct GfxDvcHandler {
    inner: Arc<Mutex<Inner>>,
    encoder: Arc<Mutex<Box<dyn H264Encoder>>>,
}

impl core::fmt::Debug for GfxDvcHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GfxDvcHandler").finish_non_exhaustive()
    }
}

impl DvcHandler for GfxDvcHandler {
    fn channel_name(&self) -> &str {
        pdu::CHANNEL_NAME
    }

    fn on_open(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn on_data(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut rest = data;
        let mut out = Vec::new();
        while !rest.is_empty() {
            let Ok(msg) = pdu::decode_client_message(rest) else {
                break;
            };
            if rest.len() < 8 {
                break;
            }
            let pdu_len = u32::from_le_bytes(rest[4..8].try_into().unwrap_or([0; 4])) as usize;
            if pdu_len < 8 || pdu_len > rest.len() {
                break;
            }
            rest = &rest[pdu_len..];

            let mut inner = recover_lock(&self.inner);
            match msg {
                ClientMessage::CapsAdvertise { sets } => {
                    out.extend(inner.on_caps(&sets, &self.encoder))
                }
                ClientMessage::FrameAcknowledge {
                    queue_depth,
                    frame_id,
                    ..
                } => {
                    inner.on_frame_ack(queue_depth, frame_id);
                }
                ClientMessage::CacheImportOffer | ClientMessage::Other { .. } => {}
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
