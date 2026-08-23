use std::io;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySelector {
    pub card: Option<String>,
    pub connector: String,
}

/// How `KMSRDP_DISPLAY` selects capture sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayMode {
    /// Unset or `all`: every connected CRTC, composited into one canvas.
    All,
    /// Named connector (`DP-1` or `card1:DP-1`): that head only.
    Single(DisplaySelector),
}

impl DisplaySelector {
    pub fn parse_connector(value: &str) -> Result<Self, String> {
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

    pub fn matches(&self, card: &str, connector: &str) -> bool {
        self.connector == connector && self.card.as_deref().is_none_or(|wanted| wanted == card)
    }

    pub fn configured_name(&self) -> String {
        match &self.card {
            Some(card) => format!("{card}:{}", self.connector),
            None => self.connector.clone(),
        }
    }
}

impl DisplayMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        Ok(Self::Single(DisplaySelector::parse_connector(value)?))
    }

    pub fn is_single(&self) -> bool {
        matches!(self, Self::Single(_))
    }
}

static DISPLAY_MODE: OnceLock<Result<DisplayMode, String>> = OnceLock::new();

pub fn display_mode() -> io::Result<&'static DisplayMode> {
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
