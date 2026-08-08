# BackLog v0.8.0 release checklist

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
- [ ] `node .github/scripts/check-versions.mjs` reports 0.8.0, the `backlog`
      package in `src-tauri/Cargo.lock` is 0.8.0, and `CHANGELOG.md` has the
      0.8.0 section.

## One-download package

- [ ] The installer download is `BackLog_0.8.0_x64-setup.exe`; the
      installer-free option is `BackLog_0.8.0_x64-portable.zip`.
- [ ] The installer contains the app, `convertd` with its Python runtime,
      `llama-server` and every imported runtime DLL, the bundled Qwen3 0.6B Q8_0, the
      pinned MiniLM semantic model/tokenizer, and the offline WebView2 runtime.
- [ ] The bundled model hash is
      `9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031`
      (Qwen3-0.6B-Q8_0). This is the one GGUF in the installer and the
      configured primary on every RAM tier.
- [ ] Qwen3-1.7B-Q8_0 is not in the installer or the portable ZIP — including
      it would take each over GitHub's 2 GiB per-release-asset limit. Settings
      presents it as a cancellable, resumable in-app download.
- [ ] A machine above 9 GiB of RAM that has not run that download still names
      documents and readiness shows the
      `escalation_model_missing_using_primary` warning rather than a blocking
      error.
- [ ] A clean install can process a document with the network disabled and
      without Python, a VC++ redistributable, a model script, or any second
      installer.
- [ ] A complete portable extraction launches through `BackLog-Portable.cmd`
      with its bundled fixed WebView2 runtime and no separate runtime download;
      the launcher rejects UNC paths and applies the Windows 10 AppContainer
      read/execute ACLs.
- [ ] First launch moves the bundled primary model into the per-user model
      directory without changing an operator's custom absolute model path.

## Windows packaging evidence

- [ ] The release workflow ran on `windows-2022` with Node 22, Python 3.11 x64,
      the toolchain from `rust-toolchain.toml`, and the hash-verified NASM
      2.16.03 archive.
- [ ] `npm ci`, Cargo `--locked` resolution, and
      `sidecar/requirements.lock` were used.
- [ ] `scripts/stage-release-inputs.ps1` verified the primary and semantic
      model assets plus the `llama.cpp b10091` archive before staging.
- [ ] `scripts/stage-webview2-runtime.ps1` verified WebView2 151.0.4129.59,
      SHA-256 `056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc`,
      and all 256 runtime files before portable packaging.
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
- [ ] With the optional escalation model absent, readiness says it is absent,
      stays usable, and safely uses the primary model for backup naming
      attempts. Same expectation on a machine at 9 GiB or less, where the
      collapsed pair is the configured shape rather than a missing file.
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

## Local Output direct delivery

- [ ] **Local folder** is available in Settings alongside the default
      **Power Automate / SharePoint** mode. In Local mode, Processing, Local
      Output, and Quarantine are distinct, non-nested safe roots; Outbox is not
      required.
- [ ] An ordinary Local delivery produces the finished renamed document in
      Local Output and exactly one receipt at
      `.backlog/receipts/<manifest_id>.json`. It does not write an Outbox
      manifest, SharePoint index, or cloud archive.
- [ ] Duplicate physical copies and an unrelated preexisting output collision
      retain every existing file and use the deterministic no-overwrite suffix.
- [ ] Restart/fault recovery reconciles every Local source with its output and
      receipt. A corrected flagged file moves directly from Quarantine to Local
      Output; a dismissed file remains in Quarantine with its review receipt
      and no delivered output path.
- [ ] Needs Review wording follows each job's immutable delivery mode after a
      Settings switch; a Local-pinned job never becomes a Power Automate
      handoff, and vice versa.

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
- [ ] These are target-tenant gates, not claims made by the Local Output
      release checks. Record Flow 1/Flow 2 connector permissions, manifest
      pickup, SharePoint indexing/archive, throttling, and checkpoint-recovery
      evidence separately.

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

- [ ] `v0.8.0` remains permanently attached to commit
      `74e31fbd2b31ad99ceaf5390bb27fb197fc706a7`; the repair workflow never
      moves, deletes, or recreates the tag.
- [ ] `TAURI_SIGNING_PRIVATE_KEY` is present and matches the updater public
      key embedded in the tagged app. A missing or mismatched key fails the
      workflow without changing the published release.
- [ ] The stable release contains exactly these four downloadable assets:
      `BackLog_0.8.0_x64-setup.exe`,
      `BackLog_0.8.0_x64-portable.zip`,
      `BackLog_0.8.0_x64-setup.exe.sig`, and `latest.json`.
- [ ] The detached signature matches `latest.json`, cryptographically
      verifies against the updater public key embedded in the tagged app,
      and covers the exact published installer bytes.
- [ ] The installer and portable ZIP SHA-256 values are included in the
      release notes and match freshly downloaded public assets.
- [ ] The release is neither a draft nor a prerelease, is marked Latest,
      and `releases/latest/download/latest.json` resolves to this release.
- [ ] No signature was generated, copied, or fabricated outside the Tauri
      signing path.
- [ ] Updater signing and Authenticode are recorded separately: updater
      signing protects installed-app updates; Authenticode identifies the
      Windows publisher and affects SmartScreen.
