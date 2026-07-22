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
Version:        0.1.34
Release:        1%{?dist}
Summary:        DRM/KMS-based RDP remote desktop server (pure Rust)

License:        MIT OR Apache-2.0
URL:            %{forgeurl}
Source0:        %{name}-%{version}.tar.gz
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
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
