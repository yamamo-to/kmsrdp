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
