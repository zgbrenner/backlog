# Releasing BackLog

BackLog is built and packaged **locally on a Windows machine** and published as
a **GitHub Release asset**. The NSIS bundle needs Windows, DPAPI and the real
sidecars, so no runner produces it.

Everything that *can* be checked without Windows is checked by
`.github/workflows/ci.yml` on every push — the trust core, the whole Rust
workspace, the frontend and UI harness, the Python sidecar tests, the manifest
contract, and the version agreement between `package.json`,
`tauri.conf.json` and `Cargo.toml`. **Do not cut a release from a commit whose
CI run is not green;** it is the only thing standing between "it compiled on my
machine" and a signed installer.

> If a change touches Rust, the frontend, or the sidecar, you must rebuild
> locally and upload a fresh installer. CI does not produce one.

There is **one** publish procedure, below under "Cutting a release". It always
attaches three assets — installer, `.sig`, `latest.json` — because attaching
fewer breaks the update channel silently for everyone already installed.

## One-time prerequisites (build machine, Windows x64)

- **Rust** — the channel pinned in `rust-toolchain.toml` (rustup installs it
  automatically, with `rustfmt` and `clippy`) plus the
  [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/) (MSVC build
  tools, WebView2 — preinstalled on Windows 11).
- **A Windows-native Perl + NASM** for the vendored OpenSSL build that backs
  `rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature (the ledger is
  SQLCipher-encrypted at rest — see `src-tauri/src/dbkey.rs`). MSYS/Git-Bash's
  `perl` gets picked up first if it's earlier on `PATH` and the OpenSSL build
  fails cryptically; put a native perl (e.g. [Strawberry
  Perl](https://strawberryperl.com/)) and `nasm` ahead of it for the build:
  ```bash
  export PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH"
  ```
  This first build compiles OpenSSL from source and takes several minutes;
  subsequent builds are incremental and fast.
- **Node 22** (the version CI and `package-lock.json` are resolved against).
- **Python 3.11** for the sidecar. The ML dependencies (onnxruntime, rapidocr)
  have **no 3.13/3.14 wheels**, so 3.11 is required even if your default
  Python is newer. This is the slim, torch-free sidecar profile -- no torch,
  transformers, sentence-transformers, or gliclass; see
  `docs/DEPENDENCY_COMPATIBILITY.md`. If you don't have 3.11 system-wide, use
  a userspace standalone via [`uv`](https://docs.astral.sh/uv/) (no admin):
  ```powershell
  uv python install 3.11
  ```
- **A `llama-server.exe`** from an llama.cpp release (see step 2).

## Build steps

### 1. Build the `convertd` sidecar
```powershell
# From the repo root. Produces src-tauri\binaries\convertd-x86_64-pc-windows-msvc.exe
pwsh scripts\build-sidecar.ps1
```
This creates an isolated Python 3.11 venv, installs the pinned `sidecar/requirements.txt`,
freezes the resolved set to `sidecar/requirements.lock` (the reproducible lock),
runs PyInstaller, smoke-tests the binary (`{"id":1,"op":"ping"}` → `{"ok":true}`),
copies it into `src-tauri/binaries/`, and prints its SHA-256.

### 2. Stage `llama-server` (and its DLLs)
Download a prebuilt Windows x64 CPU build of `llama-server` from the
[llama.cpp releases](https://github.com/ggml-org/llama.cpp/releases). The build
verified for this pilot:

- Release `b10091`, asset `llama-b10091-bin-win-cpu-x64.zip` (~17 MB)
- `https://github.com/ggml-org/llama.cpp/releases/download/b10091/llama-b10091-bin-win-cpu-x64.zip`
- SHA-256 `b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee`

The single `win-cpu-x64` zip contains runtime-dispatched CPU backends (it
auto-selects AVX2/AVX-512/etc. at load), so one asset covers all x64 CPUs.

```powershell
Invoke-WebRequest "https://github.com/ggml-org/llama.cpp/releases/download/b10091/llama-b10091-bin-win-cpu-x64.zip" -OutFile llama.zip
if ((Get-FileHash llama.zip -Algorithm SHA256).Hash -ne "B2D991BDD37258BB51309F50E9FB7A52A16FE662BA71B2CBBBBB9303B47B5DEE") { throw "checksum mismatch" }
Expand-Archive llama.zip -DestinationPath llama-cpu -Force
Copy-Item llama-cpu\llama-server.exe src-tauri\binaries\llama-server-x86_64-pc-windows-msvc.exe
# llama-server.exe is a thin stub — it needs its runtime DLLs beside it:
Copy-Item llama-cpu\*.dll src-tauri\binaries\
```

> **DLL bundling caveat.** Unlike `convertd` (a self-contained PyInstaller
> onefile), `llama-server.exe` loads ~13 DLLs (`llama*.dll`, `ggml*.dll`,
> `mtmd.dll`, `libomp140.x86_64.dll`). Tauri's `externalBin` only bundles the
> single `.exe`, so those DLLs must be shipped **next to the app executable**
> in the installed app. This is configured in `tauri.conf.json` (the llama DLLs
> are declared as `bundle.resources` mapped to the app root). After
> `npm run tauri build`, inspect `src-tauri/target/release/` and confirm the
> DLLs sit alongside `BackLog.exe` and the sidecars; then install once and
> confirm the SLM lane starts (Settings → Start with a model configured).

### 3. Icon
The full platform icon set is committed and generated from the 1024×1024
`src-tauri/icons/icon-source.png`. Nothing to do unless the artwork changes; if
it does, regenerate the whole set rather than editing one size:
```powershell
npm run tauri icon src-tauri\icons\icon-source.png
```

### 4. Verify the binaries before bundling
```powershell
pwsh scripts\verify-binaries.ps1
```
This is the gate between a dev checkout and a shippable one. `externalBin` in
`tauri.conf.json` only checks that a path *exists*, so an installer built over
the zero-byte-era dev stubs bundled clean, installed clean, and reported green —
and failed only when the first document reached the SLM lane on a user's
machine, with no logs. The script refuses stubs (they carry the
`BACKLOG-DEV-STUB-DO-NOT-SHIP` marker written by `scripts/dev-stubs.*`),
zero-byte files, anything that is not a valid PE image, and a leftover
`_placeholder.dll`. Pass the recorded hashes to pin them:

```powershell
pwsh scripts\verify-binaries.ps1 -Expected @{
  "llama-server-x86_64-pc-windows-msvc.exe" = "<SHA-256 from step 2>"
  "convertd-x86_64-pc-windows-msvc.exe"     = "<SHA-256 printed by step 1>"
}
```

### 5. Build the installer
```powershell
npm ci
npm run check               # tsc --noEmit + vite build; fails fast, cheaply
npm run tauri build         # compiles release + bundles the NSIS installer
```
The installer lands in `src-tauri/target/release/bundle/nsis/BackLog_<version>_x64-setup.exe`.
It bundles the app, the two sidecars, `resources/name.gbnf`, the llama runtime
DLLs, and — because `bundle.windows.webviewInstallMode` is `offlineInstaller` —
the WebView2 runtime, so the install needs no network. It installs per-user
(`nsis.installMode: currentUser`), which is what keeps a passive auto-update
from raising a UAC prompt.

It does **not** bundle the ML models — see step 6.

### 6. Models
The models are **not** in the installer. They reach the machine one of two ways:

- **Normally:** the operator presses Settings → **Download models (~2.4 GB)**.
  Resumable, cancellable, SHA-256-verified against `models.lock.json`. Nothing
  for you to do.
- **Air-gapped:** run `python models/download_models.py` on a connected machine
  to fetch the two Qwen GGUFs and produce `models/models.lock.json` (commit the
  lock; the weights stay untracked), then copy the two `.gguf` files into
  `%APPDATA%\ai.sonomos.backlog\models` on the target, or point Settings →
  Browse at wherever you put them.

## Updating later
1. Make the change; if it touches Rust / frontend / sidecar, it needs a rebuild.
2. Confirm CI is green on the commit.
3. Re-run steps 1–5 on a Windows machine (step 1 only if the sidecar changed).
4. Follow "Cutting a release" below to sign and publish.

## Auto-updater: signing key

BackLog self-updates via `tauri-plugin-updater`, checking a `latest.json` file
published to each GitHub Release (`releases/latest/download/latest.json` —
GitHub's "latest release" redirect resolves this to whichever release is
currently latest, which is exactly why every release must carry that asset).
Updates are verified with
a minisign keypair before install: the public half is embedded in
`src-tauri/tauri.conf.json` (`plugins.updater.pubkey`), and the private half
signs each build.

The keypair for this pilot was generated once with:
```powershell
npx tauri signer generate -w C:/Users/zgbre/.backlog-signing/backlog-updater.key --ci -p ""
```
It lives **outside the repo**, at `C:/Users/zgbre/.backlog-signing/` on the
build machine (`backlog-updater.key` + `backlog-updater.key.pub`), was
generated with an **empty password**, and must never be committed. Back it
up somewhere durable (a password manager or encrypted archive) — if it's
lost, no future release can be signed to match the pubkey already embedded in
installed copies of the app, and users would need a fresh (unsigned-chain)
manual install to move to a new keypair.

## Cutting a release

This is the **only** publish procedure. Every release — first pilot build or
tenth patch — attaches three assets: the installer, its `.sig`, and
`latest.json`. There is no "just the installer" path, because the updater
endpoint tracks `releases/latest`: publishing any newer release *without* a
`latest.json` asset makes every installed copy 404 on its update check, and
`src/main.ts`'s `checkForUpdates` swallows that error deliberately (a failed
check must not interrupt a user). The update channel can therefore be dead
fleet-wide with zero signal on any machine. Step 5 below is what catches it.

1. Bump `version` in `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml` **and**
   `package.json` — all three, together. Add the matching `CHANGELOG.md`
   section. Confirm with:
   ```powershell
   node .github/scripts/check-versions.mjs
   ```
   CI runs this too, so a mismatch fails the build rather than shipping an
   update manifest whose version nothing agrees with.
2. Build with the signing key available to the CLI via environment variables
   (PowerShell):
   ```powershell
   $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content C:/Users/zgbre/.backlog-signing/backlog-updater.key -Raw
   # Only needed if the key was generated with a password (this pilot's key has none):
   # $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "<password>"
   npm run tauri build
   ```
   Because `bundle.createUpdaterArtifacts` is `true` in `tauri.conf.json`,
   this run emits a **`.sig` detached signature** for the installer next to
   the NSIS installer under `src-tauri/target/release/bundle/nsis/`.

   > **GOTCHA — `latest.json` is NOT regenerated by the build.** The Tauri
   > CLI writes the `.sig` but leaves any existing
   > `bundle/nsis/latest.json` untouched. If a previous build left one
   > there, it still carries the **previous installer's** signature, so
   > uploading it ships an update manifest whose signature does not match
   > the new installer — installed apps then reject the update as tampered.
   > **You must rebuild `latest.json` from the fresh `.sig` every release**
   > and confirm the embedded signature equals the `.sig` file before
   > uploading. The signature field is the **raw text content** of the
   > `.sig` file (already base64), not a re-encoding of it:
   > ```powershell
   > $sig = Get-Content "src-tauri\target\release\bundle\nsis\BackLog_X.Y.Z_x64-setup.exe.sig" -Raw
   > $manifest = [ordered]@{
   >   version   = "X.Y.Z"
   >   notes     = "See CHANGELOG or commit log."
   >   pub_date  = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
   >   platforms = @{ "windows-x86_64" = @{
   >     signature = $sig.Trim()
   >     url = "https://github.com/zgbrenner/backlog/releases/download/vX.Y.Z/BackLog_X.Y.Z_x64-setup.exe"
   >   } }
   > }
   > $manifest | ConvertTo-Json -Depth 5 | Set-Content "src-tauri\target\release\bundle\nsis\latest.json" -Encoding utf8
   > # Verify BEFORE upload: these two must print the same string.
   > (Get-Content "src-tauri\target\release\bundle\nsis\latest.json" -Raw | ConvertFrom-Json).platforms."windows-x86_64".signature
   > $sig.Trim()
   > ```
3. Upload the installer, its `.sig`, and `latest.json` to the GitHub Release:
   ```powershell
   git tag vX.Y.Z
   git push origin vX.Y.Z
   gh release create vX.Y.Z `
     "src-tauri\target\release\bundle\nsis\BackLog_X.Y.Z_x64-setup.exe" `
     "src-tauri\target\release\bundle\nsis\BackLog_X.Y.Z_x64-setup.exe.sig" `
     "src-tauri\target\release\bundle\nsis\latest.json" `
     --title "BackLog vX.Y.Z" `
     --notes "See CHANGELOG or commit log."
   ```
   The `latest.json` filename on the release **must** be exactly `latest.json`
   so `releases/latest/download/latest.json` resolves to it.
4. **Assert the update channel actually resolves — before you tell anyone.**
   This is the one check that catches a dead fleet-wide update channel, and it
   takes five seconds:
   ```powershell
   $endpoint = "https://github.com/zgbrenner/backlog/releases/latest/download/latest.json"
   $published = Invoke-RestMethod $endpoint          # follows GitHub's /latest redirect
   $expected  = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
   if ($published.version -ne $expected) {
     throw "latest.json says $($published.version), tauri.conf.json says $expected"
   }
   $sig = (Get-Content "src-tauri\target\release\bundle\nsis\BackLog_$expected`_x64-setup.exe.sig" -Raw).Trim()
   if ($published.platforms.'windows-x86_64'.signature -ne $sig) {
     throw "published latest.json carries a signature that is not this build's"
   }
   Invoke-WebRequest -Method Head $published.platforms.'windows-x86_64'.url | Out-Null
   "update channel OK: $($published.version)"
   ```
   A 404 here means no `latest.json` asset resolved. A version mismatch means
   an older release is still the `latest` one, or the manifest was built from a
   stale bump. Either way, fix it now — installed copies will not tell you.
5. Installed copies of BackLog check that endpoint at startup (see
   `src/main.ts`'s `checkForUpdates`), compare the manifest's version against
   their own, and — if newer — verify the manifest's signature against the
   pubkey baked into their own `tauri.conf.json` at the time they were built.
   Only a signature that validates against that pubkey is ever installed, so
   a compromised or malformed release asset is rejected client-side rather
   than silently applied. On accept, the app downloads the installer,
   installs it passively (`plugins.updater.windows.installMode: "passive"`),
   and relaunches into the new version via `@tauri-apps/plugin-process`.
6. Every release published this way must be signed with the **same** private
   key (`C:/Users/zgbre/.backlog-signing/backlog-updater.key`) so its
   signature keeps validating against the pubkey already shipped in prior
   installs. Rotating the key breaks the update chain for everyone on an
   older version until they reinstall manually.
7. Record the installer's SHA-256 in the release notes so downloaders can
   verify it, and complete the "Release evidence" section of
   `docs/RELEASE_CHECKLIST.md`.
