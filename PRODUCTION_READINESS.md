# BackLog — Production Readiness

A hardening pass audited the whole codebase (Rust app + `backlog-core`, the
vanilla-TS frontend, the Python `convertd` sidecar, the model/training tooling,
and the Tauri/Power Automate configuration) and fixed the ship-blocking and
high-severity issues. This document records what changed, how it was verified,
and what a team should still do before a production rollout.

## Verification (no CI required)

Everything below was verified locally — deliberately, since GitHub Actions
minutes aren't available:

| Check | Command | Result |
|---|---|---|
| Trust-core tests | `cargo test -p backlog-core` | 13 pass, ~10s, no sidecars/app build |
| Full workspace tests | `cargo test --workspace` (in `src-tauri/`) | 19 pass (13 core + 6 app: config + duplicate-key) |
| Lints | `cargo clippy --workspace` | clean, 0 warnings |
| Frontend build | `npm run build` | passes (`tsc && vite build`) |
| Python | `python -m py_compile` on changed files | clean |

> The full app build (`cargo test` across the workspace, `npm run tauri build`)
> needs the real sidecar binaries and `icons/icon.ico` present. The trust core
> — the product's actual guarantee — now builds and tests with **none** of that.

## What was fixed

### Ship blockers
- **Frontend never built.** `get_stats().catch(() => ({}))` typed the fallback
  as `{}`, so strict bracket-indexing failed at 4 sites and `tsc` (hence
  `tauri build`) never produced a bundle. Fixed the fallback type.
- **Windows build aborted** without `icons/icon.ico` (tauri-build's Windows
  resource step). Added it. *(It's derived from the 32×32 placeholder `icon.png`
  — replace with a real asset; see below.)*
- **Trust core couldn't be tested on a fresh checkout.** tauri-build aborts the
  whole compile when the `externalBin` sidecars are absent, so `cargo test`
  failed before running a single checker test. Extracted `harvest`+`checker`
  into the dependency-light **`backlog-core`** crate — now testable in seconds
  with no Tauri/sidecar/icon.
- **No committed lockfiles.** Added `Cargo.lock` and `package-lock.json` for
  reproducible on-prem builds.

### Correctness — the four headline guarantees
The "happy path" was already solid (atomic manifests, streaming hash, WAL
ledger, never-deletes, no SQL injection). The **duplicate path was broken** and
violated three guarantees; all three are fixed:
- The duplicate manifest id was `format!("{sha}:{uuid}")`. `:` is invalid on
  NTFS, so the write silently failed on Windows → duplicate copies produced **no
  manifest** ("later copies get (2) names" fully broken). The fresh UUID also
  made replay non-idempotent → **double-indexing**. And duplicate names were
  never persisted → every copy resolved to `(2)` → Flow 2 **409 conflicts**.
- Now: a deterministic, filesystem-safe per-copy key; a durable ledger row per
  physical copy so `dedupe_name` increments `(2)`,`(3)`,…; `dedupe_name`
  excludes the job's own row (fixes self-collision on resume); duplicate write
  errors are logged, not swallowed.

### Security
- **Least privilege:** removed `tauri-plugin-shell` + `shell:allow-execute` and
  `tauri-plugin-opener` + `opener:default` — all dead (sidecars spawn from Rust
  via `std::process`), so they were pure IPC attack surface.
- **Path traversal:** `get_evidence` validates its id (hex/`-`) before the
  `{id}.md` path join.
- **No raw document text at rest:** cached markdown is purged on emit
  (`retain_cache=false` default) with a startup TTL sweep; flagged files keep
  their cache only until review is resolved.
- **PII-safe audit log:** the persisted `events` table gets reason codes, not
  the offending subject/date/model-output/file-path.
- **DoS guard:** type detection reads a bounded 8 KB header instead of the whole
  (possibly adversarial) file.

### Reliability
- **Sidecar can no longer wedge the pipeline.** The `timeout` field was dead and
  the blocking read ran under the shared mutex. Now stdout is drained on a
  reader thread with an enforced per-request deadline (kill + respawn on
  timeout), and a `Drop` guarantees no orphaned sidecar processes.
- **Crash-loop guard:** a durable attempt counter quarantines a poison-pill
  document as `CRASH_LOOP` after 5 restarts.
- **Backpressure:** the watcher bounds concurrent hashing/probing so a
  multi-thousand-file backfill can't spawn thousands of blocking tasks.
- **Config validation** rejects unset/duplicate/nested folders (a nested
  outbox/cache under the recursively-watched processing dir would feed the app's
  own manifests back through the pipeline).
- Pause no longer strands arriving files; quarantine failures are surfaced
  instead of orphaning the file; `update_fields` validates before locking (no
  poisoned-mutex cascade); the watcher spawns via `tauri::async_runtime` (no
  `Handle::current()` panic); `slm_slots` follows `cfg.slm_parallel`.

### Frontend
- Recoverable fatal state instead of a blank window on startup failure; pause no
  longer desyncs on error; list failures render a distinct error state; numeric
  settings clamp to their min/max; non-blocking toast replaces `alert()`.
- **System tray + close-to-hide:** closing the window used to quit the process
  and kill the sidecars mid-batch. It now hides to the tray; quit is explicit.

### Python sidecar & tooling
- Pinned every dependency (`==`) for reproducible builds.
- Fixed the `tail_pages=0` trim no-op (was re-appending the whole document),
  closed pdfium bitmaps (native-memory leak in the long-lived process), explicit
  UTF-8 for text I/O.
- `download_models.py`: POSIX lock keys (cross-OS), completeness check (not just
  "dir exists"), `local_dir_use_symlinks=False` (fixes Windows WinError 1314),
  `urlopen` timeout.
- `BUILD.md`: added the `--collect-all` flags PyInstaller needs for
  torch/transformers/sentence-transformers/pypdfium2.

## What still needs attention

Ordered roughly by priority.

> **Update — reconciliation + build pass since this report.** Items 1–3 below are
> now **done**, plus more. Landed: a real hi-res icon set; the real sidecars built
> locally (`convertd` via PyInstaller on Python 3.11, `llama-server` b10091 staged
> with its DLLs) and a validated NSIS installer; `sidecar/requirements.lock`; the
> **licensing-clean model swap** (Qwen3 + Lingua + RapidOCR, dropping the
> Liquid-licensed LFM2.5 and CC-BY-SA fastText); the **runtime preflight**
> readiness check + UI; the Unicode `harvest()` crash fix; the Power Automate
> **manifest-v2 contract**; and the **slim, torch-free sidecar** (dropped
> torch/transformers/sentence-transformers/gliclass, ~3x smaller Python
> dependency footprint; `classify`/`salience`/`ettin_spans` degrade to
> deterministic `ok=true` fallbacks instead of the gliclass/granite/Ettin
> naming enhancements -- see `docs/DEPENDENCY_COMPATIBILITY.md`). Still open:
> install-and-run validation on a clean machine with the models; encryption at
> rest for the ledger; the async-hygiene (`spawn_blocking`) refinement; an
> in-flight claim; the dev-only Vite advisory; and an auto-updater.

1. **Real app icon.** `icons/icon.{png,ico}` is a 32×32 placeholder. Supply a
   1024×1024 source and run `npm run tauri icon <source.png>` to generate the
   full platform set.
2. **Build the real sidecars.** Release builds need `convertd` and
   `llama-server` in `src-tauri/binaries/` with the target-triple suffix (see
   `sidecar/BUILD.md`). The app-crate compile was exercised locally with
   gitignored 0-byte placeholders; those are not committed.
3. **Resolve/lock Python deps in a clean venv** (`pip-compile --generate-hashes`)
   and commit `models.lock.json` after the first model download. The pinned
   versions in `requirements.txt` are plausible but should be verified to
   co-resolve on the target platform.
4. **Async hygiene under heavy load (residual).** The sidecar/hash/convert calls
   are still invoked synchronously from async code. The new per-request timeout
   bounds any single wedge, but wrapping those blocking calls in
   `tokio::task::spawn_blocking` would keep the async workers free during a large
   backfill. Worth doing, but validate with a real multi-thousand-file load test.
5. **Encryption at rest (privacy product).** The SQLite ledger and manifests
   hold *derived* PII (proposed subjects, filenames, local paths) unencrypted.
   The cache no longer holds raw document bodies, but for a privacy-first product
   consider SQLCipher / an OS-keystore-derived key for the ledger and a retention
   policy for the `events` table. (Manifests intentionally carry names to Power
   Automate — confirm the SharePoint side handles them appropriately.)
6. **In-flight claim.** The same path enqueued twice (startup sweep + a
   filesystem event) can be driven concurrently. Debounce + the content-hash key
   make this rare and the Flow 2 gate is idempotent, but an in-memory in-flight
   set keyed by sha would close it cleanly.
7. **Dev-only npm advisory.** `npm audit` reports the esbuild/vite dev-server
   advisory. It affects `npm run dev` only, **not** the shipped Tauri bundle.
   Remediate with a Vite major bump when convenient (deferred here to avoid
   destabilizing the verified build).
8. **No auto-updater.** Fine for an internal appliance; add a signed Tauri
   updater if you distribute binaries.
9. **`binary()` PATH fallback** runs a sidecar from `PATH` if it isn't found
   next to the app binary — convenient in dev, but consider gating it to debug
   builds so a planted binary can't be picked up in production.
