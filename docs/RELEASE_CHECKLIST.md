# BackLog pilot release checklist

A release is a **pilot candidate**, not production-ready, until every applicable
item below has fresh evidence attached to the release record.

## Source and automated validation

- [ ] All local validation commands below pass on the exact release commit
      (this project has no GitHub Actions CI; every check is run locally
      before release).
- [ ] `npm ci && npm run check` passes under Node 22.
- [ ] `npm run build` passes under Node 22.
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` passes
      under Rust 1.97.1.
- [ ] `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`
      passes.
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --all-targets` passes.
- [ ] Sidecar and model-lock unit tests pass under 64-bit Python 3.11.
- [ ] Every Python source compiles.
- [ ] Every Power Automate example validates against both manifest schemas.
- [ ] `package-lock.json`, `rust-toolchain.toml`, and `src-tauri/Cargo.lock` are
      committed and match the release build.

## Offline model bundle

- [ ] The bundle contains `models.lock.json` at its root or under `models/`.
- [ ] `python models/download_models.py --verify-only` passes after extraction.
- [ ] The model ZIP SHA-256 is recorded before the Windows workflow starts.
- [ ] The bundle contains the locked Qwen3 0.6B and 1.7B Q8_0 GGUF files. This
      is the slim, torch-free sidecar profile: no GLiClass snapshot, Granite
      embedding snapshot, torch, transformers, or sentence-transformers ship
      with it (see `docs/DEPENDENCY_COMPATIBILITY.md`); `classify` and
      `salience` answer `ok=true` with deterministic fallbacks instead.
- [ ] No Liquid LFM, Liquid VL, fastText, `lid.176`, or untracked model payload
      remains in the model directory.
- [ ] The optional trained Ettin directory is disabled, or held-out metrics meet
      DATE F1 >= 0.90 and PARTY/SUBJECT F1 >= 0.75 for each enabled label.
- [ ] The license and notice review in `DEPENDENCY_COMPATIBILITY.md` is complete
      for the intended pilot audience.

## Windows packaging

- [ ] Build on a clean Windows Server 2022 or Windows 11 environment with
      64-bit Python 3.11, Node 22, and Rust 1.97.1.
- [ ] A hash-pinned `sidecar/requirements.lock` is used for any signed release.
      An unlocked build may be used only as an explicitly labeled internal pilot.
- [ ] `scripts/build-sidecar.ps1 -Clean` succeeds and the NDJSON ping smoke test
      passes.
- [ ] llama-server is pinned by release provenance, version output, and SHA-256.
- [ ] The Windows workflow verifies the model ZIP and every file in
      `models.lock.json` before invoking Tauri.
- [ ] The NSIS installer contains `models/` under the Tauri resource directory.
- [ ] Installer, sidecar, llama-server, model ZIP, model lock, and model payload
      hashes are retained.
- [ ] Install, same-version repair, upgrade, and uninstall complete under a
      standard non-administrator user where policy permits.
- [ ] First launch resolves the bundled Qwen paths without depending on the
      shortcut working directory.
- [ ] First launch shows actionable preflight failures when folders, models, or
      sidecars are missing.

## Privacy and security

- [ ] With the network disabled, conversion, OCR, language detection,
      classification, naming, review, and manifest emission still work.
- [ ] An outbound-connection monitor confirms the app, sidecar, and llama-server
      contact only loopback during runtime.
- [ ] No document text appears in SharePoint `_pa_errors`, application telemetry,
      crash uploads, local build logs, or release artifacts.
- [ ] Tauri capabilities contain no shell or opener permission, and the Rust app
      does not initialize those plugins.
- [ ] The sidecar sets Hugging Face, Transformers, and Datasets offline modes.
- [ ] Installer and external binaries are code-signed before deployment outside
      the internal pilot group.
- [ ] Anti-malware scanning is complete for every bundled executable.

## Functional smoke tests

- [ ] Native Word, PDF, and text fixtures convert and produce schema-valid
      manifests.
- [ ] A scanned fixture exercises RapidOCR; low confidence retries at higher DPI
      and ends with enhanced 600-DPI classical OCR before `UNREADABLE`.
- [ ] A Danish Unicode fixture processes without panic or truncation corruption,
      and Lingua reports an ISO 639-1 language code.
- [ ] An undated fixture uses file modified date and labels the source
      `metadata`.
- [ ] An unsupported or zero-byte file is flagged without silent deletion.
- [ ] Three byte-identical physical files receive one content SHA-256, three
      instance IDs, three manifest IDs, and three distinct final filenames.
- [ ] Replaying the same manifest cannot create a second list row.
- [ ] Pausing before a file arrives and resuming later does not lose the file.
- [ ] Killing `convertd` during a request causes a bounded restart path rather
      than an indefinite hang.
- [ ] A failed manifest write leaves the source recoverable.
- [ ] Qwen JSON output that violates date, subject, or description rules is
      rejected by the deterministic checker and retried with the exact violation.

## Power Automate

- [ ] Flow 1 uses the stable `__incoming_<flow-id>-<item-id>__` envelope and
      deletes the Intake source only after destination creation succeeds.
- [ ] Flow 2 uses manifest schema v2 and durable commit checkpoints.
- [ ] `ManifestId`, `InstanceId`, and `Sha256` columns are indexed.
- [ ] Flow concurrency is 1 and app emission is 10 manifests/min for the pilot.
- [ ] Forced failures after archive, after index, and after source move each
      resume without duplicate rows or stranded files.
- [ ] The 15-minute recovery sweep is enabled and monitored.

## Release evidence

- [ ] Record commit SHA, installer SHA-256, model ZIP SHA-256, external binary
      versions/hashes, model-lock SHA-256, config snapshot, Power Automate export
      versions, test results, and known limitations.
- [ ] Keep the release labeled `pilot` until the staged runbook gates pass.
