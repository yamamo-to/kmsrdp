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
  clients) or NSCodec SurfaceCommands (macOS Windows App); optional experimental
  **GFX AVC420** via `KMSRDP_GFX=1` (OpenH264; optional VAAPI/NVENC via
  `gfx-vaapi` / `gfx-nvenc`); composite all connected CRTCs by default
  (`KMSRDP_DISPLAY=all` / unset) or one connector (`KMSRDP_DISPLAY=DP-1` /
  `card1:DP-1`); Save Session Info (PLAINNOTIFY) on connect; Monitor Layout
  when two or more CRTCs are composited
- **Input:** `uinput` mouse/keyboard with Japanese 106/109 key mapping
  (Henkan, Muhenkan, Kana, Zenkaku/Hankaku) and media keys; CJK IME text on
  X11 (XTest)
- **Clipboard:** text-only CLIPRDR; one process-wide local poller shared by
  all sessions; configurable mode (`KMSRDP_CLIPBOARD=bidirectional`,
  `host-to-client`, `client-to-host`, `disabled`)
- **Audio:** output (RDPSND) and mic input (RDPEAI) via libpulse; per connection
- **Drives:** RDPDR → FUSE at `$XDG_RUNTIME_DIR/kmsrdp/drives/<DosName>`
  (list/read/write/create/mkdir/unlink/rmdir/rename/setattr size & times;
  shared until the last session leaves; `chmod`/`chown` update the local FUSE
  view only — not the client filesystem)
- **Auth / transport:** TLS + password; NLA (CredSSP/NTLMv2) when the client
  requests it; persisted self-signed cert by default (`StateDirectory` or
  `KMSRDP_TLS_*`); configurable listen address (`KMSRDP_BIND`, default
  `127.0.0.1` / `KMSRDP_PORT`); NLA required by default
  (`KMSRDP_REQUIRE_NLA=0` to allow TLS-only Client Info auth); one
  authenticated session by default (`KMSRDP_MAX_SESSIONS`); password from
  `KMSRDP_PASSWORD`, `KMSRDP_PASSWORD_FILE`, or systemd
  `LoadCredential=kmsrdp.password`; structured logs
  via `tracing` (`KMSRDP_LOG` /
  `KMSRDP_LOG_FORMAT=json`); priority-aware writes so audio is not starved
  by graphics

## Requirements

kmsrdp captures an **already active DRM/KMS scanout** (a bound CRTC with a
framebuffer). It does **not** create a virtual desktop.

- **Needed:** a connected display (physical monitor, HDMI/DP dongle, or a
  virtual GPU head such as VirtIO-GPU/QXL) that the kernel has modeset.
  A text console (fbcon) is enough; a full GUI session is **not** required
  for capture itself.
- **Not enough:** SSH-only / headless with no active connector — startup
  fails with “no usable card/connector/CRTC”.
- **After unplug:** the service usually keeps running, but capture stops
  (clients freeze or go black) until a display is modeset again.
- **Optional for extras:** a logind graphical or `startx` session supplies
  `DISPLAY` / `XDG_RUNTIME_DIR` for clipboard, Pulse audio, drive mounts,
  and X11 CJK IME. Without it, remote view/input of the console still work;
  those extras do not.

## Limitations

- Concurrent clients: by default only one authenticated session is accepted
  (`KMSRDP_MAX_SESSIONS`, default 1). Extra clients are disconnected after
  handshake. When raised, sessions still share one composited desktop and
  one input device
- Not true per-monitor RDP windows — multi-head is one virtual desktop canvas
- Framebuffers: single-plane XRGB8888/ARGB8888 only (tiled modifiers are
  detiled via GBM/EGL when needed)
- Startup fails hard if the first frame cannot be captured (no active CRTC /
  connected display, and NvFBC unavailable); later capture drops are logged
  with hints (rate-limited) instead of exiting
- Drive FUSE: no printer/CUPS yet; `chmod`/`chown` are local FUSE metadata only
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

Connect with `xfreerdp /v:127.0.0.1 /cert:ignore /u:myuser /p:mypassword`, mstsc,
or the macOS Windows App. The server listens on `127.0.0.1:3389` and requires
NLA by default. Optional: `KMSRDP_BIND=0.0.0.0` to listen on all interfaces
(trusted LAN/VPN only); `KMSRDP_PORT=3390`; `KMSRDP_REQUIRE_NLA=0` to allow
clients that cannot do CredSSP; `KMSRDP_MAX_SESSIONS=2` to allow a second
authenticated client (they still share one desktop, one input device, and
one FUSE drive mount; max 32); `KMSRDP_PASSWORD_FILE=/path` (or systemd
`LoadCredential=kmsrdp.password`) so the password is not in the process
environment; `KMSRDP_CLIPBOARD=host-to-client` (or `client-to-host`,
`disabled`) to restrict clipboard sync direction; `KMSRDP_FPS=30` (or
`KMSRDP_FRAME_INTERVAL_MS=33`) for custom capture rates (default 20 fps);
`KMSRDP_TLS_HOSTS=host,1.2.3.4`
for certificate SANs (applied when the cert is first created — delete the
persisted files to regenerate); `KMSRDP_TLS_DIR` / `KMSRDP_TLS_CERT`+`KEY`
to choose where the identity is stored; `KMSRDP_TLS_EPHEMERAL=1` to skip
persistence; `KMSRDP_LOG=debug` / `KMSRDP_LOG_FORMAT=json` for structured
logs; `KMSRDP_DISPLAY=all` (default) to composite every CRTC, or
`DP-1` / `card1:DP-1` for a single connector (disables NvFBC fallback).
Experimental: `KMSRDP_GFX=1` enables MS-RDPEGFX / OpenH264 AVC420 (off by
default — mstsc can disconnect on GFX protocol errors). Optional HW:
`--features gfx-vaapi` / `gfx-nvenc` (NVENC → VAAPI → OpenH264).
Debug: `KMSRDP_LOG=rdpcore_rdpegfx=debug`.

**Redirected drives:** while an RDP client has shared a local drive, it
appears on the Linux host at `$XDG_RUNTIME_DIR/kmsrdp/drives/<DosName>/`
(e.g. `/run/user/1000/kmsrdp/drives/C`). Use `ls`, `cp`, `rm`, and `mv`
there like a normal directory; the mount goes away when the last session
using that drive disconnects.

Audio uses libpulse (PipeWire/PulseAudio) for both playback capture and the virtual
microphone sink. Root-owned FUSE mounts
need `user_allow_other` uncommented in `/etc/fuse.conf` so the logged-in
session user can access redirected drives (kmsrdp sets file ownership via
FUSE attrs, but the mount itself must allow other UIDs). On startup kmsrdp
validates listen port privileges, `KMSRDP_*` env, `/dev/uinput`, and helper
binaries — hard errors refuse to start; missing audio/FUSE tools are warnings
only.

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

## Documentation

- [Architecture & Protocol Flow](ARCHITECTURE.md): DRM capture pipeline, internal crates, and RDP connection sequence diagrams.
- [Agent & Developer Guidelines](AGENTS.md): architecture assumptions, safety rules, and verification workflow.
- [Quality Assessment](QUALITY.md): subjective snapshot scores (not an automated audit).
- [Security Model & Advisories](SECURITY.md): threat model, deployment recommendations, and vulnerability reporting.

## Security

Treat a connected client like a person at the console. Use a strong password,
keep env files mode `0600`, and restrict who can reach the listen port. Report
vulnerabilities via GitHub Security Advisories — see [SECURITY.md](SECURITY.md).

## License

Apache-2.0 or MIT, at your option ([LICENSE-APACHE](LICENSE-APACHE),
[LICENSE-MIT](LICENSE-MIT)).
