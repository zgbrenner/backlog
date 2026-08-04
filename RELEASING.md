# Releasing BackLog

BackLog v0.8.0 is built on a clean `windows-2022` GitHub-hosted runner by
`.github/workflows/release.yml`. This public repository uses standard public
Actions runners, which do not consume billable minutes.

The release produces two Windows x64 downloads:

- `BackLog_0.8.0_x64-setup.exe`
- `BackLog_0.8.0_x64-portable.zip`

Both contain the app, `convertd` and its Python runtime, `llama-server` and its
runtime DLLs, the verified Qwen3 0.6B primary model, and the pinned MiniLM
semantic model/tokenizer. The installer carries the offline WebView2 installer;
the portable ZIP carries a pinned fixed WebView2 runtime and launches through
`BackLog-Portable.cmd`. The Qwen3 1.7B escalation model remains an optional
in-app download.

## Delivery-mode release gates

v0.8.0 preserves **Power Automate / SharePoint** as the default manifest
handoff to Outbox for Flow 2 and adds **Local folder** direct delivery. Record
fresh Local Output evidence: the Processing, Local Output, and Quarantine roots
are distinct and non-nested; each completed delivery has its renamed output
and `.backlog/receipts/<manifest_id>.json`; and collision, restart/recovery,
correction, and dismissal reconcile as specified in `docs/PILOT_RUNBOOK.md`.

These local gates do not certify a target Power Automate tenant. Flow 1/Flow 2,
SharePoint indexing/archive, connector permissions, throttling, and recovery
need separate tenant evidence. Do not state that they passed without it.

## Required release state

Before merging or pushing a normal release commit to `main`:

1. `package.json`, `src-tauri/Cargo.toml`,
   `src-tauri/tauri.conf.json`, `package-lock.json`, and the root `backlog`
   package in `src-tauri/Cargo.lock` must carry the same version.
2. `CHANGELOG.md` must contain the matching section.
3. CI must be green on `main`.
4. The target tag must not already be published. The normal workflow can resume
   only an interrupted draft whose tag still points at the exact CI-tested
   commit. The already-published v0.8.0 prerelease is handled only by the
   dedicated immutable-tag repair workflow described below.
5. Complete the source and security portions of
   `docs/RELEASE_CHECKLIST.md`.

Run the local gates before pushing:

```bash
node .github/scripts/check-versions.mjs
node .github/scripts/check-ci-parity.mjs
npm run check:release
npm run check
python power-automate/validate_examples.py
```

`npm run check:release` validates the signed artifact contract, rejects an
unsigned publication fallback, and checks the workflow structure.

## Immutable build inputs

The workflow uses:

| Input | Pin |
|---|---|
| Runner | `windows-2022` |
| Node | 22 |
| Python | 3.11 x64 |
| Rust | `rust-toolchain.toml` (`1.94`) |
| JavaScript | `package-lock.json` via `npm ci` |
| Rust | `src-tauri/Cargo.lock` via Tauri/Cargo |
| Python sidecar | `sidecar/requirements.lock` |
| Sidecar freezer | `sidecar/build-requirements.lock` |
| Release actions | Full reviewed commit SHAs in `.github/workflows/release.yml` |
| Primary model | Qwen3 0.6B Q8_0, SHA-256 `9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031` |
| Semantic model | Xenova all-MiniLM-L6-v2 quantized ONNX, SHA-256 `afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1`; tokenizer SHA-256 `07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3` |
| llama.cpp | `b10091` Windows CPU x64 ZIP, SHA-256 `b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee` |
| `llama-server.exe` | SHA-256 `78af9cfb34f346b0de1e4f9c1577061cb3d55e8be55c8d540fde878e56bd0fe2` |
| Fixed WebView2 runtime | `151.0.4129.59` CAB, SHA-256 `056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc`, 304,114,944 bytes |

`scripts/stage-release-inputs.ps1` downloads the exact primary and semantic
models plus the llama.cpp archive, verifies each before staging, rejects
`BACKLOG-DEV-STUB-DO-NOT-SHIP`, and copies the VC143 runtime files from the
runner's installed MSVC toolchain. `scripts/verify-binaries.ps1` then checks PE
structure, recorded hashes, and every imported DLL before Tauri runs.

The Python sidecar is built with Python 3.11 and smoke-tested against real
DOCX, PDF, image, and semantic evidence fixtures by `scripts/build-sidecar.ps1`.

## Signing policy

Stable BackLog releases require the existing Tauri updater signing key.
Configure `TAURI_SIGNING_PRIVATE_KEY` with the private key matching the
updater public key embedded in `src-tauri/tauri.conf.json`; set
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` only when that key is password protected.

The release workflow fails closed when the key is absent. It does not publish
an unsigned fallback, synthesize a signature, or rotate the updater key. A
signed release must contain the installer, portable ZIP, detached `.sig`, and
`latest.json`. The workflow verifies the detached signature against the public
key embedded in the exact app build before publication.

Never rotate this updater key casually. Existing installations verify updates
against the public key they already contain; losing or replacing its private
half breaks that update chain.

### One-time v0.8.0 repair

`.github/workflows/repair-v0.8.0.yml` repairs the already-published unsigned
v0.8.0 prerelease. It checks out the immutable `v0.8.0` tag at
`74e31fbd2b31ad99ceaf5390bb27fb197fc706a7`, rebuilds and signs those exact
sources, verifies the signature against the tagged app's embedded public key,
replaces the two existing downloads, adds the `.sig` and `latest.json`, and
promotes the same release to stable Latest. It never moves or recreates the
tag. A missing or mismatched private key leaves the prerelease unchanged.

Tauri updater signing and Windows Authenticode remain separate trust
boundaries. BackLog does not yet have a trusted Authenticode certificate, so
Windows SmartScreen can still warn on a correctly updater-signed installer.

## Trigger and clean skip

After a prepared release commit reaches `main`, the normal release workflow
starts from that exact push and waits for a successful **CI** run for the same
commit. Failed CI does not allocate the Windows release build. There is
intentionally no manual dispatch path that can bypass that exact-commit CI
result.

The first job derives the release tag from validated package metadata, then
checks the exact remote tag and GitHub Release state before allocating the
Windows runner. A published tag exits successfully. If an asset upload was
interrupted, use **Re-run all jobs** on that same failed workflow run. A matching
draft resumes with `--clobber`; a tag or draft pointing at any other commit
fails closed. Before publication, the workflow requires the remote draft to
contain exactly the four signed assets and verifies the tag against the exact
CI-tested commit before any retry mutation and again immediately before
publication.

It performs:

1. successful-main-CI and release-state preflight;
2. checkout with tag history;
3. pinned toolchain and lockfile installation;
4. release-contract, frontend, and Power Automate validation;
5. exact release-input and fixed-WebView2 download/hash verification;
6. sidecar build and full binary/import verification;
7. signed Tauri installer build;
8. installer-free portable ZIP build and post-compression validation;
9. guarded four-asset verification;
10. cryptographic updater verification against the embedded public key; and
11. draft upload followed by stable publication.

Attach the workflow URL and the installer SHA-256 written into the release
notes to the release evidence.

## Reproducing packaging on Windows

For diagnosis, a clean Windows 11/Server 2022 machine with Node 22, Python 3.11,
the pinned Rust toolchain, MSVC Build Tools, native Perl, and NASM can run:

```powershell
npm ci
npm run check:release
npm run check
python -m pip install -r power-automate/requirements-dev.txt
python power-automate/validate_examples.py
./scripts/stage-release-inputs.ps1 -Clean
./scripts/build-sidecar.ps1 -Python (Get-Command python).Source -Clean
./scripts/verify-binaries.ps1 -Expected @{
  "llama-server-x86_64-pc-windows-msvc.exe" = "78af9cfb34f346b0de1e4f9c1577061cb3d55e8be55c8d540fde878e56bd0fe2"
}
npm run tauri build
$webview2 = Join-Path $env:TEMP "backlog-webview2-fixed"
./scripts/stage-webview2-runtime.ps1 -Destination $webview2 -Clean
./scripts/package-portable.ps1 -Version 0.8.0 -WebView2RuntimeDir $webview2
./scripts/validate-portable-package.ps1 `
  -Archive "src-tauri/target/release/BackLog_0.8.0_x64-portable.zip" `
  -Version 0.8.0
```

This reproduces packaging but does not authorize publication. Use the guarded
workflow to create a new release, or the dedicated repair workflow for the
existing immutable v0.8.0 tag.

## Two separate trust boundaries

Tauri updater signing and Windows Authenticode solve different problems:

- **Updater signing** proves to an installed copy that an update matches the
  private key paired with its embedded updater public key. A stable release
  requires this key.
- **Authenticode** identifies the Windows publisher and contributes to
  SmartScreen reputation for a manually downloaded installer.

BackLog does not yet have a trusted Authenticode certificate. A correctly
updater-signed stable build can therefore still show a SmartScreen warning on
manual installation, and managed fleets may still block it. Do not describe
the updater signature as Windows code signing.
