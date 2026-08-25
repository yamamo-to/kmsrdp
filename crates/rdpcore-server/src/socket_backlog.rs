//! How far behind the wire actually is, as seen by the kernel - not by our
//! own write calls.
//!
//! `Frame`s handed to `rdpcore_transport::FrameSender` are considered "sent"
//! once the writer task's `write_all` returns, but that only means the
//! kernel's TCP send buffer *accepted* the bytes - it autotunes up to several
//! MB and will happily buffer a large backlog silently while the peer (or a
//! lossy link) can't drain it, long before any `write()` call would block.
//! For NSCodec/Planar bitmap updates there is no application-level
//! acknowledgement from the client to fall back on (unlike GFX's
//! `FrameAcknowledge`), so the kernel's own outstanding-byte count is the
//! only signal available for "is the client actually caught up" - see its
//! use alongside `bulk_send` in `session_loop.rs`.

use std::os::fd::RawFd;

/// Bytes still sitting in the socket's send buffer, unacknowledged by the
/// peer (the same value `ss`'s `Send-Q` column reports). `None` if the
/// ioctl fails (e.g. the fd is already closed) - callers should treat that
/// as "unknown, assume not backed up" rather than erroring the connection
/// over a diagnostic that failed.
pub(crate) fn outstanding_send_bytes(fd: RawFd) -> Option<u32> {
    let mut bytes: libc::c_int = 0;
    // SAFETY: `fd` is a live socket for the duration of this call (callers
    // hold the connection open), `TIOCOUTQ` is a read-only ioctl, and
    // `bytes` is a valid `c_int` the kernel writes exactly one of into.
    let ret = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &mut bytes) };
    if ret == 0 {
        Some(bytes.max(0) as u32)
    } else {
        None
    }
}
