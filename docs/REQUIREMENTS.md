# kmsrdp Requirements Specification (USDM Format)

## 0. Document Positioning

- **Purpose**: Comprehensively document the implemented features of kmsrdp (Linux DRM/KMS-based RDP server) as requirement specifications to serve as standards for quality management (reviews, test coverage verification, and regression checking).
- **Format**: USDM (Universal Specification Describing Manner). Each requirement is described in a 3-layer structure: **Requirement → Reason → Specification (Spec)**. Specifications with multiple conditional branches are listed as states.
- **ID System**: `REQ-<Domain>-<Seq>` (Requirement) / `SPC-<Domain>-<Seq>-<SubSeq>` (Specification). Domain abbreviations are indicated in chapter headings.
- **Source**: `README.md`, `docs/ARCHITECTURE.md`, `docs/SECURITY.md`, `docs/QUALITY.md`, and source code under `crates/*` and `kmsrdp/src/*` (as of `main`). The "Implementation" column indicates traceability roots in code files.
- **"Test" Column**: `YES` for verified by automated tests, `PARTIAL` for code-only (no automated test), `MANUAL` for manual/hardware verification only.

---

## 1. Display Capture (DISP)

### REQ-DISP-1: Direct Scanout Capture from DRM/KMS
**Reason**: Capture the currently displayed screen (including console/fbcon) directly without relying on a compositor or X11/Wayland server, providing lightweight, display-manager-independent operation.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DISP-1-1 | Scan `/dev/dri/card*` to discover connectors, CRTCs, and primary planes | `capture/drm_discover.rs` | YES |
| SPC-DISP-1-2 | Memory-map bound CRTC framebuffers via PRIME DMA-BUF and perform CPU cache synchronization with `DMA_BUF_IOCTL_SYNC` | `capture/dmabuf.rs` | YES |
| SPC-DISP-1-3 | Support single-plane XRGB8888 and ARGB8888 framebuffer formats | `capture/drm_capturer.rs` | YES |
| SPC-DISP-1-4 | Detile tiled modifiers (Intel/AMD) to linear BGRX via GBM/EGL | `gpu_detile.rs` | YES (`detile_selftest` binary) |
| SPC-DISP-1-5 | Fail startup if the initial frame cannot be captured (unbound CRTC and NvFBC unavailable) | `capture/drm_capturer.rs` | PARTIAL |
| SPC-DISP-1-6 | Log rate-limited warnings for transient post-startup capture drops while keeping the process running | `capture/drm_capturer.rs` | PARTIAL |
| SPC-DISP-1-7 | Detect display hotplug events and refresh connector/CRTC metadata | `capture/drm_discover.rs` | PARTIAL |

### REQ-DISP-2: NVIDIA NvFBC Fallback
**Reason**: Proprietary NVIDIA driver environments do not bind CRTCs on DRM (controlled directly by Xorg), preventing direct DRM capture.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DISP-2-1 | Fall back to NvFBC capture when no CRTC is bound on DRM | `nvfbc.rs` | PARTIAL (Hardware verified on Xorg/NVIDIA) |
| SPC-DISP-2-2 | Validate frame sizes returned by NvFBC driver instead of trusting them blindly | `nvfbc.rs` | YES |
| SPC-DISP-2-3 | Disable NvFBC fallback when a single connector is explicitly requested via `KMSRDP_DISPLAY` | `capture/display_mode.rs` | PARTIAL |

### REQ-DISP-3: Multi-Monitor Compositing
**Reason**: Present multi-display host setups as a unified RDP desktop canvas.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DISP-3-1 | Composite all connected CRTCs onto one canvas by default (`KMSRDP_DISPLAY=all` or unset) | `capture/drm_capturer.rs`, `capture/display_mode.rs` | PARTIAL |
| SPC-DISP-3-2 | Capture a single connector when specified by `KMSRDP_DISPLAY=<connector>` (e.g. `DP-1`, `card1:DP-1`) | `capture/display_mode.rs` | PARTIAL |
| SPC-DISP-3-3 | Send RDP Monitor Layout PDU to client when two or more CRTCs are composited | `server/handshake.rs` | PARTIAL |
| SPC-DISP-3-4 | Treat multi-monitor setups as a single composited canvas rather than true per-monitor RDP windows (Limitation) | — | Documented constraint |

### REQ-DISP-4: Zero-Allocation Pixel Diffing
**Reason**: Avoid wasting CPU cycles and network bandwidth by re-encoding unchanged screen regions.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DISP-4-1 | Detect pixel differences between frames in 64×64 tile blocks (`find_dirty_rects`) | `capture/pixel_diff.rs` | YES |
| SPC-DISP-4-2 | Perform diffing before buffer allocation (`Arc<[u8]>`) and skip allocation on unchanged frames | `capture/pixel_diff.rs`, `server/diff.rs` | YES (Pointer equality test) |
| SPC-DISP-4-3 | Restrict pixel diffing to visible canvas bounds (ignore row stride padding) | `capture/pixel_diff.rs` | YES |

### REQ-DISP-5: Display Encoding (ENC)
**Reason**: Select optimal encoding schemes (RDP 6.0 Planar, NSCodec, MS-RDPEGFX) based on client capability negotiation.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DISP-5-1 | Default to RDP 6.0 Planar RLE (SurfaceCommands) for compatibility with standard clients (mstsc, xfreerdp) | `rdpcore-pdu`, `server/encode.rs` | YES |
| SPC-DISP-5-2 | Force NSCodec for macOS "Windows App" (`mac`/`darwin`/`iphone`/`ipad` client names) | `server/encode.rs` | PARTIAL |
| SPC-DISP-5-3 | Enable MS-RDPEGFX (AVC420, OpenH264) when requested via `KMSRDP_GFX=1` (disabled by default) | `crates/rdpcore-rdpegfx` | PARTIAL |
| SPC-DISP-5-4 | Negotiate GFX AVC420 capabilities during CapsAdvertise exchange (`CAP_VERSION_8`..`104`) | `rdpcore-rdpegfx/src/select.rs` | PARTIAL |
| SPC-DISP-5-5 | Support optional hardware encoding via `gfx-vaapi` / `gfx-nvenc` cargo features (OpenH264 CPU fallback) | `nvenc_enc.rs`, `vaapi_enc.rs`, `openh264_enc.rs` | YES (Unit tests) |
| SPC-DISP-5-6 | Separate GFX encoder mutex locks to prevent CPU encoding from blocking `FrameAcknowledge` processing | `rdpcore-rdpegfx/src/encoder.rs` | YES |
| SPC-DISP-5-7 | Send Save Session Info (PLAINNOTIFY) PDU immediately following connection establishment | `server/handshake.rs` | PARTIAL |
| SPC-DISP-5-8 | Offload heavy frame encoding tasks to `spawn_blocking` worker pools | `server/frame_pump` | PARTIAL |

---

## 2. Input Handling (INPUT)

### REQ-INPUT-1: Mouse & Keyboard Input Injection
**Reason**: Inject client input events into the host OS via local `uinput` virtual devices and X11 XTest.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-INPUT-1-1 | Parse Fast-Path input PDUs (mouse motion, clicks, wheel, keyboard scancodes, Unicode) | `server/input_handler`, `rdpcore-server/src/input.rs` | YES |
| SPC-INPUT-1-2 | Inject mouse and keyboard events via `/dev/uinput` | `kmsrdp/src/uinput.rs` | YES (`verify_input` tool) |
| SPC-INPUT-1-3 | Map Japanese 106/109 keycodes (Henkan, Muhenkan, Kana, Zenkaku/Hankaku) and media keys | `kmsrdp/src/uinput.rs` | YES |
| SPC-INPUT-1-4 | Scope input device state to connection sessions and release held keys upon disconnect | `kmsrdp/src/uinput.rs`, `session.rs` | YES |
| SPC-INPUT-1-5 | Inject CJK IME text on X11 sessions via XTest (not supported on Wayland-only sessions) | `kmsrdp/src/x11_unicode.rs` | YES |
| SPC-INPUT-1-6 | Auto-detect X11 `DISPLAY`/`XAUTHORITY` from logind, session leader, or `/tmp/.X11-unix/X*` sockets | `kmsrdp/src/x11_unicode.rs`, `systemd.rs` | PARTIAL |
| SPC-INPUT-1-7 | Support decoding and dispatch of Slow-Path Input PDUs (`ShareDataPduType::Input` / `0x1C`) | `crates/rdpcore-pdu/src/finalization.rs`, `crates/rdpcore-server/src/server/slow_path.rs` | YES |

---

## 3. Clipboard (CLIP)

### REQ-CLIP-1: Text Clipboard Synchronization (CLIPRDR)
**Reason**: Enable bidirectional text copy and paste between host and remote client.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-CLIP-1-1 | Support text-only clipboard format in CLIPRDR (images and file objects excluded) | `crates/rdpcore-cliprdr`, `kmsrdp/src/clipboard.rs` | YES |
| SPC-CLIP-1-2 | Share a single process-wide local clipboard poller across all sessions | `kmsrdp/src/clipboard.rs` | YES |
| SPC-CLIP-1-3 | Support configurable sync modes (`KMSRDP_CLIPBOARD=bidirectional`, `host-to-client`, `client-to-host`, `disabled`) | `kmsrdp/src/config.rs`, `clipboard.rs` | YES |
| SPC-CLIP-1-4 | Suppress self-originated clipboard echoes to prevent Guacamole buffer reverts | `kmsrdp/src/clipboard.rs` | YES |
| SPC-CLIP-1-5 | Accept and decode ANSI `CF_TEXT` (1) and `CF_OEMTEXT` (7) in addition to UTF-16LE `CF_UNICODETEXT` (13) | `crates/rdpcore-cliprdr/src/pdu.rs`, `kmsrdp/src/clipboard.rs` | YES |
| SPC-CLIP-1-6 | Maintain persistent `arboard::Clipboard` instances in a dedicated worker thread (`kmsrdp-clip-worker`) to preserve X11 Selection Ownership | `kmsrdp/src/clipboard.rs` | YES |

---

## 4. Audio Redirection (AUDIO)

### REQ-AUDIO-1: Audio Output Redirection (RDPSND)
**Reason**: Stream host audio playback to remote clients.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-AUDIO-1-1 | Capture audio via libpulse (PipeWire/PulseAudio) and stream over per-connection RDPSND channels | `kmsrdp/src/audio.rs`, `crates/rdpcore-rdpsnd` | YES |
| SPC-AUDIO-1-2 | Advertise RDPSND v8 and use Wave2 format for v8+ clients (fallback to legacy WaveInfo+Wave for older clients) | `rdpcore-rdpsnd/src/lib.rs` | YES |
| SPC-AUDIO-1-3 | Prevent live-slot clobbering and RTT calculation poisoning | `rdpcore-rdpsnd/src/lib.rs` | YES |
| SPC-AUDIO-1-4 | Prioritize audio packets (`Priority::Latency`) over display updates (`Priority::Bulk`) | `crates/rdpcore-transport/src/scheduler.rs` | YES |

### REQ-AUDIO-2: Microphone Input Redirection (RDPEAI)
**Reason**: Stream remote client microphone input to host virtual mic sinks.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-AUDIO-2-1 | Route client microphone audio to PulseAudio virtual mic sinks per connection | `kmsrdp/src/audio_input.rs`, `crates/rdpcore-rdpeai` | YES |

---

## 5. Drive Redirection (DRIVE)

### REQ-DRIVE-1: FUSE Client Drive Mounts (RDPDR)
**Reason**: Expose client-redirected local drives as standard Linux filesystems on the host.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-DRIVE-1-1 | Mount RDPDR drives via FUSE under `$XDG_RUNTIME_DIR/kmsrdp/drives/<DosName>` | `kmsrdp/src/rdpdr_fuse/mount.rs` | YES |
| SPC-DRIVE-1-2 | Support filesystem operations: list, read, write, create, mkdir, unlink, rmdir, rename, and setattr | `kmsrdp/src/rdpdr_fuse/fs.rs`, `crates/rdpcore-rdpdr/src/irp.rs` | YES |
| SPC-DRIVE-1-3 | Share mounted drives across sessions until the final session disconnects | `kmsrdp/src/rdpdr_fuse/mount.rs`, `bridge.rs` | PARTIAL |
| SPC-DRIVE-1-4 | Treat `chmod`/`chown` as local FUSE metadata updates without propagating to the client filesystem | `kmsrdp/src/rdpdr_fuse/fs.rs` | YES |
| SPC-DRIVE-1-5 | Printer/CUPS redirection unsupported (Limitation) | — | Unimplemented |
| SPC-DRIVE-1-6 | Require `user_allow_other` in `/etc/fuse.conf` to expose root FUSE mounts to other users | `kmsrdp/src/rdpdr_diagnostic.rs` | PARTIAL |
| SPC-DRIVE-1-7 | Sanitize path components to prevent path traversal attacks beyond the mount root | `kmsrdp/src/rdpdr_path.rs` | YES |

---

## 6. Authentication & Transport (AUTH)

### REQ-AUTH-1: TLS & Password Authentication
**Reason**: Protect RDP sessions with encryption and user credential verification.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-AUTH-1-1 | Upgrade all cleartext X.224 connections to TLS | `crates/rdpcore-connector`, `kmsrdp/src/tls.rs` | YES |
| SPC-AUTH-1-2 | Use persisted self-signed TLS certificates (`StateDirectory=kmsrdp` or `KMSRDP_TLS_*`) | `kmsrdp/src/tls.rs` | YES |
| SPC-AUTH-1-3 | Support ephemeral certificate generation via `KMSRDP_TLS_EPHEMERAL=1` | `kmsrdp/src/tls.rs` | YES |
| SPC-AUTH-1-4 | Specify custom SAN hostnames/IPs via `KMSRDP_TLS_HOSTS` on initial cert generation | `kmsrdp/src/tls.rs` | YES |
| SPC-AUTH-1-5 | Read passwords from `KMSRDP_PASSWORD`, `KMSRDP_PASSWORD_FILE`, or systemd `LoadCredential` | `kmsrdp/src/config.rs`, `credentials.rs` | YES |
| SPC-AUTH-1-6 | Specify target username via `KMSRDP_USER` | `kmsrdp/src/config.rs` | YES |

### REQ-AUTH-2: NLA (CredSSP / NTLMv2)
**Reason**: Authenticate clients before completing TLS setup to prevent plaintext credential exposure.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-AUTH-2-1 | Require NLA by default (`KMSRDP_REQUIRE_NLA=0` to allow TLS-only Client Info fallback) | `crates/rdpcore-server/src/credssp.rs`, `config.rs` | YES |
| SPC-AUTH-2-2 | Support CredSSP/NTLMv2 for NLA (Kerberos unsupported) | `credssp.rs` | YES |
| SPC-AUTH-2-3 | Enforce rate-limiting on failed authentication attempts | `crates/rdpcore-server/src/auth_limit.rs` | YES |

### REQ-AUTH-3: Listener Configuration
**Reason**: Configure bind address, port, and concurrent session limits.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-AUTH-3-1 | Default bind address `127.0.0.1:3389` (configurable via `KMSRDP_BIND` / `KMSRDP_PORT`) | `kmsrdp/src/config.rs` | YES |
| SPC-AUTH-3-2 | Limit concurrent authenticated sessions (default 1, max 32 via `KMSRDP_MAX_SESSIONS`) | `kmsrdp/src/session.rs` | YES |
| SPC-AUTH-3-3 | Share single composited desktop, input device, and FUSE mount across concurrent sessions (Limitation) | `session.rs`, `uinput.rs` | Constraint |

---

## 7. Startup Diagnostics & Metrics (BOOT)

### REQ-BOOT-1: Pre-flight Validation
**Reason**: Validate permissions, environment variables, devices, and dependencies before serving connections.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-BOOT-1-1 | Validate listen port privileges, `KMSRDP_*` env, `/dev/uinput`, and helper binaries | `kmsrdp/src/config_check.rs` | YES |
| SPC-BOOT-1-2 | Fail startup hard on missing capabilities or inaccessible KMS devices | `config_check.rs` | YES |
| SPC-BOOT-1-3 | Log soft warnings for missing optional tools (audio/FUSE) while continuing startup | `config_check.rs`, `rdpdr_diagnostic.rs` | YES |
| SPC-BOOT-1-4 | Require `CAP_SYS_ADMIN`, `CAP_DAC_OVERRIDE`, and `CAP_NET_BIND_SERVICE` capabilities on binary | Packaging scripts | YES |

### REQ-BOOT-2: Structured Logging
**Reason**: Provide machine-readable logs for operations and troubleshooting.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-BOOT-2-1 | Support structured logging via `tracing` with filtering via `KMSRDP_LOG` | `kmsrdp/src/logging.rs` | YES |
| SPC-BOOT-2-2 | Support JSON output formatting via `KMSRDP_LOG_FORMAT=json` | `logging.rs` | YES |

### REQ-BOOT-3: Telemetry & Metrics
**Reason**: Expose session telemetry and performance metrics for Prometheus monitoring.

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-BOOT-3-1 | Serve Prometheus metrics at `KMSRDP_METRICS_LISTEN` (`/metrics`, `/healthz`) | `kmsrdp/src/metrics.rs`, `metrics_server.rs` | YES |
| SPC-BOOT-3-2 | Accept integer port or full `SocketAddr` format for metrics listener (reject port 0) | `kmsrdp/src/config.rs` | YES |
| SPC-BOOT-3-3 | Leave metrics endpoint uninitialized when environment variable is unset | `config.rs` | YES |

---

## 8. Non-Functional Requirements (NFR)

### REQ-NFR-1: Security
| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-NFR-1-1 | Do not expose listen ports directly to public Internet | Deployment documentation | N/A |
| SPC-NFR-1-2 | Redact credentials from debug and log output (`[REDACTED]`) | `logging.rs`, `credentials.rs` | YES |
| SPC-NFR-1-3 | Maintain zero high-severity vulnerabilities across protocol decoders and transport layers | `docs/QUALITY.md` | YES |
| SPC-NFR-1-4 | Handle security vulnerability reports via GitHub Security Advisories | `docs/SECURITY.md` | Process |

### REQ-NFR-2: Performance
| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-NFR-2-1 | Default frame rate to 20 FPS (configurable via `KMSRDP_FPS` / `KMSRDP_FRAME_INTERVAL_MS`) | `kmsrdp/src/config.rs`, `display_hub.rs` | YES |
| SPC-NFR-2-2 | Pool and reuse tile buffers, OpenH264 I420 conversion buffers, and GPU readback buffers | `server/encode.rs`, `openh264_enc.rs`, `gpu_detile.rs` | YES |
| SPC-NFR-2-3 | Skip re-encoding and transmission for unchanged frames | `capture/pixel_diff.rs`, `server/diff.rs` | YES |

### REQ-NFR-3: Concurrency & Reliability
| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-NFR-3-1 | Offload blocking encoding and IPC operations to `spawn_blocking` worker pools | `server/frame_pump`, `rdpcore-rdpegfx/src/encoder.rs` | YES |
| SPC-NFR-3-2 | Recover shared Mutex locks gracefully from thread panics | Server and session modules | YES |
| SPC-NFR-3-3 | Apply backpressure with 15-second timeouts on frame write queues | `crates/rdpcore-transport` | YES |

### REQ-NFR-4: Quality & Testing
| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-NFR-4-1 | Maintain comprehensive unit test coverage across workspace crates | `docs/QUALITY.md` | YES |
| SPC-NFR-4-2 | Apply `proptest` property-based testing across core PDU codecs and virtual channels | Workspace unit test suites | YES |
| SPC-NFR-4-3 | Run FreeRDP E2E handshake integration tests in CI harness | `tests/e2e_freerdp.rs` | YES |

### REQ-NFR-5: Maintainability
| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-NFR-5-1 | Maintain architecture, safety, and verification rules in `docs/AGENTS.md` | `docs/AGENTS.md` | N/A |
| SPC-NFR-5-2 | Maintain system component map and Mermaid sequence diagrams in `docs/ARCHITECTURE.md` | `docs/ARCHITECTURE.md` | N/A |
| SPC-NFR-5-3 | Track quality evaluation snapshots in `docs/QUALITY.md` | `docs/QUALITY.md` | N/A |

---

## 9. Packaging & Distribution (PKG)

| Spec ID | Specification | Implementation | Test |
|---|---|---|---|
| SPC-PKG-1-1 | Publish AlmaLinux 9 RPM and Ubuntu `.deb` packages on GitHub Releases (`v*.*.*`) | `Makefile`, `kmsrdp.spec`, `debian/` | YES |
| SPC-PKG-1-2 | Maintain `Cargo.toml`, `kmsrdp.spec`, and `debian/changelog` versions in strict lockstep | Release procedure | YES |
| SPC-PKG-1-3 | Provide systemd user unit (single session) and system unit (logind watcher) | Systemd unit manifests | PARTIAL |
