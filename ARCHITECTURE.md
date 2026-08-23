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
