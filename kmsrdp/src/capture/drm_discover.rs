use std::fs;
use std::io;

use drm::Device;
use drm::control::{Device as ControlDevice, connector, crtc, plane, property};

use super::display_mode::{DisplayMode, display_mode};
use super::dmabuf::Card;

pub fn plane_type(card: &Card, handle: plane::Handle) -> io::Result<String> {
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
pub fn connector_crtc_via_atomic_prop(
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

pub struct CardCtx {
    pub card: Card,
    pub path: String,
    pub name: String,
}

#[derive(Clone)]
pub struct EnumeratedHead {
    pub card_idx: usize,
    pub crtc: crtc::Handle,
    pub connector: String,
    /// CRTC position in the host virtual desktop.
    pub x: i32,
    pub y: i32,
}

/// Open DRM cards and collect active heads per [`display_mode`].
pub fn open_drm_cards_and_heads() -> io::Result<(Vec<CardCtx>, Vec<EnumeratedHead>)> {
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

pub fn collect_heads_on_card(
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
pub fn refresh_heads(cards: &[CardCtx]) -> io::Result<Vec<EnumeratedHead>> {
    let mode = display_mode()?;
    let mut heads = Vec::new();
    let mut discovered = Vec::new();
    for (card_idx, ctx) in cards.iter().enumerate() {
        if let Err(e) = collect_heads_on_card(
            &ctx.card,
            &ctx.name,
            card_idx,
            mode,
            &mut heads,
            &mut discovered,
        ) {
            discovered.push(format!("{}: {e}", ctx.name));
        }
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
