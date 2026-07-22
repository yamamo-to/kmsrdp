# kmsrdp

DRM/KMS-based RDP remote desktop server for Linux, in pure Rust.

Captures the screen via the kernel DRM/KMS path (no compositor hook) and
injects input through `uinput`, similar to
[ReFrame](https://github.com/AlynxZhou/reframe), but speaks RDP instead of
VNC. The RDP stack lives in `crates/rdpcore-*` (no `ironrdp` dependency).

> [!WARNING]
> Experimental. Authenticated clients get full screen, keyboard/mouse,
> clipboard, audio, and optional drive access. TLS uses a **persisted
> self-signed certificate** by default (regenerated only when missing);
> NLA is CredSSP/NTLMv2 only (no Kerberos).
> **Do not expose the RDP listen port (default 3389) to the public
> Internet** — use a firewall, VPN, or SSH tunnel on a trusted network.

## Features

- **Display:** DRM/KMS capture (Linear mmap or GBM/EGL detile); NVIDIA NvFBC
  fallback when no CRTC is bound; dirty 64×64 tiles; RDP 6.0 planar (typical
  clients) or NSCodec SurfaceCommands (macOS Windows App); composite all
  connected CRTCs by default (`KMSRDP_DISPLAY=all` / unset) or one connector
  (`KMSRDP_DISPLAY=DP-1` / `card1:DP-1`); Save Session Info (PLAINNOTIFY) on
  connect; Monitor Layout when two or more CRTCs are composited
- **Input:** `uinput` mouse/keyboard; CJK IME text on X11 (XTest)
- **Clipboard:** text-only CLIPRDR; one process-wide local poller shared by
  all sessions
- **Audio:** output (RDPSND / `parec`) and mic input (MS-RDPEAI); per connection
- **Drives:** RDPDR → FUSE at `$XDG_RUNTIME_DIR/kmsrdp/drives/<DosName>`
  (list/read/write/create/mkdir; shared until the last session leaves)
- **Auth / transport:** TLS + password; NLA (CredSSP/NTLMv2) when the client
  requests it; persisted self-signed cert by default (`StateDirectory` or
  `KMSRDP_TLS_*`); configurable listen address (`KMSRDP_BIND` /
  `KMSRDP_PORT`); structured logs via `tracing` (`KMSRDP_LOG` /
  `KMSRDP_LOG_FORMAT=json`); priority-aware writes so audio is not starved
  by graphics

## Limitations

- Concurrent clients share one (possibly composited) desktop and one input device
- Not true per-monitor RDP windows — multi-head is one virtual desktop canvas
- Framebuffers: single-plane XRGB8888/ARGB8888 only (tiled modifiers are
  detiled via GBM/EGL when needed)
- Startup fails hard if the first frame cannot be captured (no CRTC / NvFBC);
  later capture drops are logged with hints (rate-limited) instead of a silent
  black client
- Drive FUSE: no printer/CUPS yet
- CJK IME needs X11 (XTest); not available on Wayland-only sessions. `startx`
  on a tty session is detected automatically (`DISPLAY` / `XAUTHORITY` from
  logind, the session leader, or a sole `/tmp/.X11-unix/X*` socket)
- Needs `CAP_SYS_ADMIN`, `CAP_DAC_OVERRIDE`, `CAP_NET_BIND_SERVICE` on the binary

**Tested:** Proxmox VM (VirtIO-GPU/QXL) via Guacamole and direct clients;
NVIDIA/Xorg via NvFBC fallback; macOS Windows App (NSCodec). See module docs
for NvFBC / GBM details.

## Quick start

```bash
cargo build --release --bin rdp_server
sudo setcap cap_sys_admin,cap_dac_override,cap_net_bind_service+ep \
  target/release/rdp_server

KMSRDP_USER=myuser KMSRDP_PASSWORD=mypassword ./target/release/rdp_server
```

Connect with `xfreerdp /v:<host> /cert:ignore /u:myuser /p:mypassword`, mstsc,
or the macOS Windows App. Optional: `KMSRDP_BIND=127.0.0.1` /
`KMSRDP_PORT=3390` to restrict the listen address; `KMSRDP_TLS_HOSTS=host,1.2.3.4`
for certificate SANs (applied when the cert is first created — delete the
persisted files to regenerate); `KMSRDP_TLS_DIR` / `KMSRDP_TLS_CERT`+`KEY`
to choose where the identity is stored; `KMSRDP_TLS_EPHEMERAL=1` to skip
persistence; `KMSRDP_LOG=debug` / `KMSRDP_LOG_FORMAT=json` for structured
logs; `KMSRDP_DISPLAY=all` (default) to composite every CRTC, or
`DP-1` / `card1:DP-1` for a single connector (disables NvFBC fallback).

Audio needs `parec` / `paplay` / `pactl` on `$PATH`. Root FUSE mounts need
`user_allow_other` in `/etc/fuse.conf`. On startup kmsrdp validates listen
port privileges, `KMSRDP_*` env, `/dev/uinput`, and helper binaries — hard
errors refuse to start; missing audio/FUSE tools are warnings only.

## Packages

GitHub Releases (`v*.*.*`) attach an AlmaLinux 9 RPM and an Ubuntu `.deb`.

```bash
# RPM (Alma/RHEL 9)
make install-build-deps && make rpm
sudo dnf install .rpmbuild/RPMS/x86_64/kmsrdp-*.rpm

# .deb (Debian/Ubuntu; needs a recent rustup toolchain)
make install-deb-build-deps && make deb
sudo apt install ./.debbuild/kmsrdp_*.deb
```

## systemd

**User unit** (one graphical login):

```bash
mkdir -p ~/.config/kmsrdp
cp /usr/share/doc/kmsrdp/kmsrdp.env.example ~/.config/kmsrdp/kmsrdp.env
chmod 600 ~/.config/kmsrdp/kmsrdp.env   # set KMSRDP_USER / KMSRDP_PASSWORD
systemctl --user enable --now kmsrdp.service
```

**System unit** (follows the active login via logind):

```bash
sudo mkdir -p /etc/kmsrdp
sudo cp /usr/share/doc/kmsrdp/kmsrdp-system.env.example /etc/kmsrdp/kmsrdp.env
sudo chmod 600 /etc/kmsrdp/kmsrdp.env   # RDP login credentials only
sudo systemctl enable --now kmsrdp.service
```

## Security

Treat a connected client like a person at the console. Use a strong password,
keep env files mode `0600`, and restrict who can reach the listen port. Report
vulnerabilities via GitHub Security Advisories — see [SECURITY.md](SECURITY.md).

## License

Apache-2.0 or MIT, at your option ([LICENSE-APACHE](LICENSE-APACHE),
[LICENSE-MIT](LICENSE-MIT)).
