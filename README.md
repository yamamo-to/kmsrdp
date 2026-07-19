# kmsrdp

DRM/KMS-based RDP remote desktop server for Linux, in pure Rust.

Captures the screen via the kernel DRM/KMS path (no compositor hook) and
injects input through `uinput`, similar to
[ReFrame](https://github.com/AlynxZhou/reframe), but speaks RDP instead of
VNC. The RDP stack lives in `crates/rdpcore-*` (no `ironrdp` dependency).

> [!WARNING]
> Experimental. Authenticated clients get full screen, keyboard/mouse,
> clipboard, audio, and optional drive access. TLS uses a **new self-signed
> certificate every start**; NLA is CredSSP/NTLMv2 only (no Kerberos).
> **Do not expose TCP 3389 to the public Internet** — use a firewall, VPN,
> or SSH tunnel on a trusted network.

## Features

- **Display:** DRM/KMS capture (Linear mmap or GBM/EGL detile); NVIDIA NvFBC
  fallback when no CRTC is bound; dirty 64×64 tiles + RDP 6.0 Planar codec;
  optional `KMSRDP_DISPLAY` for connector selection
- **Input:** `uinput` mouse/keyboard; CJK IME text on X11 (XTest)
- **Clipboard:** text-only CLIPRDR; one process-wide local poller shared by
  all sessions
- **Audio:** output (RDPSND / `parec`) and mic input (MS-RDPEAI); per connection
- **Drives:** RDPDR → FUSE at `$XDG_RUNTIME_DIR/kmsrdp/drives/<DosName>`
  (list/read/write/create/mkdir; shared until the last session leaves)
- **Auth / transport:** TLS + password; optional NLA; priority-aware writes
  so audio is not starved by graphics

## Limitations

- Single monitor; concurrent clients share one desktop and one input device
- Listens on `0.0.0.0:3389` only (no bind-address option)
- Framebuffers: single-plane XRGB8888/ARGB8888 only
- Drive FUSE: no delete/rename/setattr; no printer/CUPS yet
- CJK IME and clipboard need X11 (`DISPLAY` / `XAUTHORITY`)
- Needs `CAP_SYS_ADMIN`, `CAP_DAC_OVERRIDE`, `CAP_NET_BIND_SERVICE` on the binary

**Tested:** Proxmox VM (VirtIO-GPU/QXL) via Guacamole; NVIDIA/Xorg via NvFBC
fallback. See module docs for NvFBC / GBM details.

## Quick start

```bash
cargo build --release --bin rdp_server
sudo setcap cap_sys_admin,cap_dac_override,cap_net_bind_service+ep \
  target/release/rdp_server

KMSRDP_USER=myuser KMSRDP_PASSWORD=mypassword ./target/release/rdp_server
```

Connect with `xfreerdp /v:<host> /cert:ignore /u:myuser /p:mypassword`, or
mstsc (NLA). Optional: `KMSRDP_TLS_HOSTS=host,1.2.3.4` for certificate SANs;
`KMSRDP_DISPLAY=DP-1` or `card1:DP-1` to pick a connector (disables NvFBC
fallback).

Audio needs `parec` / `paplay` / `pactl` on `$PATH`. Root FUSE mounts need
`user_allow_other` in `/etc/fuse.conf`.

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
keep env files mode `0600`, and restrict who can reach port 3389. Report
vulnerabilities via GitHub Security Advisories — see [SECURITY.md](SECURITY.md).

## License

Apache-2.0 or MIT, at your option ([LICENSE-APACHE](LICENSE-APACHE),
[LICENSE-MIT](LICENSE-MIT)).
