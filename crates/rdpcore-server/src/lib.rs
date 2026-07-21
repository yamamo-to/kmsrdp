//! Async RDP server: TLS glue, the accept loop, and the trait surface
//! (`RdpServerDisplay`/`RdpServerInputHandler`/`CredentialValidator`)
//! backends plug into. Wraps `rdpcore-connector`'s sans-io state machine
//! with real sockets, drives steady-state fast-path input/output once the
//! connection sequence completes, and wires in the optional `rdpsnd`
//! (audio) and `cliprdr` (clipboard) static channels when a client
//! negotiates them and a backend factory is configured. All outgoing bytes
//! - bitmap fragments, audio, clipboard data - flow through
//!
//! `rdpcore-transport`'s priority scheduler rather than being written
//! directly, so a bulk channel (graphics) can't starve a latency-sensitive
//! one (audio).

mod credentials;
mod credssp;
pub mod diff;
mod display;
mod input;
mod server;
mod transport;

pub use credentials::{CredentialValidator, Credentials, ExactMatchCredentialValidator};
pub use display::{
    BitmapUpdate, DesktopSize, DisplayUpdate, MonitorLayoutEntry, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
pub use input::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
pub use server::{RdpServer, RdpServerBuilder};

/// Re-exported so callers building a `TlsAcceptor` (e.g. kmsrdp's
/// `tls.rs`) depend on exactly the version this crate was built against,
/// mirroring `ironrdp-server`'s own `pub use tokio_rustls` convention.
pub use tokio_rustls;
