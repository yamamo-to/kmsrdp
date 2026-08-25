//! Slow-Path (MCS Send Data) dispatch: SuppressOutput/RefreshRect on the I/O
//! channel, and virtual-channel PDU responses (RDPSND/CLIPRDR/DVC/RDPDR).

use std::sync::{Arc, Mutex};

use rdpcore_cliprdr::CliprdrChannel;
use rdpcore_dvc::DvcMux;
use rdpcore_pdu::finalization::{
    DataPdu, ShareDataPduType, decode_refresh_rect, decode_suppress_output,
};
use rdpcore_rdpdr::RdpdrChannel;
use rdpcore_rdpsnd::RdpsndChannel;
use rdpcore_transport::{ChannelKey, Frame, Priority};

use crate::encode::BitmapEncodePolicy;
use crate::error::SessionError;

use super::frame_pump::{send_all_or_timeout, send_outbound_bitmap};
use super::metrics::SessionBitmapMetrics;

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_slow_path_frame(
    bytes: &[u8],
    io_channel_id: u16,
    display_updates_allowed: &mut bool,
    updates: &mut dyn crate::display::RdpServerDisplayUpdates,
    input_handler: &Arc<Mutex<dyn crate::input::RdpServerInputHandler>>,
    rdpsnd: &Option<Arc<tokio::sync::Mutex<RdpsndChannel>>>,
    cliprdr: Option<&mut CliprdrChannel>,
    dvc: Option<&mut DvcMux>,
    rdpdr: Option<&mut RdpdrChannel>,
    frame_sender: &rdpcore_transport::FrameSender,
    policy: &BitmapEncodePolicy,
    frame_id: &mut u32,
    metrics: &mut SessionBitmapMetrics,
) -> Result<(), SessionError> {
    let payload = rdpcore_pdu::x224::unwrap_data(bytes)?;
    let send_data = rdpcore_pdu::mcs::SendData::decode_request(payload)?;

    if send_data.channel_id == io_channel_id {
        if let Ok(data_pdu) = DataPdu::decode(&send_data.data) {
            match data_pdu.pdu_type2 {
                ShareDataPduType::Input => {
                    if let Ok(events) =
                        rdpcore_pdu::finalization::decode_slowpath_input(&data_pdu.body)
                    {
                        let mut handler = input_handler.lock().unwrap_or_else(|e| e.into_inner());
                        for event in events {
                            super::input_handler::dispatch_input_event(&mut *handler, event);
                        }
                    }
                }
                ShareDataPduType::SuppressOutput => {
                    if let Ok(allow) = decode_suppress_output(&data_pdu.body) {
                        let was = *display_updates_allowed;
                        *display_updates_allowed = allow;
                        if allow
                            && !was
                            && let Some(full) = updates.latest_full_frame()
                        {
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
                        }
                    }
                }
                ShareDataPduType::RefreshRect => {
                    if let Ok(rects) = decode_refresh_rect(&data_pdu.body)
                        && let Some(full) = updates.latest_full_frame()
                    {
                        if rects.is_empty() {
                            let _ = send_outbound_bitmap(
                                &full,
                                frame_sender,
                                policy,
                                frame_id,
                                metrics,
                            )
                            .await;
                        } else {
                            for rect in rects {
                                let w = rect.right.saturating_sub(rect.left).saturating_add(1);
                                let h = rect.bottom.saturating_sub(rect.top).saturating_add(1);
                                let (Some(nw), Some(nh)) =
                                    (core::num::NonZeroU16::new(w), core::num::NonZeroU16::new(h))
                                else {
                                    continue;
                                };
                                if let Some(sub) = full.sub(rect.left, rect.top, nw, nh) {
                                    let _ = send_outbound_bitmap(
                                        &sub,
                                        frame_sender,
                                        policy,
                                        frame_id,
                                        metrics,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        return Ok(());
    }

    if let Some(channel) = rdpsnd {
        let mut channel = channel.lock().await;
        if send_data.channel_id == channel.channel_id() {
            let channel_id = channel.channel_id();
            let responses = channel.on_channel_data(&send_data.data)?;
            drop(channel);
            for response in responses {
                let _ = send_all_or_timeout(
                    frame_sender,
                    Frame {
                        channel: ChannelKey::Static(channel_id),
                        priority: Priority::Latency,
                        bytes: response,
                    },
                )
                .await;
            }
            return Ok(());
        }
    }
    if let Some(channel) = cliprdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = send_all_or_timeout(
                frame_sender,
                Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Bulk,
                    bytes: response,
                },
            )
            .await;
        }
        return Ok(());
    }
    if let Some(mux) = dvc
        && send_data.channel_id == mux.channel_id()
    {
        let channel_id = mux.channel_id();
        let responses = mux.on_channel_data(&send_data.data)?;
        for response in responses {
            let _ = send_all_or_timeout(
                frame_sender,
                Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes: response,
                },
            )
            .await;
        }
        return Ok(());
    }
    if let Some(channel) = rdpdr
        && send_data.channel_id == channel.channel_id()
    {
        let channel_id = channel.channel_id();
        for response in channel.on_channel_data(&send_data.data)? {
            let _ = send_all_or_timeout(
                frame_sender,
                Frame {
                    channel: ChannelKey::Static(channel_id),
                    priority: Priority::Latency,
                    bytes: response,
                },
            )
            .await;
        }
    }
    Ok(())
}
