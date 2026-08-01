# Releasing BackLog

BackLog v0.7.0 is built on a clean `windows-2022` GitHub-hosted runner by
`.github/workflows/release.yml`. This public repository uses standard public
Actions runners, which do not consume billable minutes.

The release produces two Windows x64 downloads:

- `BackLog_0.7.0_x64-setup.exe`
- `BackLog_0.7.0_x64-portable.zip`

Both contain the app, `convertd` and its Python runtime, `llama-server` and its
runtime DLLs, the verified Qwen3 0.6B primary model, and the pinned MiniLM
semantic model/tokenizer. The installer carries the offline WebView2 installer;
the portable ZIP carries a pinned fixed WebView2 runtime and launches through
`BackLog-Portable.cmd`. The Qwen3 1.7B escalation model remains an optional
in-app download.

## Required release state

Before merging or pushing the release commit to `main`:

1. `package.json`, `src-tauri/Cargo.toml`,
   `src-tauri/tauri.conf.json`, `package-lock.json`, and the root `backlog`
   package in `src-tauri/Cargo.lock` must say `0.7.0`.
2. `CHANGELOG.md` must contain the matching section.
3. CI must be green on `main`.
4. `v0.7.0` must not already be published. The workflow preflight skips a
   completed release and can resume only an interrupted draft whose tag still
   points at the exact CI-tested commit.
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

`npm run check:release` runs the behavioral signed/unsigned artifact tests and
validates the workflow structure.

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

## Signing modes

The workflow selects exactly one mode from the presence of the GitHub secret
`TAURI_SIGNING_PRIVATE_KEY`.

### Signed: stable updater release

Configure:

- `TAURI_SIGNING_PRIVATE_KEY` — the private key matching the updater public key
  embedded in `src-tauri/tauri.conf.json`;
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — optional; set it only when the key has
  a password.

The workflow builds with updater artifacts enabled, requires:

- `BackLog_0.7.0_x64-setup.exe`;
- `BackLog_0.7.0_x64-portable.zip`;
- `BackLog_0.7.0_x64-setup.exe.sig`; and
- `latest.json`;

and verifies that `latest.json` carries the exact detached signature and points
to the same installer. It also verifies and uploads the portable ZIP. It then
decodes the updater public key embedded in
`src-tauri/tauri.conf.json` and cryptographically verifies that signature over
the exact installer. Only then does it create `v0.7.0` as a draft, upload all
four files, and publish the stable GitHub release.

Never rotate this updater key casually. Existing installations verify updates
against the public key they already contain; losing or replacing its private
half breaks that update chain.

### No updater key: unsigned prerelease

When `TAURI_SIGNING_PRIVATE_KEY` is absent or blank, the workflow overlays
`scripts/tauri.unsigned.conf.json`, which disables updater artifact generation.
It then requires the installer to exist and requires both the signature and
`latest.json` to be absent.

The workflow still creates `v0.7.0`, but publishes an explicit prerelease with
the installer and portable ZIP. The release notes say that v0.4.4 remains the stable
updater. GitHub's `releases/latest` endpoint therefore continues to resolve to
the prior stable release.

Never synthesize a signature, upload a blank signature, or publish unsigned
`latest.json`. An installed BackLog cannot trust any of those states.

## Trigger and clean skip

After the prepared release commit reaches `main`, a successful **CI** run
automatically starts the release workflow for that exact commit. Failed CI
does not allocate the Windows release build. There is intentionally no manual
dispatch path that can bypass that exact-commit CI result.

The first job derives the release tag from validated package metadata, then
checks the exact remote tag and GitHub Release state before
allocating the Windows runner. A published `v0.7.0` exits successfully. If an
asset upload was interrupted, use **Re-run all jobs** on that same failed
release-workflow run. A matching draft resumes with `--clobber`; a tag or draft
pointing at any other commit fails closed. Before publication, the workflow
also requires the remote draft to contain exactly the assets allowed by the
selected signing mode and its durable release title to identify that mode. A
signed draft therefore cannot be downgraded to an unsigned prerelease if
signing credentials disappear on a retry. The tag is resolved back to the
exact CI-tested commit before any retry mutation and again immediately before
publication.

It performs:

1. successful-main-CI and release-state preflight;
2. checkout with tag history;
3. pinned toolchain and lockfile installation;
4. release-contract, frontend, and Power Automate validation;
5. exact release-input and fixed-WebView2 download/hash verification;
6. sidecar build and full binary/import verification;
7. signed or explicitly unsigned Tauri build;
8. installer-free portable ZIP build and post-compression validation;
9. guarded artifact-set verification;
10. cryptographic updater verification when signing is enabled; and
11. draft upload followed by stable or prerelease publication.

Attach the workflow URL and the installer SHA-256 (written into release notes)
to the release evidence.

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
./scripts/package-portable.ps1 -Version 0.7.0 -WebView2RuntimeDir $webview2
./scripts/validate-portable-package.ps1 `
  -Archive "src-tauri/target/release/BackLog_0.7.0_x64-portable.zip" `
  -Version 0.7.0
```

This reproduces packaging but does not authorize publication. Use the guarded
workflow to create the tag and release.

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
