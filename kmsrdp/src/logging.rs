//! Process-wide structured logging via [`tracing`].
//!
//! Call [`init`] once at process start (before other subsystems log).
//!
//! Environment:
//! - `KMSRDP_LOG` — filter directive (same syntax as `RUST_LOG`), preferred
//! - `RUST_LOG` — used when `KMSRDP_LOG` is unset
//! - default filter when neither is set: `info`
//! - `KMSRDP_LOG_FORMAT=json` — JSON lines on stderr; anything else is text

use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

/// Install the global tracing subscriber. Safe to call only once; later
/// calls log a warning and leave the existing subscriber in place.
pub fn init() {
    let filter = match std::env::var("KMSRDP_LOG") {
        Ok(spec) if !spec.is_empty() => EnvFilter::try_new(spec).unwrap_or_else(|e| {
            eprintln!("kmsrdp: invalid KMSRDP_LOG ({e}); using info");
            EnvFilter::new("info")
        }),
        _ => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };

    let json = matches!(
        std::env::var("KMSRDP_LOG_FORMAT").as_deref(),
        Ok("json" | "JSON")
    );

    let result = if json {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .json(),
            )
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_ansi(stderr_is_tty()),
            )
            .try_init()
    };

    if let Err(e) = result {
        // Avoid panicking if a test or embedder already installed a subscriber.
        eprintln!("kmsrdp: tracing subscriber already set ({e})");
    }
}

fn stderr_is_tty() -> bool {
    unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
}
