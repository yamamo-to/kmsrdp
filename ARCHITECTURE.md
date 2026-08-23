# KMSRDP アーキテクチャ解説書

本書は、KMSRDP（Linux DRM/KMS scanout capture Pure Rust RDP Server）の内部アーキテクチャ、モジュール構成、画面キャプチャ・エンコードパイプライン、および接続ライフサイクルを解説したドキュメントです。

---

## 1. 全体アーキテクチャ俯瞰

KMSRDP は、コンポジタや X11/Wayland サーバーの協力なしに、Linux カーネルの DRM/KMS からスキャンアウト（画面表示バッファ）を直接キャプチャし、Pure Rust で実装された RDP スタックを通じてクライアントに配信します。

```mermaid
flowchart TB
    subgraph Linux System ["Linux Kernel / Hardware"]
        DRM["/dev/dri/card* (DRM/KMS PRIME fd)"]
        UINPUT["/dev/uinput"]
        PULSE["PulseAudio / PipeWire"]
        FUSE["FUSE mount (/kmsrdp/drives)"]
    end

    subgraph kmsrdp_crate ["kmsrdp (システム統合バイナリ)"]
        CAP["capture::Capturer\n(DRM/KMS / NvFBC fallback)"]
        HUB["display_hub::DisplayHub\n(フレームレート制御・差分検出)"]
        INP["uinput::UinputHandler"]
        AUD["audio::PulseAudioBridge"]
        DRV["rdpdr_fuse::FuseMount"]
    end

    subgraph rdpcore_crates ["rdpcore-* (プロトコル & サーバー)"]
        CONN["rdpcore-connector\n(X.224 / MCS / NLA CredSSP)"]
        SRV["rdpcore-server\n(handshake / session_loop)"]
        GFX["rdpcore-rdpegfx\n(AVC420 H.264 エンコード)"]
        SND["rdpcore-rdpsnd / rdpeai\n(音声入出力)"]
        CLIP["rdpcore-cliprdr\n(クリップボード)"]
        PDU["rdpcore-pdu\n(BER/PER/RDP6/FastPath/NSCodec)"]
        TRANS["rdpcore-transport\n(優先度スケジューラ / WriteQueue)"]
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

## 2. コアモジュール構成と責務

### 2.1 `rdpcore-server`（RDP サーバーコア）
* **`server::handshake`**: Cleartext X.224 接続ネゴシエーション、TLS アップグレード、CredSSP / NTLMv2 認証、ClientInfo 処理、Demand Active PDU 送信。
* **`server::session_loop`**: 定常状態の非同期 `tokio::select!` ループ。Fast-Path 入力・各静的/動的仮想チャネルの PDU ディスパッチおよびリサイズ制御。
* **`server::frame_pump`**: 画面フレームおよび差分タイルのエンコード・送信スケジューリング（SurfaceCommands / Planar RLE / NSCodec / RDPEGFX AVC420）。重いエンコード処理は `spawn_blocking` にオフロード。
* **`server::input_handler`**: Fast-Path キーボード（スキャンコード・日本語キー）、マウス移動/クリック/ホイール、Unicode イベントの解析と変換。
* **`server::metrics`**: フレーム数、圧縮タイル数、送信バイト数などのパフォーマンス統計収集。

### 2.2 `kmsrdp`（Linux システム統合）
* **`capture::dmabuf`**: DRM カードデバイスのオープンと PRIME DMA-BUF の CPU キャッシュ同期（`DMA_BUF_IOCTL_SYNC`）。
* **`capture::display_mode`**: `KMSRDP_DISPLAY` に基づく単一コネクタまたはマルチモニタ合成モードの判定。
* **`capture::drm_discover`**: `/dev/dri/card*` の走査、コネクタ・CRTC・プライマリプレーンの検出、ホットプラグ更新。
* **`capture::pixel_diff`**: メモリ確保前の直前フレームとの差分判定（`take_pixels`）とビットブロック転送（`blit_bgrx`）。
* **`capture::drm_capturer`**: DRM/KMS プライマリプレーンキャプチャループ、GBM/EGL デタイル、マルチヘッド合成。
* **`gpu_detile`**: タイル配置されたフレームバッファ（Intel / AMD 等の modifier 付き）を GBM / EGL を用いて線形 BGRX に変換。
* **`nvfbc`**: CRTC が未バインドの場合や NVIDIA プロプライエタリ環境向けの NvFBC キャプチャフォールバック。

---

## 3. RDP 接続シーケンス（ライフサイクル）

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
    
    Note over Srv,Steady: ハンドシェイク完了 → run_steady_state 遷移
    
    loop 定常状態 (Steady State)
        DRM->>Steady: 最新フレーム差分 (DisplayUpdate::Bitmap)
        Steady->>Client: Fast-Path SurfaceCommands / Planar / NSCodec / GFX
        Client->>Steady: Fast-Path Input (Mouse / Keyboard / Wheel)
        Steady->>Client: Priority::Latency (RDPSND 音声 / Ack)
    end
```

---

## 4. 画面キャプチャ・差分エンコードパイプライン

1. **ゼロアロケーション差分判定 (`take_pixels`)**:
   - DRM または NvFBC から取得したバッファを、メモリ確保を行う前に直前フレームのバッファと `memcmp` / `find_dirty_rects` で比較。
   - 静止画（差分ゼロ）の場合は新しい `Arc<[u8]>` を一切ヒープ確保せず、空バッファと `unchanged = true` を返却して CPU / メモリを節約。
2. **優先度付きパケットスケジューリング (`rdpcore-transport`)**:
   - `Priority::Latency`: 音声 PCM チャンク（RDPSND）、入力応答、制御 PDU。
   - `Priority::Bulk`: 巨大な画像フレーム/差分ビットマップ、クリップボードデータ。
   - 大量の画面更新が発生しても、パケットスケジューラにより音声やキーボード/マウスの遅延・途切れを防止。
