# TurboNZB → NZBget parity: Speed, Deobfuscation, Direct Write

A focused execution plan for the three priorities that matter most for
download parity with NZBget: **raw speed**, **deobfuscation that
actually completes damaged releases**, and **writing each file directly
to one output file** instead of scattering thousands of per-segment
disk artifacts and re-assembling them.

This complements the broader `NZBGET_PARITY_PLAN.md`; it is the concrete
implementation slice for these three pillars, grounded in the current
code.

---

## What the code actually does today

- **Write path** (`engine.rs`): every decoded segment is written to
  `filename.parts/segNNNNNN` (one file per segment). After *all*
  segments finish, `assemble_file()` opens the final file, reads every
  `segNNNNNN` back from disk, and writes it concatenated. **Every byte
  is written twice and read once extra.** For a 50 GB release that is
  ~150 GB of I/O and thousands of tiny files / inodes.
- **NNTP** (`nntp.rs`): persistent pooled connections, 256 KB read
  buffer, `TCP_NODELAY`, single `write_all` for the command. **But
  pipelining is implemented and never used** — `run_worker` calls
  `client.body()` (`engine.rs:921`) which does send-then-wait-for-
  response. The split `send_body` / `read_body_response` API exists
  (`nntp.rs:227` / `:235`) but nothing in the hot path calls it. So
  there is a per-article command round-trip gap in the download pipe.
- **yEnc decode** (`yenc.rs`): correct, but allocates a `line_raw` Vec
  per line and copies payload → `out`. Plus `read_dot_body` builds a
  `bytes` Vec, then decode builds a second `out` Vec — **two full
  copies of every ~500 KB article**.
- **Deobfuscation** (`engine.rs`, `par2.rs`, `postprocess.rs`,
  `unpack.rs`): already the strongest part of the codebase — yEnc
  `name=` latching, content-sniffed extension, PAR2 FileDesc name
  restoration (content-matched, tolerates renames), RAR volume
  normalization. Gaps are repair, fast-rename, and RAR-header names.
- **No rate limiter, no ArticleCache, no direct write, no PAR2 repair.**

---

## Pillar 1 — Speed

### 1a. Engage NNTP command pipelining (biggest network win)

NZBget pipelines BODY commands: write N commands, read N responses
back-to-back so the server streams articles with no inter-article gap.
We have the plumbing; we just don't use it.

- Change the worker so each connection maintains a small in-flight
  window (e.g. 2–4): `send_body()` K times, then interleave
  `read_body_response()` with more `send_body()` to keep the window
  full.
- Must respect server input-buffer limits (the `nntp.rs` doc comment
  already warns about this) — keep the window modest and bounded.
- This turns per-article RTT (can be 50–200 ms transatlantic) into
  pure streaming throughput. On a high-latency link this alone can be
  a multiple of current speed without adding more connections.
- **Risk:** a server that doesn't pipeline will deadlock if we fill
  its input buffer. Mitigate with a small window + a
  `CAPABILITIES`/probe fallback to 1-in-flight.

### 1b. Single-copy yEnc decode into a pre-sized buffer

- We know each segment's decoded size from `=ypart begin/end` (or
  `size=`). Pre-size one `Vec<u8>` per segment and decode directly into
  it, dropping the intermediate `bytes` Vec from `read_dot_body` —
  instead, decode streaming from the `BufReader` straight into the
  segment buffer. Removes one full copy + one allocation per article.
- Replace the per-line `line_raw` Vec with a reusable thread-local
  buffer or an in-place scan. The `+42` / `=+64` escape math is
  trivially vectorizable; even a tight `unsafe` slice loop beats the
  current per-byte push.
- This is CPU-side headroom that matters at 1 Gbps+ where decode
  becomes the bottleneck.

### 1c. Reduce work-queue contention

- The shared `Mutex<VecDeque>` (`engine.rs`) is popped by every worker
  under one lock. At 50+ connections this serializes pops. Move to
  `tokio::sync::mpsc` (lock-free) feeding workers, or shard the queue
  per file. Low effort, removes a scaling cliff.

### 1d. DownloadRate (speed limit) — parity, not a gain

- Shared token-bucket (`governor` or a hand-rolled `Semaphore`-of-bytes)
  wrapping the fetch loop. NZBget has it; needed for parity and for the
  UI slider. 0 = unlimited.

### 1e. Bench hardening

- `bench_download.rs` is local-only and won't show the pipelining win
  (no RTT). Add a **latency-injecting** fake server (sleep before each
  response) so the pipelining improvement is measurable and
  regression-protected.

---

## Pillar 2 — Deobfuscation

Current deobfuscation is the strongest part of the codebase. The gaps
that matter for "completing downloads like NZBget":

### 2a. PAR2 repair (P0 — the keystone)

Without repair, a damaged obfuscated release is marked "damaged, manual
repair needed" and unpack is skipped. NZBget's whole value is
auto-repair.

- Parse `RecvSlic` packets (currently only counted, `par2.rs:
  TYPE_RECV_SLICE`) — store recovery slice exponent + data.
- RS erasure decode over GF(2^16) using `reed-solomon-erasure`
  (SIMD-accelerated). Map file slices → recovery blocks, compute
  missing slices, `pwrite` them back into the assembled file at the
  right offset.
- Wire into `post_process`: when verify reports `damaged > 0`, attempt
  repair *before* the `Damaged` early-return.
- This integrates cleanly with **direct write** (Pillar 3): repair just
  `pwrite`s into the one output file at slice offsets.

### 2b. ParRename — fast rename without full verify

Today `rename_to_par2_names` runs only *after* full verify, which
MD5-hashes every file (reads the whole release). NZBget's fast
par-rename uses only FileDesc **16 kB MD5 + length** — seconds, not
minutes.

- Add a rename-only pass: for each FileDesc, find an on-disk file with
  matching length + 16 kB MD5, rename to the PAR2 name. Run it *before*
  full verify. Full verify then only runs on the renamed set (and can
  short-circuit files whose full MD5 was already proven).

### 2c. RarRename — names from RAR headers when no PAR2

When there's no PAR2, the only record of real names is inside the RAR
archive headers. `unrar` exposes entry names.

- Parse the first volume's archive header, extract the real archive
  name / volume scheme, and rename the obfuscated `.NNN.rar` set
  accordingly. Replaces the current `normalize_rar_volumes` guess
  (`release.NNN.rar`) with the actual original name.

### 2d. ParScan extended/full

Scan non-set files (or the whole dir / other downloads) to locate
missing files by content hash. Low frequency but it's the difference
between "missing, fail" and "found elsewhere, success."

---

## Pillar 3 — Direct write to one file (no disk artifacts)

This is the architectural change that touches the most code. Goal:
**one output file per NZB file, segments `pwrite`n at their offset, no
`.parts/` dir, no assemble pass.**

### Design

1. **Pre-allocate** the output file sparse at full size
   (`File::set_len(total_size)` → `ftruncate` on Linux). Optionally
   `fallocate` to reserve space (avoid ENOSPC mid-download) — make it a
   config toggle like NZBget's `RawWrite`.
2. **Per-file writer task** with an `mpsc<WriteJob { offset, bytes }>`
   channel. Workers decode a segment and send `(begin - 1, data)` to the
   file's writer. The writer does positional writes (`File::write_at` —
   std, synchronous but large writes so cheap; or `tokio-uring` on
   Linux for true async). This serializes writes per file (disk is the
   bottleneck anyway) and needs no locking across workers.
3. **No `.parts/` dir, no `assemble_file`.** Missing segments → sparse
   holes (zero-filled on read). CRC mismatches → the bad bytes are
   still written (or skipped to leave a hole); tracked in the DB. The
   segment dot-grid UI keeps working because segment state is still
   persisted per-segment in SQLite.
4. **Resume is preserved and improved.** The partial file *is* the
   resume state on disk. On restart, re-`pwrite` only `Pending`
   segments into the existing file. No re-assembly, no temp files to
   clean up.
5. **Obfuscated files** (real name not known until `=ybegin` / content
   sniff): write to a stable temp single file `.turbonzb-<file_id>
   .partial`; on completion, sniff the head for extension and rename
   once. Still one file, zero parts.

### Concrete code changes (`engine.rs`)

- `run_worker`: replace the `parts_dir` / `tokio::fs::write(segNNNN)`
  block with "send `(offset, decoded.data)` to this file's writer
  channel." Lazily create the writer + pre-allocated file on first
  segment for a file (cached in a per-worker
  `HashMap<file_id, Sender<WriteJob>>`, or a shared
  `Arc<Mutex<HashMap>>` since workers hit any file).
- Delete `assemble_file`'s read-concatenate loop. It becomes: ensure
  file flushed/closed → for obfuscated, sniff + rename → emit
  `FileCompleted`. Missing/CRC counted from the DB segment states, not
  from disk.
- `post_process` / `par2::verify` already read files by content from
  the dir — works unchanged with direct-written files (they're just
  normal files now, possibly with sparse holes for missing segments).

### Risks

- **Windows:** `write_at` works but no sparse preallocation without
  `SetFileValidData` (needs privilege). Acceptable: on Windows,
  `set_len` still works; sparse is a Linux optimization. Document it.
- **Concurrent positional writes to the same fd from one writer task**
  — safe by construction (single writer). If we later parallelize
  writes, `pwrite` is still safe; `seek + write` would not be.
- **Crash safety:** a crash leaves a sparse file with holes. On restart
  the engine re-pwrites pending segments — but it must not treat "file
  exists with size N" as "complete." Already handled: completion is
  decided by segment states in SQLite, not file presence.
- **Free-space check:** with preallocation we should add a `DiskSpace`
  guard (P1 in the broader plan) so we don't `fallocate` past capacity.

---

## Recommended build order

Each step is independently shippable and testable:

1. **Direct write (Pillar 3)** — do this first. It's the biggest I/O
   win, deletes the most code (`.parts`, `assemble_file`), and it's the
   foundation repair writes into. Also immediately gives you "one file,
   no artifacts."
2. **Pipelining (1a)** — biggest network win, mostly contained to
   `nntp.rs` + the worker fetch loop. Add the latency-injecting bench
   first so the win is visible.
3. **Single-copy decode (1b)** — CPU headroom; refactor `yenc.rs` +
   `read_dot_body` to stream-decode into the segment buffer created by
   the direct-write path.
4. **ParRename (2b)** — quick, high-value (turns minutes of verify into
   seconds for big releases).
5. **PAR2 repair (2a)** — the large one; now that direct write exists,
   repair `pwrite`s into the output file.
6. **RarRename (2c), ParScan (2d), DownloadRate (1d), DiskSpace,
   work-queue mpsc (1c)** — polish / parity.

---

## Adjacent things worth flagging

- **ArticleCache** (RAM-resident segments before write) compounds with
  direct write — if you decode into a RAM buffer and flush to the
  pre-allocated file in large aligned runs, you get NZBget's best-case
  I/O pattern. Consider folding into step 1.
- **Per-job pause + retry-failed-articles action** are P0 in the
  broader plan and are prerequisites for "behaves like NZBget
  day-to-day" — cheap to add alongside the direct-write engine
  changes.
- The existing `NZBGET_PARITY_PLAN.md` already covers all of this at a
  roadmap level; this plan is the concrete execution slice for the
  three stated priorities.