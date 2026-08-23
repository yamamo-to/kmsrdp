# KMSRDP AIエージェント開発・保守指示書

本書は、**KMSRDP** コードベースを保守・開発する AI エージェントおよび開発者が遵守すべきアーキテクチャの前提、エンジニアリング指針、安全性ルール、および開発ワークフローを定めた指示書です。

---

## 1. プロジェクト概要とアーキテクチャ

KMSRDP は、Linux のカーネル DRM/KMS スキャンアウトを（コンポジタの協力なしに）直接キャプチャし、`uinput` 経由で入力を注入する、Pure Rust 製の高性能 RDP リモートデスクトップサーバーです。

### ワークスペース構成

* **`crates/rdpcore-pdu`**: ゼロから実装された RDP プロトコル Codec 群（MS-RDPBCGR、MS-RDP6、BER/PER、GCC、MCS、X.224、Licensing、Fast-Path、Pointer、SurfaceCommands、NSCodec）。
* **`crates/rdpcore-transport`**: パケット優先度スケジューラ（`Priority::Latency` と `Priority::Bulk`）およびノンブロッキングなフレーム化コネクションライター。
* **`crates/rdpcore-connector`**: 事前認証トランスポートネゴシエーション（NLA / TLS / Direct RDP）。
* **`crates/rdpcore-server`**: 定常状態の RDP セッションループ、CredSSP/NTLMv2 認証、認証レートリミット、画面差分タイル検出（`diff.rs`）、および画像/動画エンコードディスパッチ（`encode.rs`）。
* **`crates/rdpcore-rdpegfx`**: MS-RDPEGFX AVC420 H.264 動的仮想チャネル（OpenH264、および任意の VAAPI / NVENC ハードウェアエンコーダ）。
* **`crates/rdpcore-cliprdr`**: MS-RDPECLIP クリップボード仮想チャネル。
* **`crates/rdpcore-rdpdr`**: MS-RDPEFS / MS-RDPDR デバイスリダイレクションチャネル（FUSE ドライブリダイレクション）。
* **`crates/rdpcore-rdpsnd`**: MS-RDPSND 音声出力仮想チャネル（Wave2 PCM）。
* **`crates/rdpcore-rdpeai`**: MS-RDPEAI 音声入力（マイク）動的仮想チャネル。
* **`crates/rdpcore-dvc`**: 動的仮想チャネルマネージャ（MS-RDPEDYC）。
* **`kmsrdp`**: バイナリエントリポイントおよび Linux システム統合（DRM/KMS キャプチャ、GBM/EGL タイル変換、NvFBC フォールバック、PulseAudio/PipeWire ブリッジ、FUSE マウントライフサイクル、uinput 入力ハンドラ、X11 Unicode 入力、systemd-logind セッション監視）。

---

## 2. コアエンジニアリング & 安全性ルール

### 2.1 PDU パースとエンコードの不変条件 (`crates/rdpcore-*`)
* **本番パスでのゼロパニック**: ネットワーク入力やプロトコルパーサー内で `.unwrap()` や `.expect()` を絶対に使用しないこと。必ず `Result<T, DecodeError>` 等のエラー型を返すこと。
* **アロケーション DoS 対策**: 配列や機能セット、リスト等の要素数プレフィックス構造体をパースする際は、Vec を確保する前に **必ず** `ReadCursor::ensure_count::<T>(count)?` や明示的な境界チェックでバッファ残量と照合すること。
* **安全なスライス参照**: 直接スライスインデックス `&buf[start..end]` を避け、`.get(start..end)` を使用するか、事前に長さの不変条件を厳密に検証すること。
* **ASN.1 BER INTEGER 規則**: $0x8000\_0000$ 以上の値は最上位ビットがセットされる。2の補数表現の ASN.1 BER では、正の整数を表すために先頭に `0x00` パディングバイト（計5バイト）を付与してエンコードすること。
* **Fast-Path 入力**: イベント数が 0（`events.len() == 0`）の場合、MS-RDPBCGR 仕様に従いヘッダー長に続いて 1 バイトの `numberEvents` フィールド（`0x00`）を明示的に出力すること。

### 2.2 並行性・非同期処理ルール
* **Tokio ワーカースレッドのブロッキング禁止**:
  * 重い計算処理（Planar RLE、NSCodec YCoCg 圧縮、動画エンコード等）は `tokio::task::spawn_blocking` にオフロードすること。
  * 同期 IPC やシステムコール（X11 `arboard` 呼び出し、PulseAudio の `ensure_virtual_mic_sink` メインループ待機、FUSE アンマウント等）は `spawn_blocking` や専用 OS スレッドにオフロードすること。
* **マルチスレッド環境での `std::env::set_var` / `remove_var` の使用禁止**: マルチスレッド下での環境変数変更は C ライブラリ（`libc::getenv`）とのデータ競合を引き起こすため、`Session` コンテキストや接続設定は明示的に引数で渡すこと。
* **ポイズン耐性を持つ Mutex リカバリ**: `std::sync::Mutex` のロック時は、他スレッドのパニックによる連鎖障害を防ぐため `.unwrap_or_else(|e| e.into_inner())` や明確なリカバリパターンを用いること。

### 2.3 KMS / DRM & GPU キャプチャ (`kmsrdp/src/capture.rs`, `gpu_detile.rs`)
* **DMA-BUF CPU 同期**: PRIME DMA-BUF メモリマッピングを CPU で読み出す際は、キャッシュコヒーレンシを保証するために必ず `DMA_BUF_IOCTL_SYNC`（読み出し前に `DMA_BUF_SYNC_START`、読み出し後に `DMA_BUF_SYNC_END`）で囲むこと。
* **EGL / DRM の破棄順序**: GPU デタイラーの Drop 実装では、EGL コンテキストや GBM デバイスを解放する前に、必ず OpenGL オブジェクト（テクスチャ、フレームバッファ、シェーダープログラム等）を先に削除すること。

### 2.4 FUSE / RDPDR ドライブリダイレクション (`kmsrdp/src/rdpdr_fuse/`)
* **マルチスレッド競合への耐性**: FUSE 操作はマルチスレッド（`n_threads = 4`）で動作する。inode 挿入直後のメタデータ検索が常に成功すると仮定せず、`None` を適切に処理して `Errno::EIO` を返すこと。
* **パストラバーサル防止（サニタイズ）**: クライアントから提供されるすべてのパスおよび `DosName` は、ディレクトリトラバーサル攻撃を防ぐため `rdpdr_path::is_safe_win_component` および `sanitize_dos_name` を通過させること。

### 2.5 入力処理と解放ライフサイクル (`kmsrdp/src/uinput.rs`)
* **キー押しっぱなし（Stuck Key）の防止**: 入力状態は接続単位で管理（`ConnectionScopedInput`）すること。接続切断やパニック発生時は、`ResetInputOnDrop` によってその接続が保持していたすべてのキー・ボタンを確実に解放すること。
* **共有デバイスの参照カウント**: 共有 uinput デバイスのキー押下状態はセッション間で参照カウント管理（`inject_held`）し、あるクライアントのキー解放が同じキーを押している別クライアントの入力を妨害しないようにすること。

### 2.6 品質・メモリ効率・信頼性の原則
* **明示的で構造化されたエラー型**:
  * コアクレート（`rdpcore-*`）での `anyhow::Result` や文字列エラーの乱用を避け、`thiserror` によるドメイン固有の enum エラー型を定義してプログラムで識別可能にすること。
  * `Result<T, ()>` のような情報が欠落するエラーは返さないこと。
* **ホットパスのゼロアロケーション（メモリ再利用）**:
  * 毎フレーム・毎タイルで呼ばれる高頻度ホットパス（`diff.rs`, `encode.rs` 等）では、繰り返しのヒープ確保を避けること。
  * スタック配置のビットセット（`SmallBitSet` 等）や固定長配列、事前確保したスクラッチバッファの再利用（`buffer.clear()`）を徹底すること。
* **プロパティベーステストと不変性検証**:
  * コアプロトコルデコーダ（BER, PER, RDP6, FastPath, MCS 等）は、任意の破損バイト列が入力されても**決してパニックしないこと**、および正常なエンコード/デコードが厳密に可逆（ラウンドトリップ）であることを `proptest!` テストスイートで検証すること。
* **機密情報・クレデンシャルの保護**:
  * パスワード、NTLM ハッシュ、秘密鍵を保持する型は、`fmt::Debug` 実装でマスク（`[REDACTED]`）し、ログ出力に平文を書き込まないこと。
* **構造化ログの徹底**:
  * 本番コードで生の `println!` や `eprintln!` を使用しないこと。常に `tracing`（`error!`, `warn!`, `info!`, `debug!`, `trace!`）を構造化キーバリュー形式で使用すること。

---

## 3. 開発および検証ワークフロー

### ビルドとテストの実行
コード変更を提案またはコミットする前に、以下のコマンドがすべて警告・エラーなしで成功することを確認すること:

```bash
# 1. ワークスペース全体の全ユニット・統合テストを実行
cargo test --workspace

# 2. 厳格な Clippy リンターチェックを実行
cargo clippy --workspace --all-targets

# 3. リリースビルドの整合性を確認
cargo build --release --bin rdp_server
```

### バージョン更新手順（ロックステップ同期の必須条件）
バージョンを更新する際（例: `0.1.48` $\rightarrow$ `0.1.49`）、以下の 4 つのファイルを専用コミットで**必ず完全に同期して更新**すること:

1. **`kmsrdp/Cargo.toml`**: `version = "x.y.z"`
2. **`Cargo.lock`**: `cargo check` で同期
3. **`debian/changelog`**: `kmsrdp (x.y.z) unstable; urgency=medium` のエントリをタイムスタンプ付きで追加
4. **`kmsrdp.spec`**: `Version: x.y.z` を更新し、`%changelog` にエントリを追加

コミットメッセージ規約: `Release vx.y.z: keep Cargo.toml, spec, and debian/changelog in lockstep.`

