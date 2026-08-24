# Quality Assessment (Snapshot)

Date: 2026-08-23  
Subject: `main` at the time (around commit `5400d9f`, just after dirty diffs started ignoring pitch padding and capture/GFX paths were made panic-free)

A score of 10 means “essentially production-complete for that dimension as a remote-desktop server.” These are subjective ratings, not automated metrics or an external audit.

| Dimension | Score | Rationale |
|---|---|---|
| Correctness | 8/10 | PDUs consistently use `Result` and bounds checks; dirty detection now looks only at visible pixels. The stack is still entirely from-scratch, GFX is experimental, and real-client interoperability is not fully proven by tests. |
| Security | 8/10 | Defaults include NLA, `127.0.0.1`, auth rate limiting, path sanitization, and `[REDACTED]` credentials. TLS is self-signed; auth is NTLMv2 only (no Kerberos). Not intended for the public Internet. |
| Performance | 8/10 | Dropped unnecessary memcpy, same-frame re-encodes, and still-frame `Arc` allocations. VAAPI NV12 conversion and multi-head compositing still run on the CPU. |
| Concurrency | 9/10 | Heavy work uses `spawn_blocking`; mutexes recover from poison; input is connection-scoped and released on drop. Remaining deduction: complexity of sharing one desktop across multiple sessions. |
| Test coverage | 7/10 | About 470 unit tests and `proptest` on core codecs. Missing: DRM hardware, HW encoders, and end-to-end tests against real RDP clients. |
| Maintainability | 8/10 | Crate split and `AGENTS.md` rules are clear. Large files such as `server.rs` / `capture.rs` and the cost of learning a from-scratch protocol stack remain. |
| Documentation | 7/10 | README and SECURITY.md cover limits and deployment. rustdoc, architecture diagrams, and a client interoperability matrix are thin. |

**Overall: 8/10** (simple average 7.9, rounded)

High for an experimental Linux RDP server. Closing the gap to 10 means real-client integration tests, Kerberos / proper certificates, and DMA-BUF passthrough into HW encode.

---

## Quality Assessment (Snapshot)

Date: 2026-08-24
Subject: `main` at `ef277fd`, after a review/bugfix pass covering the RDPSND playback path, a bulk-queue backpressure bug that could truncate a screen update and silently kill the session, a GFX-encoder lock-contention fix, three hot-path allocation fixes, and NvFBC frame-length validation — one of these (the bulk-queue bug) was reported against a real GTX 1080 + Guacamole deployment, fixed, and confirmed fixed on that same real hardware.

| Dimension | Score | Rationale |
|---|---|---|
| Correctness | 9/10 | Two real, previously-shipping bugs found and fixed: RDPSND live-slot clobber/RTT poisoning, and a bulk-queue-full condition that silently truncated a screen update mid-frame and tore the session down as a "clean disconnect" (no error logged) — the latter was the root cause of a real bottom-half-not-rendering report, now confirmed fixed on the reporting hardware. NvFBC's driver-returned frame size is now validated instead of trusted. Still: GFX remains experimental, and the new encoder-lock-split/buffer-pool refactors are unit-tested but not yet stress-tested under sustained real load. |
| Security | 8/10 | Unchanged fundamentals (self-signed TLS, NTLMv2 only, not for the public Internet), but this cycle added a five-part focused audit (protocol decoders, transport, connector, server/CredSSP, RDPDR/FUSE path handling) that found no high-confidence vulnerabilities — higher confidence in the existing posture, not a change to it. |
| Performance | 9/10 | Fixed three concrete zero-allocation-hot-path violations flagged by AGENTS.md: per-tile bitmap buffer cloning (now pooled), the OpenH264 I420 conversion buffer (now reused), and the GPU-detile GL readback buffer (now reused). `rdp6::encode`'s internal compression buffer still isn't pooled, and VAAPI/multi-head compositing remain CPU-bound. |
| Concurrency | 9/10 | Found and fixed a real cross-connection blast-radius bug: the GFX encoder's `std::sync::Mutex` was held for the full CPU-bound H.264 encode, so a `FrameAcknowledge` on the connection task could block a shared tokio worker thread for the whole encode duration. Split into its own lock, with a test that deadlocks on the old design. `send_all`'s new 15s backpressure timeout is logically sound and unit-tested but not exercised under many-connection real load yet. |
| Test coverage | 8/10 | proptest coverage extended from the core PDU codecs to all six virtual-channel crates (cliprdr/dvc/rdpdr/rdpeai/rdpegfx/rdpsnd). Added targeted regression tests that fail meaningfully on reversion: bulk-queue backpressure waits instead of dropping, tile buffers are actually recycled (pointer-identity check), the GFX lock split doesn't deadlock, NvFBC size-mismatch detection. Still missing: automated DRM-hardware/HW-encoder/multi-client CI coverage — the GTX 1080 verification this cycle was a manual, one-off real-hardware check, not a repeatable test. |
| Maintainability | 8/10 | Unchanged this cycle — large files (`rdpcore-rdpdr/lib.rs` 1605 lines, `rdpcore-connector/lib.rs` 1599, `session.rs` 939, `session_loop.rs` 898) and thin rustdoc on some `pub` surfaces (e.g. `session_loop.rs` has zero `///` comments on 12 `pub` items) are being addressed as a follow-up. |
| Documentation | 7/10 | `ARCHITECTURE.md` (module map + Mermaid sequence diagram) exists and is reasonably current. Client interoperability matrix and a README/SECURITY.md freshness pass are in progress as a follow-up. |

**Overall: 8/10** (simple average 8.29, rounded)

Same overall number as the previous snapshot, but the composition changed: the prior snapshot reflected "no bugs found yet"; this one reflects "real shipping bugs found, fixed, and one confirmed fixed on the reporting hardware." The remaining gap to 10 is unchanged in kind (real-client integration breadth, Kerberos/proper certificates, HW-encode DMA-BUF passthrough) plus the maintainability/documentation follow-ups now in progress.
