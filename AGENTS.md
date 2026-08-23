# AI Agent Instructions for KMSRDP

This document provides essential architectural context, engineering guidelines, safety rules, and development workflows for AI agents working on the **KMSRDP** codebase.

---

## 1. Project Overview & Architecture

KMSRDP is a pure-Rust, high-performance RDP remote desktop server for Linux that captures active kernel DRM/KMS scanouts (without compositor cooperation) and injects input via `uinput`.

### Workspace Structure

* **`crates/rdpcore-pdu`**: From-scratch RDP protocol codecs (MS-RDPBCGR, MS-RDP6, BER/PER, GCC, MCS, X.224, Licensing, Fast-Path, Pointer, SurfaceCommands, NSCodec).
* **`crates/rdpcore-transport`**: Packet priority scheduler (`Priority::Latency` vs `Priority::Bulk`) and non-blocking framed connection writer.
* **`crates/rdpcore-connector`**: Pre-authentication transport negotiation (NLA / TLS / Direct RDP).
* **`crates/rdpcore-server`**: Steady-state RDP session loop, CredSSP/NTLMv2, authentication rate limiting, dirty-tile diffing (`diff.rs`), and video/bitmap encoding dispatch (`encode.rs`).
* **`crates/rdpcore-rdpegfx`**: MS-RDPEGFX AVC420 H.264 dynamic virtual channel (OpenH264, optional VAAPI & NVENC hardware encoders).
* **`crates/rdpcore-cliprdr`**: MS-RDPECLIP clipboard virtual channel.
* **`crates/rdpcore-rdpdr`**: MS-RDPEFS / MS-RDPDR device redirection channel (FUSE drive redirection).
* **`crates/rdpcore-rdpsnd`**: MS-RDPSND audio output virtual channel (Wave2 PCM).
* **`crates/rdpcore-rdpeai`**: MS-RDPEAI audio input (microphone) dynamic virtual channel.
* **`crates/rdpcore-dvc`**: Dynamic Virtual Channel manager (MS-RDPEDYC).
* **`kmsrdp`**: Binary entrypoint and Linux system integration (DRM/KMS capture, GBM/EGL detiling, NvFBC fallback, PulseAudio/PipeWire bridges, FUSE mount lifecycle, uinput input handler, X11 Unicode typing, and systemd-logind session watcher).

---

## 2. Core Safety & Engineering Guidelines

### 2.1 PDU Parsing & Encoding Invariants (`crates/rdpcore-*`)
* **Zero Panics in Production Paths**: Never use `.unwrap()` or `.expect()` on network input or protocol parsers. Return `Result<T, DecodeError>` or appropriate error types.
* **Allocation DoS Protection**: When parsing count-prefixed structures (arrays, capability sets, lists), **always** validate against the remaining buffer using `ReadCursor::ensure_count::<T>(count)?` or explicit bounds checks before allocating vectors.
* **Safe Slicing**: Avoid direct slice indexing `&buf[start..end]`. Use `.get(start..end)` or ensure length invariants are strictly validated prior to slicing.
* **ASN.1 BER INTEGER Rules**: Values $\ge 0x8000\_0000$ have the high bit set. In 2's complement ASN.1 BER, encode with a leading `0x00` pad byte (5 bytes total) to denote a positive integer.
* **Fast-Path Input**: When `events.len() == 0`, MS-RDPBCGR requires the 1-byte `numberEvents` field (`0x00`) to be written explicitly following the header length.

### 2.2 Concurrency & Async Rules
* **No Blocking Calls on Tokio Worker Threads**:
  * Offload heavy computations (Planar RLE, NSCodec YCoCg compression, video encoding) to `tokio::task::spawn_blocking`.
  * Offload synchronous IPC / system calls (X11 `arboard` calls, PulseAudio `ensure_virtual_mic_sink` mainloop wait, FUSE unmounts) to `spawn_blocking` or dedicated OS worker threads.
* **No Multithreaded `std::env::set_var` / `remove_var`**: Modifying process environment variables in a multi-threaded application causes data races with C libraries (`libc::getenv`). Pass `Session` context and connection configs explicitly.
* **Poison-Tolerant Mutex Recovery**: When locking `std::sync::Mutex`, use `.unwrap_or_else(|e| e.into_inner())` or clear recovery patterns to prevent cascade failure if another thread panicked.

### 2.3 KMS / DRM & GPU Capture (`kmsrdp/src/capture.rs`, `gpu_detile.rs`)
* **DMA-BUF CPU Synchronization**: When reading PRIME DMA-BUF memory mappings on the CPU, always bracket reads with `DMA_BUF_IOCTL_SYNC` (`DMA_BUF_SYNC_START` before read, and `DMA_BUF_SYNC_END` after read) to ensure cache coherency.
* **EGL / DRM Cleanup Order**: In GPU detiler drop implementations, always delete OpenGL objects (textures, framebuffers, programs) before terminating EGL contexts and GBM devices.

### 2.4 FUSE / RDPDR Drive Redirection (`kmsrdp/src/rdpdr_fuse/`)
* **Multi-threaded Race Tolerance**: FUSE operations run on multi-threaded worker pools (`n_threads = 4`). Never assume an inode metadata lookup immediately following an insert will succeed; handle `None` gracefully and return `Errno::EIO`.
* **Path Sanitization**: All client-supplied paths and `DosName`s must pass `rdpdr_path::is_safe_win_component` and `sanitize_dos_name` to prevent directory traversal attacks.

### 2.5 Input Handling & Release Lifecycle (`kmsrdp/src/uinput.rs`)
* **Stuck Key Prevention**: Input state is connection-scoped (`ConnectionScopedInput`). When a connection drops or panics, `ResetInputOnDrop` releases any keys/buttons held by that connection.
* **Shared Refcounting**: Shared uinput device hold states must be refcounted across multiple sessions (`inject_held`) so releasing one client's key does not interrupt another client holding the same key.

---

## 3. Development & Verification Workflows

### Building & Testing
Before proposing or committing any code changes, ensure all of the following commands succeed without warnings or errors:

```bash
# 1. Run all unit and integration tests across all workspace crates
cargo test --workspace

# 2. Verify strict Clippy linter checks
cargo clippy --workspace --all-targets

# 3. Verify release build
cargo build --release --bin rdp_server
```

### Version Bump & Release Procedure (Lockstep Requirement)
When bumping the version (e.g. `0.1.48` $\rightarrow$ `0.1.49`), all 4 files **MUST** be updated in lockstep in a dedicated commit:

1. **`kmsrdp/Cargo.toml`**: `version = "x.y.z"`
2. **`Cargo.lock`**: Synchronized via `cargo check`
3. **`debian/changelog`**: Add entry for `kmsrdp (x.y.z) unstable; urgency=medium` with timestamp
4. **`kmsrdp.spec`**: Update `Version: x.y.z` and append an entry to `%changelog`

Commit message convention: `Release vx.y.z: keep Cargo.toml, spec, and debian/changelog in lockstep.`
