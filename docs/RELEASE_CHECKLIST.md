# BackLog v0.5.0 release checklist

A release remains a pilot candidate until every applicable item has fresh
evidence attached to the release record.

## Source and automated validation

- [ ] The release commit is on `main`, has passed `.github/workflows/ci.yml`,
      and the workflow URL is recorded. This public repository's standard
      Actions runners use no billable minutes.
- [ ] `./scripts/ci-local.sh` passes on a clean checkout.
- [ ] `node .github/scripts/check-ci-parity.mjs` confirms hosted and local CI
      run the same staging and gate commands.
- [ ] `npm run check:release` passes all release-contract tests and validates
      `.github/workflows/release.yml`.
- [ ] `npm run check` and `npm run harness:shots` pass under Node 22, with no
      browser console error.
- [ ] Rust format, clippy, and all workspace tests pass with
      `src-tauri/Cargo.lock` and Rust 1.94.1.
- [ ] Sidecar/model unit tests pass with Python 3.11:

  ```bash
  python -m unittest discover -s sidecar/tests -t sidecar/tests
  python -m unittest discover -s models/tests -t models/tests
  ```

- [ ] `python power-automate/validate_examples.py` passes after installing
      `power-automate/requirements-dev.txt`.
- [ ] `node .github/scripts/check-versions.mjs` reports 0.5.0, the `backlog`
      package in `src-tauri/Cargo.lock` is 0.5.0, and `CHANGELOG.md` has the
      0.5.0 section.

## One-download package

- [ ] The only required user download is
      `BackLog_0.5.0_x64-setup.exe`.
- [ ] The installer contains the app, `convertd` with its Python runtime,
      `llama-server` and every imported runtime DLL, Qwen3 0.6B Q8_0, and the
      offline WebView2 runtime.
- [ ] The primary model hash is
      `9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031`.
- [ ] The Qwen3 1.7B Q8_0 model is not in the installer. Settings presents it
      as an optional, cancellable, resumable in-app download for difficult
      documents.
- [ ] A clean install can process a document with the network disabled and
      without Python, a VC++ redistributable, a model script, or any second
      installer.
- [ ] First launch moves the bundled primary model into the per-user model
      directory without changing an operator's custom absolute model path.

## Windows packaging evidence

- [ ] The release workflow ran on `windows-2022` with Node 22, Python 3.11 x64,
      and the toolchain from `rust-toolchain.toml`.
- [ ] `npm ci`, Cargo `--locked` resolution, and
      `sidecar/requirements.lock` were used.
- [ ] `scripts/stage-release-inputs.ps1` verified the primary model and the
      `llama.cpp b10091` archive before staging.
- [ ] `scripts/build-sidecar.ps1 -Clean` smoke-tested the built sidecar against
      real DOCX, PDF, and scanned-image fixtures.
- [ ] `scripts/verify-binaries.ps1` passed: no file carries
      `BACKLOG-DEV-STUB-DO-NOT-SHIP`, every executable/DLL is a valid PE image,
      the pinned llama server hash matches, `_placeholder.dll` is absent, and
      every non-Windows imported DLL is bundled.
- [ ] `bundle.windows.webviewInstallMode` is `offlineInstaller` and the NSIS
      install mode is `currentUser`.
- [ ] Installer SHA-256, exact release-input hashes, commit SHA, and workflow
      URL are retained.
- [ ] Install, same-version repair, upgrade, and uninstall succeed under a
      standard non-administrator account where policy permits.

## Recovery and user experience

- [ ] On first run, **Save and check this computer** persists the selected
      folders before readiness runs.
- [ ] An active optional-model transfer can be cancelled; Cancelled and Failed
      states offer Resume; Completed state survives navigation.
- [ ] With the optional 1.7B model absent, readiness says it is absent, stays
      usable, and safely uses the primary model for backup naming attempts.
- [ ] Quitting during work releases claims; restart immediately reclaims and
      resumes them without waiting for a stale timeout.
- [ ] A background model request cannot respawn `llama-server` after shutdown
      begins.
- [ ] A valid previously written terminal manifest is reconciled without
      repeating model work.
- [ ] A failed quarantine or manifest write leaves the job visible and
      retryable.
- [ ] Only one BackLog process per user can own startup claim recovery.
- [ ] **Processing is caught up** is distinct from **All caught up** when
      Needs Review still contains work.
- [ ] Pending approval Undo remains available after navigating away from
      Needs Review.
- [ ] Documents without a trustworthy date require review; a genuinely
      undated document uses the file-modified date with honest provenance.

## Power Automate handoff

- [ ] **Done** is described and tested as “manifest handed to Power Automate,”
      not “confirmed in SharePoint.”
- [ ] Flow 2 owns rename, archive, SharePoint copy/index, checkpoints, and
      retries after handoff.
- [ ] Flow 2 uses manifest schema v3 and `manifest_id`, not content SHA, as the
      replay key.
- [ ] `ManifestId`, `InstanceId`, and `Sha256` columns are indexed.
- [ ] Flow concurrency is 1 and the pilot uses
      `"manifest_emit_per_min": 10`.
- [ ] Forced failures after archive, index, and source move resume without
      duplicate rows or stranded files.

## Privacy and security

- [ ] With networking disabled, conversion, OCR, classification, naming,
      review, and manifest emission work.
- [ ] During processing, the app and sidecars contact only loopback. The only
      documented non-loopback app operations are the optional model download
      and startup updater check.
- [ ] No document text, filename, model proposal, key, or personal path appears
      in CI logs or release artifacts.
- [ ] Anti-malware scanning is complete for every bundled executable.
- [ ] `docs/PRIVACY.md`, `docs/SECURITY.md`, `NOTICE.md`, and the package
      contents agree.
- [ ] The absence of Authenticode is explicitly accepted for this pilot.
      SmartScreen reputation remains an external constraint.

## Publication guard

- [ ] `v0.5.0` does not exist before the prepared release commit reaches
      `main`; successful CI starts the release workflow automatically for that
      exact commit.
- [ ] The release-state preflight starts Windows packaging only after successful
      CI on `main` for the exact release commit. A published `v0.5.0` skips
      cleanly; only a matching interrupted draft can resume, and a tag pointing
      at any other commit fails closed.
- [ ] If `TAURI_SIGNING_PRIVATE_KEY` is present, it matches the updater public
      key embedded in the app. The stable release contains exactly the
      installer, its `.sig`, and `latest.json`; the manifest signature matches
      the detached signature, cryptographically verifies against the embedded
      key over this installer, and its URL resolves to this installer.
- [ ] If the updater key is absent, the release is a prerelease containing the
      installer only. There is no `.sig` and no `latest.json`, and the notes
      explicitly say v0.4.4 remains the stable updater.
- [ ] No signature was generated, copied, or fabricated outside the Tauri
      signing path.
- [ ] Updater signing and Authenticode are recorded separately: updater signing
      protects installed-app updates; Authenticode identifies the Windows
      publisher and affects SmartScreen.
