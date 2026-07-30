# Handoff: convertd startup time and the readiness probe timeout

**Date:** 2026-07-30 · **Branch merged from:** `fix/sidecar-startup-timeout` (based on `v0.5.0`)

**Status: code complete, NOT verified end to end.** Every change below is
written and reviewed, and the sidecar half is measured and proven. The app half
has never been run, because no release build of the app crate finished on the
machine this was done on — for environment reasons unrelated to the changes
(see "Why it is unverified"). **Do not cut a release from this without
completing the verification checklist at the bottom.**

---

## The bug this fixes

On an installed v0.5.0, Settings → Readiness reported **"Document reader
answers — Blocked"** every time and **"Naming engine starts — Blocked"**
intermittently, on an install where nothing was actually wrong: both sidecars,
all 32 DLLs, the grammar file and both models were present and correct, and
both binaries ran fine when launched by hand.

Two causes, one of them a consequence of the other.

**1. The probe deadline was shorter than the sidecar's start-up.**
`preflight.rs` clamped both liveness probes to five seconds
(`cfg.sidecar_timeout_secs.clamp(1, 5)`), overriding the configured 45. But
`convertd` was a ~250 MB PyInstaller **onefile**, which unpacks its whole
payload into a fresh `%TEMP%\_MEI*` directory on *every* launch. On a machine
whose antivirus scans each unpacked file, three consecutive cold starts
measured **34 s, 52 s, 38 s** — slow on every launch, not just the first, so
that probe could never pass. Corroborating evidence on the affected machine:
**20 abandoned `_MEI*` directories**, one per killed probe, holding ~2 GB.

**2. The naming-engine probe inherited the wreckage.** It runs immediately
after, on the same five-second budget, while the timed-out `convertd` keeps
unpacking in the background. `llama-server --version` measured **0.9 s idle but
3.0 s** under that contention, and crossed five seconds on a cold boot with the
antivirus scanning its 32 DLLs. So a second component was reported broken for a
reason that had nothing to do with it.

Note the app's existing message for the llama failure — "Your antivirus may
have blocked it" — was nearly right and still misleading: antivirus was
involved, but as *latency*, never as a block.

---

## What changed

| File | Change |
|---|---|
| `src-tauri/src/preflight.rs` | Probe ceiling `clamp(1, 5)` → `clamp(1, 60)`, so the configured `sidecar_timeout_secs` is honored. |
| `src-tauri/src/lib.rs` | Same ceiling in `review_probe_timeout()` — a **second copy** of the identical bug, on the approval path. Plus convertd path resolution for the onedir layout, and a guard described below. |
| `scripts/build-sidecar.ps1` | PyInstaller `--onefile` → `--onedir`; stages the whole tree to `src-tauri/binaries/convertd/`; smoke test and SHA-256 updated. |
| `src-tauri/tauri.conf.json` | `convertd` removed from `externalBin` (it can only carry a single file) and shipped via `resources` as `"binaries/convertd/": "convertd/"`. `llama-server` still uses `externalBin`. |
| `scripts/verify-binaries.ps1` | Validates `convertd/convertd.exe` plus a non-empty `_internal/`, instead of a flat exe. Does not PE-check `_internal` contents — many are legitimately not PE (ONNX weights, fonts, metadata). |
| `scripts/dev-stubs.{ps1,sh}` | Stage `binaries/convertd/convertd.exe` in the new layout. |
| `.github/scripts/check-stub-marker.mjs` | Its flat `readdirSync` scan hard-failed on the new layout *and* silently stopped checking convertd at all. |
| `sidecar/requirements.lock` | Regenerated. The committed one pinned `onnxruntime==1.28.0`, which is uninstallable on Windows because `magika` caps it at `<=1.20.1` on win32 — a Linux-generated lock. See "Still open". |
| `sidecar/BUILD.md`, `README.md` | Describe the onedir layout and why it exists. |
| `src-tauri/Cargo.toml` | Dropped `cdylib` from `crate-type`. It exists for mobile targets this app does not build, and GNU linkers refuse a debug cdylib of this crate ("export ordinal too large"), so no windows-gnu build could link at all. |

### One non-obvious guard worth keeping

`resolve_binary` tries `<exe_dir>/<name>.exe` **before** the resource path.
That is right for an `externalBin` sidecar and actively harmful for convertd:
upgrading over a `--onefile` install leaves a stale 248 MB `convertd.exe` beside
the app, and it would win. Nothing would look broken — the app would simply go
on unpacking a quarter-gigabyte per launch, silently reverting this entire fix.
So `binary()` resolves convertd against its own tree and nothing else.

---

## What is verified, with numbers

- `scripts/build-sidecar.ps1` completes and its smoke test passes — real
  `.docx`, `.pdf` and scanned `.png` driven through the built binary, all
  `ok:true` with non-empty markdown and `ocr_used:true`.
- **convertd cold start: 34–52 s → 13–15 s.**
- **Runtime unpacking eliminated.** Repeated runs of the onedir build created
  **zero** new `_MEI*` directories; the onefile build created one every launch.
- Tauri's directory-resource semantics were confirmed against Tauri 2 docs
  before the change: a trailing-slash key copies a tree recursively (preserving
  `_internal/`'s subdirectories), and on Windows `resource_dir()` *is* the
  directory holding the executable — hence `<install>\convertd\convertd.exe`.

The residual 13–15 s is antivirus scanning the 72 libraries convertd loads at
start. It is comfortably inside the new 60 s ceiling. An antivirus exclusion for
the install directory would take it to roughly a second, and needs IT approval
on a managed fleet — worth requesting, not required for correctness.

---

## Why it is unverified, and what is left

No release build of the app crate ever completed. Nothing in that is a defect
in the changes above; it was all local build-environment failure, and each
cause is now understood and recorded in "Environment notes" below. The app
half — the two timeout ceilings and the onedir path resolution — has therefore
never executed.

### Verification checklist before trusting or shipping this

1. `cargo build --release` (or `npm run tauri build`) completes.
2. Install/stage it, then confirm **Settings → Readiness shows every row green**
   on a machine where convertd takes >5 s to start. That is the whole point;
   everything else is secondary.
3. Confirm `<install>\convertd\convertd.exe` and `<install>\convertd\_internal\`
   exist after an NSIS install, and that **no flat `<install>\convertd.exe`
   remains** — especially on an *upgrade over v0.5.0*, which is the case the
   guard above exists for.
4. Start the pipeline and put a real document through it end to end.
5. Approve a document from the review pane — that exercises
   `review_probe_timeout`, the second ceiling, which nothing else covers.
6. Run the gates: `cargo test --workspace --all-targets`, `cargo clippy
   --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`.
   **None have been run against these edits.**
7. `scripts/verify-binaries.ps1` against a real staged tree — its new
   convertd branch has not been executed either.

### Known risk, not yet assessed

NSIS packaging of a onedir tree means thousands of small files instead of one
248 MB blob. Correctness is not in doubt, but installer build time, installer
size and install/uninstall duration are all unmeasured. If the installer becomes
unacceptably slow, the fallback is to keep onefile and rely on the timeout fix
alone — which resolves the reported symptom by itself, at the cost of leaving
the 34–52 s start-up and the temp-directory litter in place.

---

## Still open (pre-existing, surfaced by this work)

- **`sidecar/requirements.lock` is regenerated but still not hash-pinned**
  (`docs/KNOWN_ISSUES.md` item 2 stands). It also now carries `pyreadline3`,
  a Windows-conditional dependency that `pip freeze` records unconditionally;
  it installs harmlessly on Linux (pure-Python wheel) but is dead weight there.
  A genuinely portable lock needs markers `pip freeze` cannot express.
- The lock that shipped in v0.5.0 could never have installed on Windows, which
  means **no Windows machine has built the sidecar from the committed lock**.
  Worth understanding how that reached a release before trusting the next one.

---

## Environment notes for whoever picks this up

Recorded because they cost hours and none are obvious. These describe the
machine this was done on, not the repo.

- **Two Git installations** with different MSYS roots — `PortableGit` and
  `Programs\Git`. `/usr` resolves to different physical folders in each, so
  perl modules installed for one are invisible to the other. This is what made
  the vendored-OpenSSL build fail *only* when launched from a detached shell,
  while every interactive test passed.
- **Vendored OpenSSL needs perl modules Git's perl omits** (`IPC::Cmd`,
  `Params::Check`, `Locale::Maketext::Simple`, `Module::Load::Conditional`,
  `Pod::Usage`, `ExtUtils::MakeMaker`). They were copied from a portable
  Strawberry Perl into each Git's `usr/share/perl5/site_perl`. Do **not** put
  Strawberry on `PATH` or in `PERL5LIB` — its native XS modules break cygwin
  perl, and MSYS mangles a POSIX `PERL5LIB` when crossing into native processes.
- **A killed `cargo` leaves `target/<profile>/.cargo-lock` and orphan
  processes.** The next build then prints `Blocking waiting for file lock` and
  waits forever, which is indistinguishable from a slow compile. Kill
  `cargo.exe`/`rustc.exe`, confirm zero remain, delete the lock, and after
  relaunching **confirm the log says `Compiling` rather than `Blocking`**.
- **A build launched from a shell that is later cleaned up gets its `rustc`
  children killed while `cargo` survives**, and cargo then waits on processes
  that no longer exist — silent, and again indistinguishable from a slow
  compile. Launch detached (`Start-Process`), and watch for *both* `cargo` and
  `rustc` being alive, not just `cargo`.
- **`CC`/`CXX` are folded into `openssl-sys`'s fingerprint.** Changing the
  literal string — even to another spelling of the same compiler — forces a
  fresh ~1 hour OpenSSL compile. If the build must run under Git Bash for MSYS
  path translation, keep the POSIX spelling exactly as-is.
- **A release build needs the bundled model staged** at
  `src-tauri/resources/models/Qwen3-0.6B-Q8_0.gguf` or `tauri-build` fails on a
  non-matching resource glob. `scripts/stage-release-inputs.ps1` fetches it; a
  hardlink to an existing copy is instant.
- **windows-gnu `cargo test` binaries die at load with
  `STATUS_ENTRYPOINT_NOT_FOUND` (0xc0000139)** — `rfd` imports
  `TaskDialogIndirect`, which only comctl32 **v6** exports, and test exes carry
  no manifest requesting it. Linux CI never sees this. Working around it needs
  an external `<exe>.manifest` plus zeroing the rustc-embedded resource
  directory; helper scripts for that live on the `wip/windows-dev-fixes-20260729`
  branch (`scripts/test-exe.manifest`, `scripts/zero-rsrc-dir.py`).

## Related branches

- `wip/windows-dev-fixes-20260729` — an earlier Windows session against the
  pre-v0.5.0 tree. Its log-scrubber fixes were **independently fixed in
  v0.5.0** and are not needed; what remains useful there are the two
  windows-gnu test-binary helper scripts named above. Not merged deliberately:
  it is based on a stale `main` and would conflict.
