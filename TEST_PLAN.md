# TurboNZB Test Plan

## Implementation status (updated)

| Area | Status | Evidence |
|------|--------|----------|
| §2 NNTP client | **Automated** (13 tests) | `tests/nntp_robustness.rs` — partial reads, drops, auth-fail, garbage, timings |
| §4 yEnc | **Automated** (7 tests) | `tests/yenc_property.rs` — roundtrip, CRC detect, fuzz-style hammering |
| §5 NZB parsing | **Automated** (9 tests) | `tests/nzb_robustness.rs` — valid, holes, meta, malformed, large docs |
| §3 engine fallback | **Automated** (3 tests) | `tests/engine_fallback.rs` — server fallback, missing-on-all, isolated jobs |
| §6 queue persistence | **Automated** (2 tests) | `tests/queue_persistence.rs` — state survives reopen; 50-job bulk |
| §6.1 crash-recovery | **Automated** (Unix) | `tests/crash_recovery.rs` — real SIGKILL of a fresh subprocess at a graceful partial state **and** mid-active-write; both resume byte-identically |
| §8 hostile unpack | **Automated** (9 tests) | `tests/unpack_robustness.rs` — path-traversal contained (sevenz rejects `../`), nested `..` contained, corruption, password wrong/correct, unicode, high-ratio, normal 7z + RAR |
| §7 PAR2 verify + repair | **Automated** (5 integration) | `tests/par2_integration.rs` — within-budget full recover, beyond-budget honest shortfall, multi-file, unicode, malformed |
| Infra №1 mock NNTP | **Done** | `tests/common/mod.rs` — scriptable server (bytewise, drop, delay, auth, dot-stuff) |
| §12.5 clippy/fmt gates | **Green** | `cargo clippy --workspace --all-targets` and `cargo fmt --check` both clean |
| Remaining | Manual / future work | See §14–§15 items not yet covered below (crash-kill recovery, cargo-fuzz, CI YAML, GUI, PAR2/parity, unpack, postprocess) |

### How the Fuzz (**P**) targets are covered

Cargo-fuzz needs nightly; instead each `**P**` target in the plan is already
covered by an in-test deterministic fuzzer (seeded Xorshift PRNG) running in
normal `cargo test`:

- §2.11 NNTP response fuzz → `tests/nntp_robustness.rs` (bytewise / drop / garbage)
- §4.2 yEnc roundtrip+fuzz → `tests/yenc_property.rs` (`garbage_input_never_panics`, 500–2000 iterations)
- §5.4 NZB fuzz → `tests/nzb_robustness.rs` (`large_and_deep_documents_parse`)

### Remaining automated work (next tranche)

- §7.5 PAR2 golden interop with `par2cmdline` (external tool, not yet automated)
- §8 ZIP support (not yet a format the app handles) and an explicit decompression-bomb size guard
- §6 queue migration path (legacy DB upgrades)
- §10 Newznab outbut/rate-limit/aggregation edge tests
- §11 GUI backend command round-trips
- §13 criterion benchmarks
- §12.4 logging assertions (no secrets in logs), §12.6 upgrade test
- CI YAML (3-OS matrix) for the gates now green in-repo

---

Goal: reach NZBGet / SABnzbd-grade confidence. Both of those projects are
battle-tested against years of pathological NZBs, flaky news servers, corrupt
uploads, and hostile filesystems. This plan is organized so that every layer
that can lose a user's download is covered by automated tests, and everything
that can't be automated has an explicit manual checklist.

Legend:

- **[A]** automated (unit/integration, `cargo test`)
- **[M]** manual / scripted scenario
- **[P]** property/fuzz target
- Priority: **P0** = data loss / hang / crash risk, **P1** = correctness of a
  core feature, **P2** = polish / edge behavior

---

## 1. Current state (baseline)

Existing coverage to build on:

- `turbonzb-core/src/yenc.rs`, `nzb.rs`, `par2.rs`, `postprocess.rs` — unit tests
- `turbonzb-core/tests/end_to_end.rs`, `bench_download.rs` — live-NNTP end-to-end
- `turbonzb-core/tests/rar_volumes.rs` — multi-volume RAR unpacking
- `turbonzb-index/src/{aggregate,search_parser,caps_parser}.rs`,
  `tests/newznab_integration.rs`
- `turbonzb-gui/src/backend.rs` — backend glue tests

Known gaps: NNTP protocol edge cases, engine failure/resume paths, queue
crash recovery, unpacking hostile inputs, GUI behavior, and any fuzzing.

---

## 2. NNTP client (`core/src/nntp.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 2.1 | P0 | [A] | Mock NNTP server (TCP listener speaking the protocol) covering: greeting (200/201), AUTHINFO USER/PASS success, 481 auth failure, 502 permission denied |
| 2.2 | P0 | [A] | BODY/ARTICLE responses: 223 + article, 430 no such article, response split across arbitrary TCP reads (partial lines, partial dot-stuffed bodies) |
| 2.3 | P0 | [A] | Dot-unstuffing: lines beginning with `..`, body ending exactly on a buffer boundary, `\r\n.\r\n` terminator at a packet split |
| 2.4 | P0 | [A] | Server sends garbage / non-numeric status / truncated response → error, never panic or hang |
| 2.5 | P0 | [A] | Connection drops mid-body → detectable error, reconnect logic kicks in |
| 2.6 | P0 | [A] | Idle timeout: server accepts and never replies → read timeout fires |
| 2.7 | P1 | [A] | Reconnect-and-resume after server closes connection (481/400/forced close), including re-authentication |
| 2.8 | P1 | [A] | Per-server connection limit respected: never open more than N sockets even under heavy article demand |
| 2.9 | P1 | [A] | pipelined/sequential STAT checks for segment existence |
| 2.10 | P1 | [M] | Interop matrix against real providers (one Eweka/Newshosting-class, one cheap reseller): TLS + non-TLS, IPv6 if available |
| 2.11 | P2 | [P] | Fuzz the response parser with arbitrary bytes + random line splits (cargo-fuzz target) |

## 3. Download engine (`core/src/engine.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 3.1 | P0 | [A] | Full download against mock server: all segments fetched, assembled file byte-identical to fixture |
| 3.2 | P0 | [A] | **Kill mid-download** (abort task at random points), restart, verify article-level resume completes without re-downloading finished segments and final file is correct |
| 3.3 | P0 | [A] | Article returns 430 on primary server → falls back to secondary server; missing everywhere → segment marked missing, job fails with readable reason |
| 3.4 | P0 | [A] | Corrupt yEnc (bad CRC) on one server, good copy on the other → retry on fallback recovers the segment |
| 3.5 | P0 | [A] | Engine never writes a partially-decoded segment into the assembled file (missing/corrupt segments are reported, assembly refuses or fails job) |
| 3.6 | P1 | [A] | Multiple servers with different priorities/completion: scheduler prefers primary, distributes fairly, per-server connection caps hold concurrently |
| 3.7 | P1 | [A] | Pause job mid-flight → in-flight work settles, no segment state corruption; resume continues |
| 3.8 | P1 | [A] | Cancel job → partial files cleaned up per config, queue row state consistent |
| 3.9 | P1 | [A] | Same job added twice (duplicate NZB) → dedup/refuse, not two writers to the same files |
| 3.10 | P1 | [A] | Disk-full during write → job fails cleanly with ENOSPC surfaced, no infinite retry loop, no zero-byte "success" |
| 3.11 | P1 | [A] | Unwritable destination dir, path length limits, pre-existing files (overwrite vs rename policy) |
| 3.12 | P1 | [M] | Throughput: saturate a gigabit link with one server ≥ 4 connections; verify connections scale and CPU isn't the bottleneck (use `bench_download.rs` as harness) |
| 3.13 | P2 | [A] | Soak: 200+ small jobs queued back-to-back; no leaked tasks/connections/memory growth |

## 4. yEnc decoding (`core/src/yenc.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 4.1 | P0 | [A] | Golden vectors: known-good singlepart and multipart (=ypart begin/end) fixtures, including escaped chars, CRLF and bare-LF line endings |
| 4.2 | P0 | [P] | Roundtrip fuzz: encode random bytes → decode → identical; mutational fuzz flipping bytes in valid yEnc → error, never panic, never wrong output |
| 4.3 | P1 | [A] | CRC32 mismatch detection: singlepart `pcrc`, multipart per-part and full-file CRC |
| 4.4 | P1 | [A] | Malformed headers: missing `=ybegin`, bogus `size=`, `line=` width that disagrees with actual lines → graceful error |
| 4.5 | P2 | [A] | Non-yEnc article bodies (uuencode/plain) → clear "not yEnc" error |

## 5. NZB parsing (`core/src/nzb.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 5.1 | P0 | [A] | Well-known fixture corpus: NZBs from common indexers, multi-file, multi-segment, out-of-order segments, missing segment numbers, duplicate segment numbers |
| 5.2 | P0 | [A] | Metadata extraction: subjects with tags/password hints (`{{password}}`, `[PRiVATE]`), proper display naming |
| 5.3 | P1 | [A] | Malformed XML, wrong DOCTYPE, empty `<file>`, files with zero segments, absurd segment counts → errors, not panics |
| 5.4 | P1 | [P] | Fuzz XML parser entry point (well-formedness + giant/deep documents, billion-laughs style) — confirm size limits and no OOM |
| 5.5 | P2 | [A] | UTF-8/Latin-1 subjects, entities, CDATA in subject lines |

## 6. Queue & persistence (`core/src/queue.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 6.1 | P0 | [A] | **Crash recovery**: kill -9 the process (or abort the tokio runtime) at defined points — mid-download, mid-unpack, mid-par2 — then restart and verify queue state is consistent and jobs resume or fail sensibly, never silently lost |
| 6.2 | P0 | [A] | Write-ahead state transitions: job never appears "complete" while files are incomplete |
| 6.3 | P1 | [A] | Concurrent DB access: engine + GUI reading/writing queue simultaneously; no SQLITE_BUSY user-visible failures (WAL/busy_timeout verification) |
| 6.4 | P1 | [A] | Schema migration path: open v1 DB with current build, migrate cleanly, data preserved |
| 6.5 | P1 | [A] | Corrupt queue.db (flip bytes, truncate) → app starts, reports error, offers rebuild instead of crashing |
| 6.6 | P1 | [A] | Reorder, priority changes, pause-all/resume-all with 500 queued jobs (performance bound: operations stay <100 ms) |
| 6.7 | P2 | [A] | History retention and cleanup policies |

## 7. PAR2 (`core/src/par2.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 7.1 | P0 | [A] | Verification-only (current scope): good set → PASS; flipped bytes in payload → FAIL with correct damaged-file identification; missing file → FAIL |
| 7.2 | P1 | [A] | Repair (when implemented): delete/corrupt N files within recovery-block budget → full recovery; beyond budget → honest failure listing unrecoverable blocks |
| 7.3 | P1 | [A] | PAR2 sets with unicode filenames, nested paths, case-insensitive collisions on disk |
| 7.4 | P1 | [A] | Malformed/truncated .par2 packets → skipped with log, no panic, verification of the rest continues |
| 7.5 | P2 | [A] | Golden-file interop: verify parity2 packets created by par2cmdline, and (later) that our repairs are accepted by par2cmdline |
| 7.6 | P2 | [P] | Fuzz packet parser |

## 8. Unpacking (`core/src/unpack.rs`, `tests/rar_volumes.rs`)

Existing: multi-volume RAR coverage. Extend with:

| # | Pri | Type | Test |
|---|-----|------|------|
| 8.1 | P0 | [A] | Corrupt archive mid-set (truncated .r03) → failure reported with volume/part identified, no partial extracted tree left in final dir |
| 8.2 | P0 | [A] | **Zip-slip / path traversal**: archives containing `../`, absolute paths, drive letters → entries rejected/sanitized; nothing written outside the job dir |
| 8.3 | P0 | [A] | Decompression bomb guard: tiny archive claiming huge output → abort with clear error (define a sane ratio/size cap) |
| 8.4 | P1 | [A] | Password handling: correct password (incl. from NZB `{{pw}}` subject hint and per-download setting), wrong password → single clear error, no retry storm; password remembered per job only |
| 8.5 | P1 | [A] | RAR5 vs RAR4 vs 7z vs zip fixtures; split 7z (.7z.001…); nested archives policy (extract one level or none — document and test it) |
| 8.6 | P1 | [A] | Filenames with unicode, emoji, very long names, reserved Windows names (`CON`, `aux.txt`), trailing dots/spaces |
| 8.7 | P1 | [A] | Disk full mid-extract → clean failure + partial cleanup |
| 8.8 | P2 | [A] | Mixed case volume extensions (`.RAR`, `.R00`), discontinuities in volume numbering |

## 9. Post-processing pipeline (`core/src/postprocess.rs`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 9.1 | P0 | [A] | Full pipeline ordering: download → par2 verify → (repair) → unpack → cleanup; each stage's failure halts the pipeline and sets the right final status + user-readable reason |
| 9.2 | P1 | [A] | Failure injection at each stage independently (mock each subsystem) |
| 9.3 | P1 | [A] | Jobs with no par2, no archives, archives-but-no-par2, par2-but-no-archives — each combination terminates correctly |
| 9.4 | P1 | [A] | Cleanup policy: what intermediate files (*.par2 volumes, segments) are deleted on success vs kept on failure — document and assert |
| 9.5 | P2 | [A] | Post-process concurrency: N jobs post-processing simultaneously, one per heavy stage at a time, queue UI stays responsive |

## 10. Indexers / Newznab (`turbonzb-index`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 10.1 | P0 | [A] | Fixture-based search: recorded responses from major indexers (recorded, not live) → parsed results correct incl. attributes (size, grabs, password flags) |
| 10.2 | P1 | [A] | Aggregation: overlapping results from 3 indexers deduped, sources merged onto one row; distinct releases not over-merged |
| 10.3 | P1 | [A] | One indexer down/500ing/timing out → search still returns results from the rest, failed indexer shown as failed |
| 10.4 | P1 | [A] | Rate limiting (429), API-key invalid (100-series Newznab error codes) → surfaced to user, not treated as "no results" |
| 10.5 | P1 | [A] | caps parsing drives limits (max results, categories); pagination across `offset` |
| 10.6 | P2 | [P] | Fuzz search/caps parsers |

## 11. GUI (`turbonzb-gui`)

| # | Pri | Type | Test |
|---|-----|------|------|
| 11.1 | P0 | [A] | Backend (`backend.rs`) command/state round-trips: every UI action has a backend test — add, pause, resume, cancel, delete, reorder, settings change |
| 11.2 | P0 | [M] | First-run wizard: fresh config dir → wizard appears → add server with live connection test → add indexer → config persisted and used on next launch |
| 11.3 | P0 | [M] | Long-session stability: leave app running 24h with active queue; check memory, handle count, log growth |
| 11.4 | P1 | [M] | UI reflects engine state accurately through the 3.2/3.8 scenarios (pause/cancel/resume); segment map colors match real segment states |
| 11.5 | P1 | [M] | Window edge cases: tiny window resize, fullscreen, display DPI scaling, monitor disconnect |
| 11.6 | P1 | [M] | Settings: invalid values entered by hand in config.json → load fails gracefully with per-field defaults and a warning, not a crash |
| 11.7 | P2 | [A] | Consider headless GUI smoke test (e.g. winit headless / screenshot diff) for the queue tab rendering |
| 11.8 | P2 | [M] | Keyboard-only navigation and scrollback behavior |

## 12. Cross-cutting scenarios

| # | Pri | Type | Test |
|---|-----|------|------|
| 12.1 | P0 | [M] | **The full-monty chaos run**: scripted scenario — 3 NZBs queued, kill app mid-download, corrupt one segment on server, restart, fail a par2 verify, repair, unpack passworded RAR → final files correct |
| 12.2 | P0 | [A] | Config + queue + downloads on a nearly-full disk → every failure mode is a clean error |
| 12.3 | P1 | [A/M] | Cross-platform CI matrix: Linux, Windows, macOS — `cargo test --workspace` green on all three, plus one manual smoke download per release |
| 12.4 | P1 | [A] | Logging: every user-visible failure produces a log line with job id and reason; no secrets (passwords, API keys) in logs at any level |
| 12.5 | P1 | [A] | Clippy `-D warnings` + `cargo fmt --check` + `cargo deny` in CI, enforced per-PR |
| 12.6 | P2 | [A] | Upgrade test: run N-1 release, create queue, upgrade to N, verify state carries over |

## 13. Performance / benchmarks

| # | Pri | Type | Test |
|---|-----|------|------|
| 13.1 | P1 | [A] | Regression benchmarks (criterion) for: yEnc decode throughput, par2 verify throughput, segment-map rendering at 10k segments |
| 13.2 | P1 | [M] | SABnzbd/NZBGet comparison: same NZB, same server, same connection count — download time within ~10% of NZBGet |
| 13.3 | P2 | [M] | UI frame time with 1000-job queue and a 50k-segment job open |

---

## 14. Infrastructure to build

Priority order for closing gaps:

1. **Mock NNTP server** (test harness listening on localhost, scriptable
   responses, latency/timeout/drop injection). Unlocks §2 and §3 as
   fast, hermetic tests — this is the single highest-leverage item.
2. **Fixture corpus** under `crates/*/tests/data/`: real-world NZBs
   (anonymized), yEnc golden vectors, par2 sets, RAR/7z/zip archives with
   known contents + passwords, recorded Newznab responses.
3. **Crash-recovery harness**: spawn the engine in a subprocess, kill it at
   instrumented checkpoints, assert restart behavior (§6.1).
4. **cargo-fuzz targets**: nzb XML, yEnc decode, par2 packets, Newznab
   parsers (§2.11, 4.2, 5.4, 7.6, 10.6); run short fuzz in CI, long runs nightly.
5. **CI matrix** (GitHub Actions): linux/windows/macos × test/clippy/fmt;
   e2e live-NNTP tests behind a self-hosted or secrets-gated job.
6. **Code coverage** (cargo-llvm-cov or tarpaulin) reported in CI; target
   ≥80% on `turbonzb-core`, track trend rather than chasing 100%.

## 15. Definition of "done" (release gate)

A release candidate must pass:

- [ ] `cargo test --workspace` green on all three OSes
- [ ] §3.2 and §6.1 crash/resume suites green
- [ ] Chaos run §12.1 passes end-to-end
- [ ] Benchmarks within 10% of previous release (no perf regression)
- [ ] Fuzz corpus: no open crashers
- [ ] One manual smoke: fresh install wizard → search → download → par2 →
      unpack on each supported OS
- [ ] No P0/P1 issues open against this plan
