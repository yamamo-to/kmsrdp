# AlmaLinux / RHEL 9 packaging for kmsrdp.
#
# The kmsrdp repository is private, so codeload.github.com archive URLs
# 404 without an auth token; both sources are generated locally by `make
# vendor` (`git archive` for Source0, `cargo vendor` for Source1, which a
# mock/COPR build needs since it has no network access) instead of being
# fetched from a URL.

%global forgeurl https://github.com/yamamo-to/kmsrdp
# Cargo-built binaries don't line up with rpm's debugsource expectations
# (paths point into vendor/ and ~/.cargo, not a rebuildable layout), so the
# auto-generated debugsource subpackage ends up empty and fails the build.
%global debug_package %{nil}

Name:           kmsrdp
Version:        0.1.62
Release:        1%{?dist}
Summary:        DRM/KMS-based RDP remote desktop server (pure Rust)

License:        MIT OR Apache-2.0
URL:            %{forgeurl}
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  gcc-c++
BuildRequires:  fuse3-devel
BuildRequires:  pulseaudio-libs-devel
BuildRequires:  systemd-rpm-macros

Requires:       libcap
Requires:       fuse3
Requires(post): libcap

%description
kmsrdp is a from-scratch remote desktop server for Linux, inspired by
ReFrame's compositor-bypass architecture (DRM/KMS capture + uinput input
injection) but speaking RDP instead of VNC via its own from-scratch RDP
protocol implementation (no ironrdp or other RDP library dependency). It
supports screen capture, mouse/keyboard input, Japanese/CJK IME text
injection (X11 sessions), bidirectional clipboard sync, audio output and
microphone redirection, FUSE mounts for redirected client drives, TLS +
username/password authentication (optional NLA), and priority-aware
scheduling so video traffic can't starve audio.

Known limitations: Linear (non-tiled) framebuffers only, single monitor,
and no printer redirection (CUPS) yet. See the upstream README for details.

%prep
%autosetup -p1 -n %{name}-%{version}
tar -xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"
EOF

%build
cargo build --release --offline --bin rdp_server

%install
install -D -m755 target/release/rdp_server %{buildroot}%{_libexecdir}/%{name}/%{name}-server
install -D -m644 dist/%{name}.service %{buildroot}%{_userunitdir}/%{name}.service
install -D -m644 dist/%{name}.env.example %{buildroot}%{_docdir}/%{name}/%{name}.env.example
install -D -m644 dist/%{name}-system.service %{buildroot}%{_unitdir}/%{name}.service
install -D -m644 dist/%{name}-system.env.example %{buildroot}%{_docdir}/%{name}/%{name}-system.env.example

%post
setcap cap_sys_admin,cap_dac_override,cap_net_bind_service+ep %{_libexecdir}/%{name}/%{name}-server || :
cat <<MSG
kmsrdp installed. Two ways to run it:

Per user (single session, no root):
  mkdir -p ~/.config/kmsrdp
  cp %{_docdir}/%{name}/kmsrdp.env.example ~/.config/kmsrdp/kmsrdp.env
  chmod 600 ~/.config/kmsrdp/kmsrdp.env
  \$EDITOR ~/.config/kmsrdp/kmsrdp.env   # set KMSRDP_USER / KMSRDP_PASSWORD
  systemctl --user enable --now kmsrdp.service

As root (follows whichever login session is active):
  mkdir -p /etc/kmsrdp
  install -m600 %{_docdir}/%{name}/kmsrdp-system.env.example /etc/kmsrdp/kmsrdp.env
  \$EDITOR /etc/kmsrdp/kmsrdp.env   # set KMSRDP_USER / KMSRDP_PASSWORD
  systemctl enable --now kmsrdp.service
MSG

%files
%license LICENSE-MIT LICENSE-APACHE
%doc README.md
%{_libexecdir}/%{name}/%{name}-server
%{_userunitdir}/%{name}.service
%{_unitdir}/%{name}.service
%{_docdir}/%{name}/%{name}.env.example
%{_docdir}/%{name}/%{name}-system.env.example

%changelog
* Fri Sep 04 2026 kmsrdp contributors <noreply@example.com> - 0.1.62-1
- Extend resync catch-up (union/diff against last confirmed frame) to Planar clients (mstsc, xfreerdp), not just NSCodec - fixes stuck or incorrect screen regions when a busy session dropped a dirty rect that no later update overlapped

* Tue Aug 25 2026 kmsrdp contributors <noreply@example.com> - 0.1.61-1
- Fix NSCodec catch-up backlog growing unboundedly under sustained display churn (e.g. video), causing multi-minute latency
- Gate NSCodec catch-up on real kernel TCP send backlog, not just our own send-queue bookkeeping
- Fix GFX backpressure forcing a bigger IDR frame into an already backed-up client
- Fix nvenc feature failing to compile (EncoderError type mismatch)

* Tue Aug 25 2026 kmsrdp contributors <noreply@example.com> - 0.1.60-1
- Fix Guacamole clipboard synchronization and echo loop suppression
- Support CF_TEXT and CF_OEMTEXT formats alongside CF_UNICODETEXT
- Keep persistent arboard Clipboard instance in dedicated worker thread
- Fix X11 Unicode Japanese text input keycode clearing race condition

* Tue Aug 25 2026 kmsrdp contributors <noreply@example.com> - 0.1.59-1
- Add Prometheus metrics HTTP endpoint (/metrics, /healthz)
- Add lock-free global metrics collection and Prometheus text exporter
- Add GFX (H.264 AVC420) frame and byte telemetry in SessionBitmapMetrics
- Add in-process mock client E2E tests for MS-RDPEAI and MS-RDPDR
- Prevent X11 fake key injection side effects on active desktop during tests

* Tue Aug 25 2026 kmsrdp contributors <noreply@example.com> - 0.1.58-1
- Keep RDPSND at the live capture edge (Pulse flush, 1x, newest 20 ms)
- Gate Wave2 sends on measured client play-queue hold, not receive-acks
- Read WaveConfirms during full-screen bitmap send so the unacked window
  cannot grow while graphics blocks the session loop

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.57-1
- Validate NvFBC's returned frame length instead of trusting it blindly
- Fix stuck-key bug: autorepeat Presseds no longer inflate the shared
  uinput hold refcount (X11 could retype a held key indefinitely)
- Blank IME Unicode-injection scratch keycodes after use
- Split large source files and add documentation (no behavior change)

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.56-1
- Close remaining bulk-queue-full mid-sequence truncation gaps; bound
  send_all's wait so a hung client can't wedge the connection forever
- Fix WavePublisher::publish's mutex-poisoning inconsistency
- Reuse the I420 scratch buffer in the OpenH264 GFX path per frame
- Recycle per-tile bitmap buffers across frames instead of cloning
- Move the GFX H.264 encoder to its own lock, out from under session state
- Reuse the GPU-detile readback buffer instead of allocating per tick

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.55-1
- Fix RDPSND live-slot clobber and WaveConfirm RTT poisoning
- Add no-panic proptest coverage for virtual-channel decoders
- Fix bulk-queue-full mid-frame truncation and spurious session teardown

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.54-1
- Keep RDPSND live: cap Pulse capture, drain to the live edge, pace sends
- Overwrite unread waves on a slow socket instead of queueing PCM
- Bound outstanding Wave2 blocks to WaveConfirm so client lag cannot grow

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.53-1
- Advertise RDPSND v8 so clients accept Wave2 instead of dropping audio
- Wait for Quality Mode on v6+ clients and fall back to WaveInfo+Wave for older ones

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.52-1
- Modularize server and capture monoliths into structured submodules
- Add FreeRDP E2E integration test suite with Xvfb harness
- Add comprehensive ARCHITECTURE.md documentation and sequence diagrams
- Clean up rustdoc intra-doc link warnings and enforce pre-commit cargo fmt

* Mon Aug 24 2026 kmsrdp contributors <noreply@example.com> - 0.1.51-1
- Replace anyhow with structured thiserror types (DisplayError, ServerError) in rdpcore-server
- Optimize bitmap encode and avoid blocking Tokio worker threads
- Enforce codebase formatting consistency with cargo fmt

* Sun Aug 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.50-1
- Replace encoder String errors with structured EncoderError (thiserror)
- Optimize BGRX-to-YUV420 color conversion by pre-slicing row pointers
- Expand SmallBitSet to 8K-capable inline buffer to avoid heap allocation
- Add systemd sd_notify READY/WATCHDOG/STOPPING integration
- Harden session diagnostics with typed errors and capability-based codecs
- Close protocol test gaps with broader proptest and fuzz coverage

* Sun Aug 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.49-1
- Add DMA_BUF_IOCTL_SYNC cache synchronization around CPU mmap reads in KMS capture
- Handle FUSE inode metadata lookup failures and thread spawn gracefully without panicking
- Fix ASN.1 BER INTEGER positive sign bit encoding for values >= 0x8000_0000
- Fix FastPathInput encoding for zero events to include explicit count byte per MS-RDPBCGR
- Add Japanese keyboard (Henkan/Muhenkan/Kana/Zenkaku) and media keycodes mapping in uinput
- Offload synchronous clipboard IPC to spawn_blocking in LocalClipboardBackend
- Guard buffer slicing in bitmap diff, tile encoding, pointer, and rdp6 decode_plane
- Eliminate unchecked unwraps in ReadCursor::read_u64_le

* Sun Aug 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.48-1
- Clear KMSRDP_PASSWORD from environment after reading to reduce memory exposure
- Add automatic recovery and backoff for GPU detiler on render failure
- Use poison-tolerant Mutex recovery in audio input initialization
- Use libc::poll in XFixes clipboard event loop to avoid 50ms busy sleep
- Optimize Planar RDP 6.0 zigzag delta filtering to be branchless and SIMD-friendly
- Direct map Latin-1 codepoints to standard X11 keysyms
- Sanitize odd-length UTF-16 strings in ClientInfo PDU decoding
- Support 32bpp RGBA cursor encoding and sync default pointer on connect

* Sun Aug 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.47-1
- Parse GFX/FPS in Config and pass them through the server builder
  (rdpcore-server no longer reads KMSRDP_* itself)
- Fall back from GFX AVC420 to Planar/NSCodec after repeated H.264
  encode failures, without dropping the session
- Cover Session::negotiate with TLS loopback tests
- Clipboard sync fix, auth/TLS hardening, RDPDR FUSE modularization,
  and encoder/detile performance work since 0.1.46

* Fri Aug 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.46-1
- Decouple RDPSND wave delivery from the steady-state session loop
- Release stuck keys/mouse buttons on RDP disconnect
- Broad correctness/security hardening across the RDP stack (write
  scheduler priority, RDPDR device lifecycle, NVENC resource teardown,
  handshake timeouts, and more)
- Performance and code-quality pass: TCP_NODELAY, fewer per-frame
  allocations/copies in capture and video encoding, deduplicated
  protocol decode helpers
- Fixes packaging: this spec's Version had drifted from Cargo.toml
  since 0.1.44, so 0.1.44/0.1.45 never actually shipped correctly
  versioned RPMs

* Fri Aug 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.43-1
- Drain stale PulseAudio/PipeWire capture buffer to prevent audio lag accumulation
- Use monotonic real-time timestamps in RDPSND Wave2 frames

* Fri Aug 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.42-1
- Keep RDPSND live with a single latest-wins Wave slot
- Drop late PCM instead of streaming Pulse/FIFO backlog to the client
- Hint PipeWire/Pulse toward 20 ms before capture connects

* Fri Aug 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.41-1
- Keep RDPSND realtime with latest-wins Wave handoff
- Overwrite unread PCM when GFX encode stalls the session loop
- Flush newest audio immediately after each display frame

* Thu Aug 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.40-1
- Security and privacy hardening release
- Constant-time password validation using subtle::ConstantTimeEq
- Add configurable NLA enforcement (KMSRDP_REQUIRE_NLA / KMSRDP_FORCE_NLA)
- Add granular clipboard synchronization policies (KMSRDP_CLIPBOARD)
- Harden TLS private key creation with random nonce and O_EXCL

* Thu Aug 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.39-1
- Constant-time password validation using subtle::ConstantTimeEq
- Add configurable NLA enforcement (KMSRDP_REQUIRE_NLA / KMSRDP_FORCE_NLA)
- Add granular clipboard synchronization policies (KMSRDP_CLIPBOARD)
- Harden TLS private key creation with random nonce and O_EXCL

* Thu Aug 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.38-1
- Security fixes: PDU string decoding bounds check, lchown for FUSE mounts
- Enforce maximum payload limits (CredSSP 64KB, DVC/Cliprdr/RDPDR 16MB)
- Validate ClientInfo username against NLA CredSSP authenticated identity
- Mask Credentials passwords in Debug output

* Thu Jul 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.37-1
- Expand MS-RDPEGFX unit tests (Annex B wire, Caps/session, OpenH264 smoke)

* Thu Jul 23 2026 kmsrdp contributors <noreply@example.com> - 0.1.36-1
- Add experimental MS-RDPEGFX AVC420 path (OpenH264; optional VAAPI/NVENC)
- Opt-in at runtime with KMSRDP_GFX=1; Annex B bitstream per MS-RDPEGFX

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.35-1
- Ignore rsa Marvin Attack advisory (no upstream fix; CredSSP/NTLM only)
- Fix fuzz CI musl/ASAN; reject hostile Monitor Layout counts (OOM)

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.34-1
- Cut Pulse/PipeWire audio buffer defaults (~2s) to ~20ms for RDPSND/RDPEAI

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.33-1
- Replace parec/paplay/pactl with in-process libpulse for RDPSND and RDPEAI
- Load virtual mic null-sink via libpulse; add pulse_util unit tests

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.32-1
- CI: cargo audit/deny, llvm-cov coverage, release build, fuzz smoke (nightly)
- Add deny.toml dependency policy; PDU/RDPDR fuzz targets and wire-stack tests
- Unit tests: clipboard, audio, session_watcher, logging, x11_unicode

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.31-1
- Drive FUSE polish: rename flags, empty rmdir check, local chmod/chown attrs
- Expand unit tests across rdpcore-pdu, rdpcore-rdpdr, and kmsrdp

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.30-1
- Drive FUSE: delete, rename, and setattr (size/times)
- Silence unused FUSE flush/fsync/xattr warnings

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.29-1
- Validate config, capabilities, and helper binaries at startup

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.28-1
- Persist self-signed TLS identity across restarts
- Surface capture failures with actionable hints at startup
- Structured logging via tracing (KMSRDP_LOG / KMSRDP_LOG_FORMAT)

* Wed Jul 22 2026 kmsrdp contributors <noreply@example.com> - 0.1.27-1
- Auto-detect X11 DISPLAY on tty/startx sessions for CJK Unicode injection

* Tue Jul 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.26-1
- Faster stop on shutdown (TimeoutStopSec=5, SIGTERM/SIGINT immediate exit)

* Tue Jul 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.25-1
- Composite multi-monitor capture with KMSRDP_DISPLAY selection
- Save Session Info PLAINNOTIFY; Monitor Layout when compositing 2+ CRTCs

* Tue Jul 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.24-1
- Frame Marker, Suppress Output / Refresh Rect, and MaxRequestSize handling
- Leave mouse pointer drawing to the client (no soft-cursor PDUs)

* Tue Jul 21 2026 kmsrdp contributors <noreply@example.com> - 0.1.23-1
- Document cbScanWidth bytes-vs-pixels interop note (MS-RDPBCGR vs mstsc)

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.22-1
- Fix clippy warnings (useless_vec, too_many_arguments) for CI

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.21-1
- Apply rustfmt fixes for CI

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.20-1
- Fix RDP6 cbScanWidth encoding so mstsc displays after handshake

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.19-1
- Update README for NSCodec, listen-address config, and limitation accuracy

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.18-1
- Silence rdpcore-server compile warnings for unused channel IDs

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.17-1
- NSCodec SurfaceCommands for macOS Windows App; Mac clipboard startup delay

* Mon Jul 20 2026 kmsrdp contributors <noreply@example.com> - 0.1.16-1
- Add KMSRDP_BIND and KMSRDP_PORT for listen address and port

* Sun Jul 19 2026 kmsrdp contributors <noreply@example.com> - 0.1.15-1
- Shorten README and correct shared-clipboard description

* Sun Jul 19 2026 kmsrdp contributors <noreply@example.com> - 0.1.14-1
- Add Debian/Ubuntu .deb packaging and CI release artifact

* Sun Jul 19 2026 kmsrdp contributors <noreply@example.com> - 0.1.13-1
- Share wrap_indication in rdpcore-pdu; unify FUSE directory enumeration
- Refresh stale Phase/later-phase comments (FUSE, DVC, scheduler, RDPDR)

* Sun Jul 19 2026 kmsrdp contributors <noreply@example.com> - 0.1.12-1
- Narrow tokio features; share one clipboard poller across RDP connections
- Disable arboard image-data by default; gate RDPDR diagnostic and DVC echo behind features
- Share full-frame bitmaps via Arc to avoid duplicate framebuffer copies

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.11-1
- Hand off shared FUSE ownership by swapping the RDPDR backend without umount
- Detach last-connection umount so disconnect cannot block other RDP sessions

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.10-1
- Share one FUSE mount per DosName across RDP connections; release on last disconnect
- Hand off the RDPDR owner when the mounting connection leaves first

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.9-1
- Per-connection FUSE mounts so concurrent sessions no longer share/unmount one path
- Abort pending RDPDR waiters on disconnect so umount does not block for 60s

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.8-1
- Reap parec/paplay children with wait() to stop Guacamole session zombies
- Join audio capture and FUSE threads; stop clipboard watcher on disconnect

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.7-1
- Mount redirected client drives via FUSE under the session runtime dir
- Prefer HYBRID CredSSP wake path for RDPDR I/O; clear stale FUSE mounts

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.6-1
- Add optional NLA (CredSSP/NTLMv2) with HYBRID preferred and TLS fallback
- Pass TLS subjectPublicKey bytes so FreeRDP/Guacamole pubKeyAuth verifies

* Sat Jul 18 2026 kmsrdp contributors <noreply@example.com> - 0.1.5-1
- Complete mstsc reactivation after a server-side desktop resize
- Preserve and send the post-resize full frame after capability negotiation
- Handle batched MCS finalization PDUs and interleaved static-channel traffic

* Fri Jul 17 2026 kmsrdp contributors <noreply@example.com> - 0.1.4-1
- Publish the latest full frame before broadcasting dirty updates so
  lagged clients recover the current scene instead of stale X tiles
- Increase the display broadcast buffer for slow RDP clients

* Fri Jul 17 2026 kmsrdp contributors <noreply@example.com> - 0.1.3-1
- Force a full-frame refresh when the DRM framebuffer changes
- Lower the dirty-area threshold to prevent stale tiles after X logout

* Fri Jul 17 2026 kmsrdp contributors <noreply@example.com> - 0.1.2-1
- Keep the DRM card fd open across captures so the text console is
  restored after an X session logs out (no more stale X wallpaper)

* Fri Jul 17 2026 kmsrdp contributors <noreply@example.com> - 0.1.1-1
- Add CAP_NET_BIND_SERVICE so the service can bind TCP 3389
- Bind the listener before creating the uinput device to avoid
  restart-loop uinput spam on startup failure

* Wed Jul 15 2026 kmsrdp contributors <noreply@example.com> - 0.1.0-1
- Initial packaging
