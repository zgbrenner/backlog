# BackLog pilot release checklist

A release is a **pilot candidate**, not production-ready, until every applicable
item below has fresh evidence attached to the release record.

> Every command in this file has been run on the current tree and passes. An
> item that cannot pass is marked so explicitly rather than left as a box to
> tick on faith — that is how the `cargo fmt` failure survived unnoticed
> through several reviews of a checklist that called it mandatory.

## Source and automated validation

`./scripts/ci-local.sh` runs all five jobs — trust core, workspace, frontend,
python, version-drift — on one machine and exits non-zero on the first failure.
**Run it on a clean checkout of the exact release commit** — that is the item;
the rest of this section says what it covers, and each line is one step inside
it.

- [ ] `./scripts/ci-local.sh` passes on a clean checkout of the release commit,
      and the run is attached to the release record.
- [ ] ~~CI is green on the release commit.~~ **This is not satisfiable today.**
      `.github/workflows/ci.yml` describes the same five jobs but has never
      been assigned a runner (`docs/KNOWN_ISSUES.md` item 11), so no commit in
      this repository has ever had a green Actions run and none can be produced
      on demand. The local script above is the substitute, and
      `node .github/scripts/check-ci-parity.mjs` — part of the version-drift
      job — is what keeps it honest by failing if the two definitions drift.
- [ ] `npm ci && npm run check` passes under Node 22.
      (`check` = `tsc --noEmit` then `vite build`.)
- [ ] `npm run harness:shots` renders every scenario with no console error, and
      the screenshots have been looked at.
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` passes.
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --workspace --all-targets -- -D warnings`
      passes.
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --workspace --all-targets`
      passes.
- [ ] Sidecar and model-lock unit tests pass under 64-bit Python 3.11:
      `python -m pytest sidecar/tests models/tests`.
- [ ] Every Power Automate example validates against both manifest schemas, and
      the schemas match the emitter:
      `pip install -r power-automate/requirements-dev.txt` then
      `python power-automate/validate_examples.py`. (Without that install it
      dies with `ModuleNotFoundError: jsonschema`.)
- [ ] `package-lock.json`, `rust-toolchain.toml`, `src-tauri/Cargo.lock` and
      `sidecar/requirements.lock` are committed and match the release build.
- [ ] `package.json`, `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml`
      declare the same version (`node .github/scripts/check-versions.mjs`), and
      `CHANGELOG.md` has a section for it.

## Offline model bundle

The installer does **not** contain the models. They arrive either through the
in-app downloader (Settings → Download models) or by copying two `.gguf` files
into `%APPDATA%\ai.sonomos.backlog\models` by hand. A model ZIP is optional and
only exists for air-gapped deployment.

- [ ] `python models/download_models.py --verify-only` passes against the staged
      `models/` directory. (`--verify-only` is one of only two flags this script
      has; the other is no flag at all.)
- [ ] `models.lock.json` is committed and its SHA-256 is recorded.
- [ ] The staged set is exactly the locked Qwen3 0.6B and 1.7B Q8_0 GGUF files.
      This is the slim, torch-free sidecar profile: no GLiClass snapshot,
      Granite embedding snapshot, torch, transformers, or sentence-transformers
      ship with it (see `docs/DEPENDENCY_COMPATIBILITY.md`); `classify` and
      `salience` answer `ok=true` with deterministic fallbacks instead.
- [ ] No Liquid LFM, Liquid VL, fastText, `lid.176`, or untracked model payload
      remains in the model directory.
- [ ] If an offline model ZIP is produced: it contains `models.lock.json` at its
      root or under `models/`, and its SHA-256 is recorded before packaging
      starts.
- [ ] The optional trained Ettin directory is disabled, or held-out metrics meet
      DATE F1 >= 0.90 and PARTY/SUBJECT F1 >= 0.75 for each enabled label.
      (It is disabled in every shipped build — the slim sidecar has no
      `transformers`, so the lane is inert. See `training/README.md`.)
- [ ] `NOTICE.md` matches what this build actually redistributes, and the
      license and notice review in `DEPENDENCY_COMPATIBILITY.md` is complete for
      the intended pilot audience.

## Windows packaging

- [ ] Build on a clean Windows Server 2022 or Windows 11 environment with
      64-bit Python 3.11 (exactly), Node 22, and the toolchain in
      `rust-toolchain.toml`.
- [ ] `scripts/build-sidecar.ps1 -Clean` succeeds and the NDJSON ping smoke test
      passes.
- [ ] llama-server is staged with **all** its runtime DLLs and pinned by release
      provenance, `--version` output, and SHA-256 (`RELEASING.md` Build step 2).
- [ ] **Dev stubs are gone and the real binaries verified:**
      `pwsh scripts/verify-binaries.ps1` exits 0. It asserts each
      `binaries/*.exe` is non-empty, carries no `BACKLOG-DEV-STUB-DO-NOT-SHIP`
      marker, is a valid PE image, matches its recorded SHA-256 where one is
      supplied, and that `_placeholder.dll` is absent. **Do not skip this.**
      `tauri.conf.json`'s `externalBin` only checks that a path exists, so a
      stubbed installer builds clean, installs clean and reports green — the
      failure surfaces on the first document to reach the SLM lane on a user's
      machine, with no logs. **This script is the only gate that catches it:**
      the in-app readiness check still passes a marked stub, because
      `preflight.rs`'s `binary_exists` only requires a non-empty file
      (`docs/KNOWN_ISSUES.md` item 9).
- [ ] `bundle.windows.webviewInstallMode` is `offlineInstaller` and
      `bundle.windows.nsis.installMode` is `currentUser` in `tauri.conf.json`.
      (Per-machine would make every passive auto-update raise a UAC prompt the
      appliance user cannot satisfy; the default WebView2 bootstrapper would
      fetch from Microsoft mid-install.)
- [ ] Installer, sidecar, llama-server, model lock and model payload hashes are
      retained.
- [ ] Install, same-version repair, upgrade, and uninstall complete under a
      standard non-administrator user where policy permits.
- [ ] First launch resolves the app-data model paths without depending on the
      shortcut working directory, and Settings → Download models works from a
      genuinely empty models directory.
- [ ] First launch shows actionable preflight failures when folders, models, or
      sidecars are missing, each with a plain-language message and — where one
      applies — a working action button.
- [ ] `sidecar/requirements.lock` is hash-pinned for a **signed** release
      (`pip-compile --generate-hashes`). **This is not satisfiable today** — the
      lock is version-pinned only (`docs/KNOWN_ISSUES.md` item 2). An unlocked
      build may be used only as an explicitly labeled internal pilot.

## Privacy and security

- [ ] With the network disabled, conversion, OCR, language detection,
      classification, naming, review, and manifest emission still work.
- [ ] An outbound-connection monitor confirms that during **document
      processing** the app, sidecar, and llama-server contact only loopback, and
      that the only non-loopback connections the app makes at all are the two
      documented ones: the Hugging Face model download (once, from the Settings
      button) and the startup updater check to
      `github.com/zgbrenner/backlog/releases/latest/download/latest.json`
      (`src/main.ts`'s `checkForUpdates`). "Loopback-only at runtime" on its own
      is false and was reported as such.
- [ ] No document text appears in SharePoint `_pa_errors`, application telemetry,
      crash uploads, local build logs, or release artifacts.
- [ ] Tauri capabilities contain no shell or opener permission, and the Rust app
      does not initialize those plugins.
- [ ] The sidecar sets Hugging Face, Transformers, and Datasets offline modes.
- [ ] Installer and external binaries are code-signed before deployment outside
      the internal pilot group. (No certificate exists yet — every install shows
      a SmartScreen warning; the click-through is documented in
      `docs/USER_GUIDE.md`.)
- [ ] Anti-malware scanning is complete for every bundled executable.
- [ ] `docs/PRIVACY.md`'s uninstall-residue statement still matches what the
      installer leaves behind.

## Functional smoke tests

- [ ] Native Word, PDF, and text fixtures convert and produce schema-valid
      manifests.
- [ ] A scanned fixture exercises RapidOCR; low confidence retries at 400 DPI
      and ends with enhanced 600-DPI classical OCR before `UNREADABLE`.
- [ ] A Danish Unicode fixture processes without panic or truncation corruption,
      and Lingua reports an ISO 639-1 language code.
- [ ] An undated fixture uses file modified date, labels the source `metadata`,
      and carries `DATE_FROM_FILE_MTIME`.
- [ ] An unsupported or zero-byte file is flagged without silent deletion.
- [ ] A file whose name begins with an underscore (`_DRAFT Agreement.docx`) is
      processed, not skipped.
- [ ] Three byte-identical physical files receive one content SHA-256, three
      instance IDs, three manifest IDs, and three distinct final filenames.
- [ ] Replaying the same manifest cannot create a second list row.
- [ ] A dismissal ("Can't fix this") produces a `dismissed` manifest that lands
      in `NeedsReview` with `ReviewState = Dismissed` and **never** reaches
      `DocumentIndex`.
- [ ] Pausing before a file arrives and resuming later does not lose the file.
- [ ] Killing `convertd` during a request causes a bounded restart path rather
      than an indefinite hang.
- [ ] A failed manifest write leaves the source recoverable.
- [ ] Qwen JSON output that violates date, subject, or description rules is
      rejected by the deterministic checker and retried with the exact violation.

## Power Automate

- [ ] Flow 1 delivers each file under its **plain original name** inside a
      per-delivery subfolder (`/BackLog/Processing/<flow-id>-<item-id>/<name>`)
      and deletes the Intake source only after destination creation succeeds.
      (The old `__incoming_…__` filename envelope described app behavior that
      does not exist — see `power-automate/FLOW1-intake.md`.)
- [ ] Flow 2 uses manifest schema **v3** — including the `dismissed` status and
      the non-empty `model_versions` requirement on `ok` — and durable commit
      checkpoints.
- [ ] `ManifestId`, `InstanceId`, and `Sha256` columns are indexed.
- [ ] Flow concurrency is 1 and app emission is
      `"manifest_emit_per_min": 10` for the pilot. The shipped default in
      `config.rs` is `0`, meaning unlimited — it must be set explicitly.
- [ ] Forced failures after archive, after index, and after source move each
      resume without duplicate rows or stranded files.
- [ ] The 15-minute recovery sweep is enabled and monitored.

## Release evidence

- [ ] Record commit SHA, the `./scripts/ci-local.sh` output, installer SHA-256, external binary
      versions/hashes, model-lock SHA-256, config snapshot, Power Automate export
      versions, test results, and known limitations.
- [ ] The release carries the installer, its `.sig`, **and** `latest.json` as
      assets, and `latest.json`'s `version` equals `tauri.conf.json`'s
      (`RELEASING.md` Cutting a release step 4, the update-channel assertion).
      Publishing without `latest.json` makes every
      installed copy 404 on its update check, and the frontend swallows that
      error — the update channel can be dead fleet-wide with no signal anywhere.
- [ ] Keep the release labeled `pilot` until the staged runbook gates pass.
