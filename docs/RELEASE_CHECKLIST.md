# BackLog pilot release checklist

A release is a **pilot candidate**, not production-ready, until every applicable
item below has fresh evidence attached to the release record.

## Source and automated validation

- [ ] Pull request CI is green on the exact release commit.
- [ ] `npm ci && npm run check` passes.
- [ ] `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check` passes.
- [ ] `cargo test --manifest-path src-tauri/Cargo.toml --all-targets` passes.
- [ ] Sidecar and model utility unit tests pass under Python 3.11.
- [ ] All Python sources compile.
- [ ] Every Power Automate example validates against `manifest.schema.json`.
- [ ] `package-lock.json` and `src-tauri/Cargo.lock` are committed and match the
      release build.

## Offline model bundle

- [ ] `models/models.lock.json` exists and is committed.
- [ ] `python models/download_models.py --verify-only` passes on the deployment
      model directory.
- [ ] The optional VL snapshot is either fully locked and tested or explicitly
      omitted.
- [ ] A trained Ettin directory is either disabled or its held-out metrics meet
      DATE F1 >= 0.90 and PARTY/SUBJECT F1 >= 0.75 for each enabled label.
- [ ] License and redistribution review in `DEPENDENCY_COMPATIBILITY.md` is
      complete for the intended pilot audience.

## Windows packaging

- [ ] Build on a clean Windows 11 or Windows Server 2022 runner with 64-bit
      Python 3.11.
- [ ] `scripts/build-sidecar.ps1 -Clean` succeeds and its ping smoke test passes.
- [ ] llama-server is pinned by version and SHA-256.
- [ ] The Tauri installer builds from the exact release commit.
- [ ] Installer, sidecar, llama-server, and model hashes are retained.
- [ ] Install and uninstall complete under a standard non-administrator user
      where organizational policy permits.
- [ ] First launch shows actionable preflight failures when folders, models, or
      sidecars are missing.
- [ ] The application starts after valid paths are selected.

## Privacy and security

- [ ] With the network disabled, conversion, OCR, classification, naming,
      review, and manifest emission still work.
- [ ] An outbound-connection monitor confirms that the app, sidecar, and
      llama-server contact only localhost during runtime.
- [ ] No document text appears in SharePoint `_pa_errors`, application telemetry,
      crash uploads, or GitHub artifacts.
- [ ] Tauri capabilities contain no unused shell or opener permission.
- [ ] Installer and external binaries are code-signed before broader deployment.
- [ ] Anti-malware scanning is complete for every bundled executable.

## Functional smoke tests

- [ ] Native Word/PDF/text fixture converts and produces a schema-valid manifest.
- [ ] Scanned fixture exercises RapidOCR; low confidence follows the retry ladder.
- [ ] Danish Unicode fixture processes without panic or truncation corruption.
- [ ] Undated fixture uses file modified date and labels the source `metadata`.
- [ ] An unsupported or zero-byte file is flagged without deletion.
- [ ] Three byte-identical physical files receive one content SHA-256, three
      instance IDs, three manifest IDs, and three distinct final filenames.
- [ ] Replaying the same manifest cannot create a second list row.
- [ ] Pausing before a file arrives and resuming later does not lose the file.
- [ ] Killing `convertd` during a request causes a timeout/restart path rather
      than an indefinite hang.
- [ ] A failed manifest write leaves the source recoverable.
- [ ] Human correction resolves every still-flagged physical duplicate.

## Power Automate

- [ ] Flow 1 uses the stable `__bl_<UniqueId>__` transfer prefix and deletes the
      Intake source only after destination confirmation.
- [ ] Flow 2 uses manifest schema v2 and `BackLogCommits` checkpoints.
- [ ] `ManifestId`, `InstanceId`, and `Sha256` columns are indexed.
- [ ] Flow concurrency is 1 and app emission is 10 manifests/min for the pilot.
- [ ] Forced failures after archive, after index, and after source move each
      resume without duplicate rows or stranded files.
- [ ] The 15-minute recovery sweep is enabled and monitored.

## Release evidence

- [ ] Record commit SHA, installer SHA-256, external binary versions/hashes,
      model-lock SHA-256, config snapshot, Flow export versions, test results,
      and known limitations.
- [ ] Keep the release labeled `pilot` until the staged runbook gates pass.
