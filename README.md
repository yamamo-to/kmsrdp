# kmsrdp

A DRM/KMS-based RDP remote desktop server for Linux, written in pure Rust.

Inspired by [ReFrame](https://github.com/AlynxZhou/reframe)'s architecture -
capturing the screen directly from the kernel's DRM/KMS subsystem (no
compositor cooperation needed, works on the login screen, headless, over
NVIDIA) and injecting input via `uinput` - but speaking RDP instead of VNC.

The RDP protocol stack itself (`crates/rdpcore-*`) is a from-scratch
implementation - TPKT/X.224/MCS/GCC framing, capability negotiation,
fast-path input/output, RDPSND, CLIPRDR, MS-RDPEAI (audio input), MS-RDPDR
(drive/printer redirection), and an RDP 6.0 "Planar" bitmap codec - with no
dependency on `ironrdp-*` or any other RDP protocol library. It's structured
as a Cargo workspace so the protocol crates are usable independently of
kmsrdp's own DRM/uinput glue.

## Features

- Screen capture: DRM/KMS dma-buf export. A Linear XRGB8888/ARGB8888
  framebuffer is decoded with a plain CPU mmap; a tiled (vendor-modifier)
  one of the same formats goes through a GBM/EGL detile pass instead
  (`kmsrdp::gpu_detile`) - both EGL/GLES and GBM are dlopen'd at runtime, so
  a build without a GPU driver stack installed still works for the mmap
  path, it just can't use the GBM/EGL one.
- NVIDIA NvFBC fallback (`kmsrdp::nvfbc`): when DRM/KMS can't find a bound
  CRTC at all (the proprietary NVIDIA + classic Xorg case below), this
  captures straight from the X driver's internal state instead, bypassing
  DRM/KMS entirely. `libnvidia-fbc.so.1` is also dlopen'd at runtime, so
  this is a no-op fallback (not a hard dependency) on non-NVIDIA boxes.
- Mouse/keyboard input via a virtual `uinput` device
- Japanese/CJK (IME-composed Unicode) text input, via an X11-specific
  keymap-remap + XTest trick (X11 sessions only)
- Bidirectional clipboard sync (CLIPRDR <-> local clipboard, text only)
- TLS (self-signed, regenerated per run) + username/password authentication
- Dirty-rect diffing for the display path (64x64 tiles), with lossless
  RDP 6.0 Planar compression on top of each tile
- Audio output redirection (RDPSND <-> PipeWire monitor source via `parec`)
- Microphone/audio input redirection (MS-RDPEAI <-> a virtual PipeWire
  microphone source other local apps can select as their input device)
- Priority-aware write scheduling: a bulk graphics fragment can never
  starve a latency-sensitive audio frame, even mid-burst

## Known limitations

- Only single-plane XRGB8888/ARGB8888 framebuffers are handled (Linear via
  CPU mmap, tiled via GBM/EGL) - multi-plane formats (e.g. YUV) aren't
  supported by either path.
- The proprietary NVIDIA driver has been seen to not bind a CRTC to the
  connector at all (neither the legacy encoder->crtc chain nor the atomic
  `CRTC_ID` property) while running a classic Xorg session - the display is
  on, but the standard DRM/KMS layer has no record of an active CRTC, so
  DRM/KMS capture fails with `no usable card/connector/CRTC found`
  regardless of the GBM/EGL path (there's no framebuffer to hand it in the
  first place). The NvFBC fallback above covers exactly this case. Not yet
  confirmed whether a Wayland session with the same driver needs it too.
- NvFBC's officially supported hardware is GRID/Tesla/Quadro; GeForce needs
  the unofficial "magic private data" unlock every open source NvFBC
  client uses (see `kmsrdp::nvfbc`'s doc comment) - unofficial, but this is
  exactly the mechanism tools like Sunshine rely on, and it's what's
  actually validated below.
- Single monitor only.
- MS-RDPDR (drive/printer redirection) is implemented and live-validated
  against a real client at the protocol level (`crates/rdpcore-rdpdr`), but
  isn't wired into the production server yet - there's no consumer on this
  side to make a redirected drive/printer actually usable from the Linux
  desktop session (a FUSE mount for drives, a CUPS backend for printers).
  That's a planned follow-up.
- Extended-key (arrow keys, etc.) scancode mapping covers only the common
  cases, not the full table.
- Single-process design requires `CAP_SYS_ADMIN` (DRM) and `CAP_DAC_OVERRIDE`
  (`/dev/uinput` is `root:root` 0600) as file capabilities on the binary -
  see the systemd unit below.

### Tested environments

- **Works:** a Proxmox VM with its default virtual display (std VGA /
  VirtIO-GPU / QXL - plain Linear framebuffers, no vendor tiling),
  connected to over RDP through Apache Guacamole.
- **Works (NvFBC fallback):** a physical NVIDIA/Xorg desktop where DRM/KMS
  finds no bound CRTC at all (see the limitation above) - `capture_raw_bgrx`
  falls back to NvFBC, and a real `xfreerdp` client correctly renders the
  live desktop end to end. This is the scenario a GPU-passthrough Proxmox
  VM running the proprietary NVIDIA driver hits, so it's the one that
  actually matters for that deployment target, not just this dev box.
- **GBM/EGL detile path exercised, but not against a live desktop:** on
  that same NVIDIA/Xorg box, `kmsrdp::gpu_detile`'s import/shader/readback
  pipeline round-trips a known color correctly against a real GBM buffer
  allocated directly on the GPU (see `detile_selftest`) - the CRTC-binding
  issue means no live tiled framebuffer has reached this path yet, but a
  driver/session where DRM/KMS does find a CRTC (e.g. Wayland) should hand
  it one.

## Building

```
cargo build --release --bin rdp_server
```

## Running

Requires:

- `CAP_SYS_ADMIN` + `CAP_DAC_OVERRIDE` on the binary:
  `sudo setcap cap_sys_admin,cap_dac_override+ep target/release/rdp_server`
- An active graphical session (X11 or Wayland) with a Linear-framebuffer
  display for DRM capture to have something to capture - see "Tested
  environments" above; clipboard sync and CJK text input additionally
  require an X11 session (`DISPLAY`/`XAUTHORITY` in the environment).
- `parec`/`paplay`/`pactl` (`pulseaudio-utils`) on `$PATH` for audio
  output/input redirection.

```
KMSRDP_USER=myuser KMSRDP_PASSWORD=mypassword ./target/release/rdp_server
```

Connect with any RDP client, e.g. `xfreerdp /v:<host> /sec:tls /cert:ignore
/u:myuser /p:mypassword`.

## Packaging (AlmaLinux / RHEL 9 RPM)

```
make install-build-deps   # one-time, needs sudo
make rpm                  # -> .rpmbuild/RPMS/x86_64/kmsrdp-*.rpm
sudo dnf install .rpmbuild/RPMS/x86_64/kmsrdp-*.rpm
```

The repository is private, so `codeload.github.com` archive URLs 404
without an auth token: `make rpm`/`make srpm` regenerate both the plain
source tarball and the vendored Rust dependencies locally (`make vendor`)
from the current checkout instead of fetching them from GitHub, so the
actual `rpmbuild` step needs no network access (as a mock/COPR build
wouldn't have).

Other targets: `make srpm` (source RPM only), `make lint` (rpmlint the
spec), `make clean`.

## Installing as a service

Two install options, pick one: a `--user` unit tied to a single login
(below), or a root unit that follows whichever session is active
(further down).

### systemd --user service

Installing the RPM places `dist/kmsrdp.service` and
`dist/kmsrdp.env.example` under `/usr/lib/systemd/user/` and
`/usr/share/doc/kmsrdp/`, and runs `setcap` on the binary automatically
(spec's `%post`). Building from source instead, copy those two files into
place yourself and run the `setcap` command from "Running" above. Either
way, then:

```
mkdir -p ~/.config/kmsrdp
cp /usr/share/doc/kmsrdp/kmsrdp.env.example ~/.config/kmsrdp/kmsrdp.env
chmod 600 ~/.config/kmsrdp/kmsrdp.env
$EDITOR ~/.config/kmsrdp/kmsrdp.env  # set KMSRDP_USER / KMSRDP_PASSWORD
systemctl --user enable --now kmsrdp.service
```

Verified end-to-end on a GDM/GNOME (X11) AlmaLinux 9 session: the
`--user` manager there already imports `DISPLAY`/`XAUTHORITY` into its
activation environment, so the unit needs no extra environment-import
glue. Other session managers may need one (e.g. an XDG autostart entry
or session-startup script that runs `systemctl --user import-environment
DISPLAY XAUTHORITY`).

### Root system service

Needs no `setcap`/environment-import glue at all: the built-in logind
D-Bus session watcher finds whichever graphical session is currently
active and follows it across logout/login and user switches.

Installing the RPM places `dist/kmsrdp-system.service` (as
`kmsrdp.service`) and `dist/kmsrdp-system.env.example` under
`/usr/lib/systemd/system/` and `/usr/share/doc/kmsrdp/`. Building from
source instead, copy those two files into place yourself. Either way,
then:

```
mkdir -p /etc/kmsrdp
cp /usr/share/doc/kmsrdp/kmsrdp-system.env.example /etc/kmsrdp/kmsrdp.env
chmod 600 /etc/kmsrdp/kmsrdp.env
$EDITOR /etc/kmsrdp/kmsrdp.env  # set KMSRDP_USER / KMSRDP_PASSWORD
systemctl enable --now kmsrdp.service
```

`KMSRDP_USER`/`KMSRDP_PASSWORD` here are the RDP login credentials
presented to the RDP client - unrelated to which Linux account's screen
gets captured, since that's auto-detected via logind.
