# Changelog

Notable changes to BackLog. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow the
single number that `package.json`, `src-tauri/tauri.conf.json` and
`src-tauri/Cargo.toml` must agree on (CI enforces this — see
`.github/scripts/check-versions.mjs`).

`RELEASING.md`'s "Cutting a release" procedure references this file: step 1 adds
the section for the version being cut, and the release notes and the `notes`
field of `latest.json` should quote it.

> **Provenance.** The 0.2.0 section was reconstructed from the working tree and
> the superseded `PRODUCTION_READINESS.md` rather than from a per-commit log,
> because the pre-0.2.0 history was squashed. Treat it as an accurate summary
> of *what the code does now*, not as a commit-by-commit record.

## [0.4.1] — the two things 0.4.0 measured and left broken

### Undated documents are named again

**Fixed — the mtime fallback fired only when the model volunteered `"none"`.**
`README.md` promises that undated documents fall back to the file modified date.
The branch existed and was tested, but reaching it required the model to decline;
on tax pages dense with years a 0.6B/1.7B model proposes a plausible date
instead, `DateNotInEvidence` correctly refuses it, the ladder re-asks, and the
document quarantines as `SLM_FAIL` — having never reached the fallback that
exists for precisely that case. Measured before: **0 of 3** genuinely undated
fixtures named.

`check_with` now converts a would-be `DateNotInEvidence` into the fallback when
the document itself carried no date. Three things made that safe rather than a
loophole:

- **It sits after the per-date evidence check, not before it.** An earlier
  version gated ahead of the tripwire and discarded dates that metadata
  genuinely supported; five existing tests caught it. That is the difference
  between "this date is unsupported" and "this document has no date."
- **It does not require `file_metadata_dates` to be empty.** That reads safer and
  is a no-op: `pipeline.rs` always extends that list with the file's own mtime
  and ctime, so it is never empty for a real file. Gating on it left the fallback
  as unreachable as before — measured at 6 of 18, and those six only because the
  model happened to guess the mtime. It is also circular: a filesystem timestamp
  cannot be the evidence that forbids falling back to the filesystem timestamp.
- **The central promise is untouched.** This path does not ship the model's date.
  It discards it and substitutes one with real provenance, recording both
  `DATE_FROM_FILE_MTIME` and `DATE_PROPOSAL_DISCARDED:<what was proposed>` — so
  the two meanings of `date_source: "metadata"` stay distinguishable in the index,
  and how often the model fabricates dates stays measurable. Where the document
  does contain dates, a mismatched proposal is still a hard rejection;
  `rejects_hallucinated_date` is unchanged and passing.

**Fixed — `harvest.dates` did not include every date the model was shown.**
`harvest::harvest` scans the first 6,000 and last 2,500 characters, but
`filter.rs` shows the model salient sentences drawn from the *whole* document and
Ettin spans from the first 8,000. A date outside the harvest window could
therefore appear in the bundle while the checker had no record of it. That was
already a latent way to reject a correct, evidenced answer; it became
load-bearing the moment an empty harvest started licensing the fallback, because
a document whose only date sat at character 7,000 would have looked dateless.
`build_evidence` now folds those lanes back into the harvest.

### Conversion is no longer serialized app-wide

**Fixed — `Sidecar` held one convertd process behind one mutex for an entire
request/response round trip**, so every conversion, OCR, probe and langid call in
the app queued behind every other one, and `Config::convert_workers` sized a
semaphore that bought queue depth and no parallelism at all. convertd's main loop
is `while True: readline()`, strictly one request per process, so the fix is more
processes rather than more requests down one pipe.

`Sidecar` now runs a pool. A condvar free-list rather than one mutex per slot: an
intermediate version scanned slots and then blocked on a rotating one, which let
a caller sit behind a long OCR while a different worker went idle — the fairness
loss it was meant to remove. Workers are handed out through an RAII `Checkout`,
so the several ways `call` can leave — success, four failures, or a panic in
serde or a caller — all return or retire the worker; without it any missed path
would have cost the pool a worker permanently and a long backfill would grind
down with nothing in the log to explain it.

Measured, warm pool, conversion stage alone: 13 documents in 3.2 s on one worker
and 2.4 s on four (1.33x). Not 4x because these fixtures are small text-layer
PDFs where the JSON round trip dominates; the gain grows with per-document work,
which is what scanned pages running escalating OCR passes are.

**`Config::convert_workers` is now capped by installed RAM** as well as by cores.
Before the pool the value cost nothing in memory however large it was, since one
process served everything. Now each worker is its own ~195 MB Python process, so
six of them is ~1.2 GB and does not fit on 8 GB beside Windows, the two model
servers and the app: <=9 GiB caps at 2, <=17 GiB at 4, above that 6.

### Honest about what this did not fix

Pooling conversion did not shorten the end-to-end batch, and `docs/SIZING.md`
says so with the numbers. With `slm_parallel: 1` the naming lane sets the wall
clock, so converted documents queue behind a single naming slot; a 12-file batch
measured 34.3 s/file before and 40.25 s/file after with four workers, the
difference being CPU the extra workers took from llama-server. Conversion
parallelism pays off only where `slm_parallel` can also rise, which is a function
of RAM. The benchmark is `#[ignore]`d and measures the conversion stage in
isolation for exactly that reason.

The remaining failures on undated fixtures are subject and description
rejections, not date ones — a naming-quality limit of a small model on sparse
"draft working notes" pages rather than an unreachable rule.

## [0.4.0] — sized for the machine it actually runs on

0.3.0 proved the release procedure worked. This release is what happened when
the product was pointed at its real workload — a thousand tax PDFs and Word
documents on an 8 GB laptop with no GPU — and measured instead of assumed. New
in `docs/SIZING.md`: every number below, how it was obtained, and how to
reproduce it.

### The defaults could not run on 8 GB

**Fixed — `slm_parallel` defaulted to 4 on every machine, which needs ~6 GB for
the language models alone.** `slm.rs` derives `--ctx-size` as
`4096 * slm_parallel` and llama.cpp preallocates the entire KV cache at startup.
Qwen3's attention shape (28 layers, 8 KV heads, head_dim 128, F16) costs
112 KiB/token, so **each parallel slot is 448 MiB** — and the weights are the
cheap half. Measured, both model servers resident:

| `slm_parallel` | Working set | Private commit |
|---|---|---|
| 4 (old default) | 6,078 MB | 3,904 MB |
| 1 (new 8 GB default) | 3,385 MB | 1,207 MB |

Both tiers *are* resident on any real batch: `SlmLane` keeps `primary` and
`escalation` in separate slots, and the 1.7B server stays up for the rest of the
run once any document reaches a third naming attempt. So 6 GB was the
steady state, before Windows, the app and convertd. On 8 GB that is not a slow
run, it is a thrashing one.

`default_slm_parallel()` now reads installed RAM (`GlobalMemoryStatusEx`,
declared inline rather than adding a system-info dependency): <=9 GiB gives 1,
<=17 GiB gives 2, above that 4, and unknown gives 2 rather than gambling on the
smaller machine. Lowering it costs no naming quality — per-slot context is 4,096
tokens either way, since the total is `4096 * n` shared across `n` slots.

**Fixed — the persisted config kept the unsafe value across upgrades.**
`backlog.config.json` outlives the installer, so an 8 GB machine that had ever
run an earlier build would keep `slm_parallel: 4` forever, having never chosen
it. `Config::load` now clamps to what RAM supports, one-directionally: a value
at or below the ceiling is left exactly as configured, because someone lowering
it knows something this does not. Overcommitment is corrected and logged, never
silently.

**Fixed — the naming HTTP timeout and the wall-clock budget disagreed, and the
tighter one silently won.** `slm.rs` hardcoded a 60-second client timeout while
`pipeline.rs`'s `wall_clock_cap` budgets `per_file_wall_clock_secs` (90) for the
same request. On a workstation naming takes seconds and this never surfaced; on
the CPU-only laptops this ships to it turns a slow-but-succeeding document into
`SLM_FAIL:no valid output after escalation` — blaming the model for a deadline
the HTTP client imposed. Now 120s, with the coupling to the config value stated
at the constant.

### One download, good to go

**Added — the installer carries the primary model.** A fresh machine can name
its first document without the 2.4 GB in-app fetch. On first launch the bundled
GGUF is *relocated* into the app-data models folder rather than pointed at in
place: per-user installs share a volume with app-data, so the move is instant
and free, and keeping one canonical models dir is what stops a later "Download
models" from writing back into the install tree and being orphaned by the next
upgrade.

Both Q8_0 weights together are 2.4 GB and GitHub caps a release asset at 2 GiB,
so the 1.7B ships as a separate optional asset. That is a real quality
trade-off, not a packaging detail — see the table below. The official Qwen
repositories publish only Q8_0, so a smaller quantisation would mean
third-party weights and would break the provenance `NOTICE.md` documents.

**Added — the escalation tier degrades instead of failing when the 1.7B is
absent.** `Config::normalize` points both tiers at the primary when the
escalation GGUF is missing, and `SlmLane::ensure_up` then reuses the running
server instead of standing a second one up over identical weights. Without this,
a missing optional model was not a degraded mode but a cliff: `spawn_server`
refuses a GGUF that is not a file, so every third naming attempt failed outright.

### Measured behaviour

`pipeline.rs`'s `e2e_real_batch` is a new `#[ignore]`d load harness that drives
the real sidecars and real weights against real folders, parameterised by
environment variables. It is not one of the five gates and never runs in
`cargo test`. Everything else in the suite exercises the orchestrator against
stubs, so nothing could previously answer what a batch costs.

Measured on a mixed synthetic tax corpus, `slm_parallel: 1`:

| Tiers | Per file | Named `ok` |
|---|---|---|
| 0.6B only | 23.9 s | 2/12 (17%) |
| 0.6B + 1.7B | 34.3 s | 7/12 (58%) |

Extrapolated to 1,000 files: **6.6–9.5 hours**. The bottleneck is not the naming
lane — `Sidecar::call` holds one mutex for a whole request/response round trip,
so conversion is one-at-a-time app-wide and `convert_workers` buys queue depth
rather than parallelism. That is recorded in `docs/SIZING.md` as a known
characteristic, not fixed here.

Roughly a third to a half of a real tax batch lands in Needs Review. Much of
that is correct — an ambiguous `04/05/2023` must go to a human, and `checker.rs`
refuses any date it cannot prove against the document text. `docs/SIZING.md`
separates the genuine misses from the designed refusals rather than reporting a
single success rate.

## [0.3.0] — the release procedure, actually executed end to end

Everything below the "Build and release" heading was found by running
`RELEASING.md` from a bare clone on a clean Windows 11 box rather than by
reading it. Four of the five defects were in the build and packaging path, not
in the app: the shipped Rust, TypeScript and Python code needed no correction,
and every gate that could be run passed on the first honest attempt once the
scripts themselves would run.

### Build and release

**Fixed — `sidecar/requirements.lock` could not be installed on Windows at
all.** It pinned `magika==0.6.3` alongside `onnxruntime==1.28.0`, but magika
0.6.3's own metadata caps `onnxruntime<=1.20.1` on `win32`. The lock had been
resolved on Linux, where that marker does not apply, and committed as "the
reproducible lock" for a product that only ships on Windows. Any resolving
installer rejects it outright. Repinned to `magika==0.6.2`, resolved on
Windows/Python 3.11, which is the only difference between the committed lock
and a fresh Windows resolution.

**Fixed — `scripts/build-sidecar.ps1` shipped a sidecar with no document
parsers in it when the dependency install failed.** `$ErrorActionPreference =
"Stop"` does not apply to native commands, so the `uv pip install` failure
above was ignored: the run continued with only PyInstaller installed, logged
twelve `--collect-all ... is not a package` warnings, and produced a
`convertd.exe` containing none of MarkItDown, RapidOCR, ONNX Runtime or
pdfminer. Every install step now checks `$LASTEXITCODE`, matching what the
script already did for PyInstaller itself. The fixture smoke test did catch
this build — but a build must not rely on a later gate to notice its
dependencies were never installed.

**Fixed — the sidecar smoke test blamed the binary for its own encoding.**
PowerShell encodes a native command's stdin with `[Console]::InputEncoding`.
Where that is UTF-8 *with* a preamble, a 3-byte BOM was glued to the first
request, convertd correctly answered `JSONDecodeError: Unexpected UTF-8 BOM`
with `"id": null`, and the gate reported `no response for request 1` — pointing
at the sidecar rather than at the harness. The encoding is now pinned for the
duration of the test and restored afterwards. The shipped Rust client was never
affected: `sidecar.rs` writes `serde_json::to_string` bytes, which carry no BOM.

**Fixed — `scripts/verify-binaries.ps1` could not run on Windows PowerShell.**
`$BinDir` defaulted to `Join-Path $PSScriptRoot ...` inside the `param()`
block; 5.1 binds parameters before `$PSScriptRoot` is populated, so the gate
died on its own first line with `Cannot bind argument to parameter 'Path'`.
Resolved in the body instead, `-BinDir` still overridable. Both this and the
BOM defect were invisible because every script is documented as `pwsh`, and
pwsh 7 happens to paper over both.

**Documented — `npm run tauri build` hangs after bundling, waiting for a
password this key does not have.** The CLI prints `Decrypting updater signing
key, expect a prompt for password` and blocks on stdin even for an
empty-password key, so in any shell without a console the build stops with the
installer written and no `.sig` next to it. `$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""`
is not a workaround — PowerShell deletes a variable assigned the empty string.
`RELEASING.md` now says so and gives the separate `signer sign` step, which is
how 0.3.0 was signed.

**Fixed — two `RELEASING.md` commands that cannot work as written.** The
signing-key command uses `-p ""`, and PowerShell drops an empty-string argument
before it reaches the executable, so the CLI exits 2 demanding the value that
was just supplied; it is now marked as the one command in that document to run
from a POSIX shell. Build step 4 also told you to pin `llama-server.exe` to the
SHA-256 recorded in Build step 2, which is the hash of the *zip* and can never
match the extracted binary.

### Gates that had never run on Windows

`scripts/ci-local.sh` runs the five jobs on Linux, and
`.github/workflows/ci.yml` has never been assigned a runner
(`docs/KNOWN_ISSUES.md` item 11). Two of those jobs turn out to have been
failing the whole time on the only platform this product ships on. Both are now
fixed and both gates pass on Windows.

**Fixed — `cargo test --workspace` could not start on Windows at all.** The test
harness binary died with `STATUS_ENTRYPOINT_NOT_FOUND` (0xC0000139) before
reaching `main`, which cargo surfaces as `test exited abnormally` with no test
having run. Cause: the harness links tao/wry and so inherits their static
imports of `TaskDialogIndirect`, `RemoveWindowSubclass` and `DefSubclassProc`
— ComCtl32 **v6** exports. `System32\comctl32.dll` is v5.82 and exports only
`SetWindowSubclass`; v6 is reachable only via the side-by-side
`Microsoft.Windows.Common-Controls` assembly, which needs an application
manifest. `tauri_build::build()` embeds one through `rustc-link-arg-bins` —
bins only — so the harness had no `.rsrc` section whatsoever. `build.rs` now
declares that dependency with `cargo:rustc-link-arg`. The scoped
`rustc-link-arg-tests` would have been the narrower tool but covers only
integration tests under `tests/`, which this crate has none of; cargo rejects
it with "does not have a test target", and the harness at issue is built from
the *lib* target, which has no scoped instruction of its own. This never
affected the shipped `BackLog.exe`, which is a bin and always had its manifest;
and `cargo test -p backlog-core` passed throughout because the trust core links
none of this, which is precisely the property it was separated out for.

**Fixed — the log scrubber could be defeated by a path arriving in fragments,
which on Windows was every path it was meant to redact.** `SharedSink::write`
scrubbed each `write` call as it came, justified by a comment asserting that
"env_logger formats a whole record and hands it over in one call, so scrubbing
here … cannot be defeated by a sensitive path straddling two writes". That
holds for env_logger and for nothing else. `fmt::write` calls back once per
format fragment, and `std`'s `Debug for OsStr` escapes its input with
`f.write_char(c)` — one character per call — so a `{path:?}` argument reaches
the sink as about a hundred single-byte writes, and no root can be matched
inside a one-byte haystack. `SharedSink` now reassembles complete lines before
scrubbing, so the guarantee holds whatever the caller's write granularity is
instead of resting on an undocumented detail of one dependency's
`Target::Pipe`. The same buffering fixes a second defect in the same line:
`String::from_utf8_lossy` over one-byte slices turned every byte of a
multi-byte character into U+FFFD, so non-ASCII filenames were being mangled on
the way into the log.

The two `logging` tests that assert this — which is to say, the tests that
assert the claim `docs/PRIVACY.md` makes about the log file — were correct and
failing; nothing was wrong with them. A regression test now pins the
fragmentation itself, since it originates in `std` and would not survive being
rediscovered by accident.

Reachability, stated plainly: in a shipped build every line goes through
env_logger, which does hand over whole records, so no released version is known
to have written a document path to disk. The defect was that the product's
central privacy promise held by coincidence rather than by construction, and
any second writer into that sink would have silently turned the log into the
plaintext index of HR filenames the encrypted ledger exists to prevent.

**Fixed — `cargo clippy --workspace --all-targets -- -D warnings` failed on
Windows.** `pipeline.rs`'s `tiny_cap` and `Harness::dir` are reachable only
from `#[cfg(unix)]` tests (there are five of those and no `cfg(windows)`
counterpart), so on Windows both are dead code and `-D warnings` rejects the
build. `tiny_cap` is now `#[cfg(unix)]` to match its only callers. `Harness::dir`
is `#[allow(dead_code)]` instead, because it is not really unused: it is a
`TempDir` whose drop deletes the very tree the pipeline is pointed at, so the
field is load-bearing on every platform even where nothing reads it.

**Changed — the updater signing key was rotated.** The keypair that signed
0.1.0 and 0.2.0 was not on the build machine and is not recoverable, so 0.3.0
is signed by a new one and `plugins.updater.pubkey` was replaced to match.
Installed 0.1.0 and 0.2.0 copies verify against the old pubkey baked into them
and will therefore reject 0.3.0 silently, by design; they need one manual
install. From 0.3.0 forward the chain is intact. See `RELEASING.md`.

### Added
- `.github/workflows/ci.yml`: five Linux jobs (trust core, workspace, frontend
  + UI harness, Python 3.11, release version agreement). The project previously
  had no CI at all, and `cargo fmt --all -- --check` — an item
  `docs/RELEASE_CHECKLIST.md` calls mandatory — had never been executed.
- `rust-toolchain.toml` pinning the channel with `rustfmt` and `clippy`.
- `LICENSE` (proprietary, source-available) and `NOTICE.md` enumerating every
  redistributed component, so the redistribution gate in
  `docs/DEPENDENCY_COMPATIBILITY.md` can be closed.
- `docs/USER_GUIDE.md`, `docs/TROUBLESHOOTING.md` and `docs/PRIVACY.md` — the
  first documents in the repo written for the office worker who runs this
  appliance rather than for a builder or auditor.
- `docs/KNOWN_ISSUES.md` and `docs/DECISIONS.md`, replacing
  `PRODUCTION_READINESS.md`.
- `scripts/verify-binaries.ps1`: refuses to package a release whose sidecars
  are dev stubs, truncated builds, or not valid PE images.
- `training/README.md` and `training/requirements.txt`.
- `bundle.windows.webviewInstallMode: offlineInstaller` and
  `bundle.windows.nsis.installMode: currentUser` in `tauri.conf.json`.

### Changed
- `scripts/dev-stubs.{sh,ps1}` write a `BACKLOG-DEV-STUB-DO-NOT-SHIP` marker
  instead of zero bytes, so a stub is provably a stub to
  `scripts/verify-binaries.ps1`, which is the gate between a dev checkout and a
  bundle. The *runtime* readiness check has not caught up: `preflight.rs`'s
  `binary_exists` still only requires a non-empty file, so it certifies a marked
  29-byte stub as installed — see `docs/KNOWN_ISSUES.md` item 9.
- `power-automate/manifest.schema.json`,
  `power-automate/manifest.parse-json.schema.json` and the example fixtures
  move to manifest **v3** (`dismissed` status; `model_versions` required
  non-empty on `ok`), matching `src-tauri/src/manifest.rs`.
- `power-automate/FLOW1-intake.md` rewritten: Flow 1 now delivers each file
  under its plain original name inside a per-delivery subfolder it composes
  itself. The previously documented `__incoming_<flow-id>-<item-id>__` envelope
  described app behavior that does not exist.

### Fixed
- README behavior guarantees, prerequisites and setup steps that did not match
  the code (`--vl`, "Python 3.11+", the llama-server version rationale,
  "indexed once", "never deleted").

## [0.2.0] — backend hardening + licensing-clean model swap

### Added
- **Encryption at rest for the ledger.** `ledger.db` is whole-file encrypted
  with SQLCipher (`rusqlite`'s `bundled-sqlcipher-vendored-openssl`). The
  256-bit key is generated on first open, DPAPI-protected, and stored at
  `<data_dir>/ledger.key` — never written in plaintext (`src-tauri/src/dbkey.rs`).
- **In-app model downloader** (`src-tauri/src/model_download.rs` +
  `download_models` command): resumable, cancellable, SHA-256-verified against
  `models.lock.json`, so a non-technical user never opens a terminal to install
  the two Qwen3 GGUFs.
- **Runtime preflight** (`src-tauri/src/preflight.rs`): every check carries a
  plain-language `message`, a technical `detail`, and — where the app can fix
  it — an `action` the UI renders as a button.
- **Signed auto-updater**: `tauri-plugin-updater` with a minisign pubkey in
  `tauri.conf.json`, checked once at startup by `src/main.ts`.
- **System tray + close-to-hide**: closing the window no longer quits the
  process and kills the sidecars mid-batch.
- `JobState::Dismissed` and the `dismiss` command: a terminal human decision,
  distinct from `Emitted` so throughput does not count it as work delivered.
- Diagnostics (`get_diagnostics`), structured logging with path redaction
  (`src-tauri/src/logging.rs`), and ledger read APIs behind the review loop.
- `backlog-core` crate: `harvest` + `checker` extracted with no Tauri, sidecar
  or icon dependency, so the trust core tests on a bare checkout — which is
  what makes Linux CI possible at all.

### Changed
- **Licensing-clean model swap**: Qwen3-0.6B/1.7B, Lingua and RapidOCR 3
  replace LFM2.5, fastText `lid.176` and `rapidocr-onnxruntime`.
- **Slim, torch-free sidecar**: `torch`, `transformers`,
  `sentence-transformers` and `gliclass` removed (~3x smaller Python footprint).
  `classify`, `salience` and `ettin_spans` degrade to deterministic `ok=true`
  fallbacks with `available: false`, so no document is ever flagged over a
  missing naming enhancement.
- Manifest schema **v2 → v3**: adds the `dismissed` status and requires a
  non-empty `model_versions` on an `ok` manifest.
- Model paths are rehomed at startup to `%APPDATA%\ai.sonomos.backlog\models`;
  a path set through Settings' Browse dialog passes through untouched.
- Retry ladder rungs now vary the *evidence bundle*, not just the model tier —
  rung 3 used to be byte-identical to rung 1.

### Fixed
- **Duplicate path.** The duplicate manifest id was `{sha}:{uuid}`: `:` is
  invalid on NTFS so the write silently failed on Windows, and the fresh UUID
  made replay non-idempotent. Now a deterministic filesystem-safe per-copy key,
  with a durable ledger row per physical copy so `(2)`/`(3)` names increment.
- **Sidecar could wedge the pipeline.** stdout is drained on a reader thread
  with an enforced per-request deadline (kill + respawn on timeout), and `Drop`
  guarantees no orphaned sidecar processes.
- **Crash-loop guard**: a durable attempt counter quarantines a poison-pill
  document as `CRASH_LOOP` after 5 restarts.
- **Watcher no longer skips leading-underscore filenames.** `_DRAFT
  Agreement.docx` was silently dropped — no ledger row, no manifest, no log.
- Unicode crash in `harvest()`; `tail_pages=0` trim no-op; pdfium bitmap leak;
  `get_evidence` path traversal; PII in the persisted `events` table; unbounded
  header read during type detection.
- Removed `tauri-plugin-shell` and `tauri-plugin-opener` and their capability
  grants — all dead code, and pure IPC attack surface.
