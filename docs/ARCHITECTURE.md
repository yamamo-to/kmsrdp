# KMSRDP Architecture

This document describes the internal architecture of KMSRDP (a pure-Rust RDP server that captures Linux DRM/KMS scanout): module layout, the capture/encode pipeline, and the connection lifecycle.

---

## 1. High-Level Overview

KMSRDP captures scanout (the displayed framebuffer) directly from the Linux kernel DRM/KMS subsystem, without help from a compositor or an X11/Wayland server, and delivers it to clients through a from-scratch RDP stack.

```mermaid
flowchart TB
    subgraph Linux System ["Linux Kernel / Hardware"]
        DRM["/dev/dri/card* (DRM/KMS PRIME fd)"]
        UINPUT["/dev/uinput"]
        PULSE["PulseAudio / PipeWire"]
        FUSE["FUSE mount (/kmsrdp/drives)"]
    end

    subgraph kmsrdp_crate ["kmsrdp (system-integration binary)"]
        CAP["capture::Capturer\n(DRM/KMS / NvFBC fallback)"]
        HUB["display_hub::DisplayHub\n(frame pacing / dirty detection)"]
        INP["uinput::UinputHandler"]
        AUD["audio::PulseAudioBridge"]
        DRV["rdpdr_fuse::FuseMount"]
    end

    subgraph rdpcore_crates ["rdpcore-* (protocol & server)"]
        CONN["rdpcore-connector\n(X.224 / MCS / NLA CredSSP)"]
        SRV["rdpcore-server\n(handshake / session_loop)"]
        GFX["rdpcore-rdpegfx\n(AVC420 H.264 encode)"]
        SND["rdpcore-rdpsnd / rdpeai\n(audio in/out)"]
        CLIP["rdpcore-cliprdr\n(clipboard)"]
        PDU["rdpcore-pdu\n(BER/PER/RDP6/FastPath/NSCodec)"]
        TRANS["rdpcore-transport\n(priority scheduler / WriteQueue)"]
    end

    DRM --> CAP
    CAP --> HUB
    HUB --> SRV
    SRV --> TRANS
    TRANS --> NET["TCP / TLS 3389 (RDP Client)"]

    NET --> SRV
    SRV --> INP --> UINPUT
    PULSE <--> AUD <--> SND <--> SRV
    NET <--> DRV <--> FUSE
```

---

## 2. Core Modules and Responsibilities

### 2.1 `rdpcore-server` (RDP server core)
* **`server::handshake`**: Cleartext X.224 connection negotiation, TLS upgrade, CredSSP / NTLMv2 authentication, Client Info handling, and Demand Active PDU send.
* **`server::session_loop`**: Steady-state asynchronous `tokio::select!` loop. Dispatches Fast-Path input and static/dynamic virtual-channel PDUs, and handles resize.
* **`server::frame_pump`**: Schedules encoding and sending of frames and dirty tiles (SurfaceCommands / Planar RLE / NSCodec / RDPEGFX AVC420). Heavy encode work is offloaded to `spawn_blocking`.
* **`server::input_handler`**: Parses and translates Fast-Path keyboard (scancodes, Japanese keys), mouse move/click/wheel, and Unicode events.
* **`server::metrics`**: Collects performance stats such as frame count, compressed tile count, and bytes sent.

### 2.2 `kmsrdp` (Linux system integration)
* **`capture::dmabuf`**: Opens DRM card devices and CPU-cache-syncs PRIME DMA-BUF (`DMA_BUF_IOCTL_SYNC`).
* **`capture::display_mode`**: Chooses single-connector vs multi-monitor composite mode from `KMSRDP_DISPLAY`.
* **`capture::drm_discover`**: Scans `/dev/dri/card*`, discovers connectors, CRTCs, and primary planes, and applies hotplug updates.
* **`capture::pixel_diff`**: Dirty detection against the previous frame before allocating (`take_pixels`) and blit (`blit_bgrx`).
* **`capture::drm_capturer`**: DRM/KMS primary-plane capture loop, GBM/EGL detile, and multi-head compositing.
* **`gpu_detile`**: Converts tiled framebuffers (Intel / AMD modifiers and similar) to linear BGRX via GBM / EGL.
* **`nvfbc`**: NvFBC capture fallback for unbound CRTCs and NVIDIA proprietary setups.

---

## 3. RDP Connection Sequence (Lifecycle)

```mermaid
sequenceDiagram
    autonumber
    actor Client as RDP Client (mstsc / FreeRDP)
    participant Srv as rdpcore-server (handshake)
    participant TLS as tokio-rustls
    participant NLA as CredSSP / NTLMv2
    participant Steady as rdpcore-server (session_loop)
    participant DRM as kmsrdp (Capturer)

    Client->>Srv: X.224 Connection Request (Cleartext)
    Srv-->>Client: X.224 Connection Confirm (PROTOCOL_HYBRID / PROTOCOL_SSL)
    Client->>TLS: TLS Handshake (ClientHello)
    TLS-->>Client: TLS ServerHello + Persisted Cert
    
    opt NLA (PROTOCOL_HYBRID)
        Client->>NLA: TSRequest (NTLM Negotiate / Challenge / Authenticate)
        NLA-->>Client: TSRequest (pubKeyAuth)
    end

    Client->>Srv: MCS Connect Initial + GCC Conference
    Srv-->>Client: MCS Connect Response
    Client->>Srv: MCS Attach User + Channel Join Requests
    Srv-->>Client: MCS Confirm
    Client->>Srv: Client Info PDU (Credentials / Timezone)
    Srv-->>Client: Demand Active PDU (Capabilities)
    Client->>Srv: Confirm Active PDU + Synchronize + Control Co-op
    Srv-->>Client: Synchronize + Control Co-op + Font Map
    
    Note over Srv,Steady: Handshake complete → transition to run_steady_state
    
    loop Steady State
        DRM->>Steady: Latest frame dirty rects (DisplayUpdate::Bitmap)
        Steady->>Client: Fast-Path SurfaceCommands / Planar / NSCodec / GFX
        Client->>Steady: Fast-Path Input (Mouse / Keyboard / Wheel)
        Steady->>Client: Priority::Latency (RDPSND audio / Ack)
    end
```

---

## 4. Capture and Dirty-Encode Pipeline

1. **Zero-allocation dirty detection (`take_pixels`)**:
   - Compare the DRM or NvFBC buffer to the previous frame with `memcmp` / `find_dirty_rects` *before* allocating.
   - On a still frame (no dirty pixels), return an empty buffer with `unchanged = true` and allocate no new `Arc<[u8]>`, saving CPU and memory.
2. **Priority packet scheduling (`rdpcore-transport`)**:
   - `Priority::Latency`: audio PCM chunks (RDPSND), input responses, control PDUs.
   - `Priority::Bulk`: large image frames / dirty bitmaps, clipboard data.
   - The scheduler keeps audio and keyboard/mouse from stalling even when screen updates are heavy.

---

## 5. Client Interoperability

kmsrdp negotiates encoding and protocol behavior primarily by **capability** (NSCodec presence, AVC420 in CapsAdvertise), falling back to **client-name sniffing** (`client_needs_compat_workarounds` in `rdpcore-server/src/encode.rs`) only where a client omits the capability it actually needs. "Confirmed" below means observed against a real client (test or hardware); "code-inspection only" means the behavior exists to satisfy a wire quirk documented in comments/history but has no automated check.

| Area | mstsc / Windows App | FreeRDP (xfreerdp) | Guacamole / guacd | macOS "Windows App" |
|---|---|---|---|---|
| RDP6 Planar | Default path. Batches PDUs (Synchronize+Cooperate+Control+FontList) and fragments Confirm Active — handled, code-inspection only | Default path. Confirmed: `tests/e2e_freerdp.rs` runs a real `xfreerdp +auth-only` against the server in CI (NLA handshake only, not bitmap rendering) | Default path (FreeRDP-based backend) | Not used — falls back to NSCodec |
| NSCodec | Not used unless negotiated | Not used unless negotiated | Not used unless negotiated | Forced via name-sniff (`mac`/`darwin`/`iphone`/`ipad` in client name); capability-first if the client itself negotiates NSCodec. README: real-hardware tested |
| GFX / AVC420 | **Off by default** (`KMSRDP_GFX=1` opt-in) — mstsc can disconnect on GFX protocol errors; treat as unverified/risky, not confirmed working | Capability-negotiated (`select_avc420_capability`, version-ranked `CAP_VERSION_8`..`104`) when `KMSRDP_GFX=1`; code-inspection only, no automated test | Not confirmed either way — no test or hardware note found | Not applicable (NSCodec path) |
| RDPSND | v8 advertised so Wave2 is used when the client also negotiates ≥v8; falls back to legacy WaveInfo+Wave otherwise. Live-slot/RTT bugs affecting all clients fixed 2026-08-24 — client-agnostic, not client-specific | same | same | same |
| Known real-world bug (fixed) | — | — | **Confirmed on real GTX 1080 hardware, 2026-08-24**: bulk-queue backpressure during large legacy-bitmap updates silently truncated mid-frame and killed the session (looked like "bottom half never renders"); fixed and re-verified live on the reporting machine the same day | — |
| Overall confirmation level | Code-inspection only (extensive wire-quirk handling in `rdpcore-connector`, no automated e2e) | **Test-confirmed** (NLA only) + informal real use | **Real-hardware confirmed** (this session's live bug fix + re-verification) | README claims real-hardware tested; no automated test |

**Gaps**: no automated e2e coverage exists for actual bitmap/GFX/audio rendering against any client (only NLA auth-only via FreeRDP); mstsc and macOS Windows App interop rest on code comments and undated README claims, not repeatable tests; GFX/AVC420 has no confirmed-working client at all.
