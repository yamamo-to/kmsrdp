# Hardware / real-client checklist

Repeatable manual verification for paths CI cannot fully cover (DRM capture,
real GPU, Guacamole, residual tiles, GFX under load). Run after meaningful
display/GFX changes or before a release that touches those paths.

Record results in the table at the bottom (copy a row per run).

## Environment

- Host: Linux with active DRM/KMS (or NvFBC) desktop
- Build: release binary matching the tag under test (`kmsrdp` / `rdp_server`)
- Clients to exercise (as available):
  - `xfreerdp3` (or `xfreerdp`)
  - Guacamole / guacd
  - Optional: mstsc / Windows App (especially before enabling GFX for that audience)

## Scenarios

### 1. Cold connect (Planar, default)

1. Ensure `KMSRDP_GFX` is unset or `0`.
2. Connect with FreeRDP: `/cert:ignore` and valid credentials.
3. Expect: login screen / desktop visible within a few seconds; no disconnect.

### 2. Console scroll (Planar)

1. Open a terminal on the host console (not only inside the RDP session if you are testing capture of the real seat).
2. Run a high-churn scroll, e.g. `find /usr 2>/dev/null | head -n 5000` or similar.
3. Expect: text scrolls smoothly; no frozen bands; session stays up.

### 3. Clear / residual tiles (Planar)

1. Fill the terminal with output, then clear (`Ctrl+L` or `clear`) and run `ls -l`.
2. Expect: no leftover glyphs/tiles from the previous screen.

### 4. GFX scroll (`KMSRDP_GFX=1`)

1. Set `KMSRDP_GFX=1`, restart kmsrdp, reconnect with FreeRDP (GFX/AVC420 enabled if the client offers a flag, e.g. `/gfx:AVC420`).
2. Repeat scenario 2 (heavy scroll).
3. Expect: no `zgfx_decompress failure`, no sudden disconnect. (Regression fixed in v0.1.65+ via MULTIPART segments.)

### 5. GFX residual tiles

1. With GFX still on, repeat scenario 3.
2. Expect: clear/redraw without stale tiles.

### 6. Guacamole (if deployed)

1. Connect via Guacamole to the same host.
2. Spot-check scenarios 1–3 (Planar). Optionally 4–5 if Guacamole negotiates GFX.
3. Expect: no “bottom half never updates” / silent mid-frame death (backpressure fix, 2026-08-24).

### 7. Audio (optional)

1. Play short audio on the host while connected.
2. Expect: RDPSND path does not tear down the session; glitches are noted but not required to pass.

## Result log

| Date | kmsrdp version | Host | Client | Scenarios (1–7) | Pass/Fail | Notes |
|------|----------------|------|--------|-----------------|-----------|-------|
|      |                |      |        |                 |           |       |

## Related automation

- CI FreeRDP Planar short session: `crates/rdpcore-server/tests/e2e_freerdp.rs` (requires `freerdp2-x11` + `xvfb` on Ubuntu 24.04 CI; enforced when `KMSRDP_REQUIRE_FREERDP=1`).
- GFX wire / mock: `rdpcore-rdpegfx` unit tests + `cargo test -p rdpcore-server --features gfx`.
- Interop summary: [ARCHITECTURE.md](ARCHITECTURE.md) § Client Interoperability.
