# Releasing BackLog without GitHub Actions

BackLog is built and packaged **locally on a Windows machine** and published as
a **GitHub Release asset**. GitHub Releases are plain file hosting — creating one
and attaching an installer costs **zero GitHub Actions minutes**. Nothing in this
repo runs in CI; if a future change needs compiling or bundling, it must be built
locally before the release is uploaded.

> If you update the app and a change touches Rust, the frontend, or the sidecar,
> you must rebuild locally and upload a fresh installer. There is no automated
> build. Flag any PR that changes those areas as "needs local rebuild before
> release" if you can't rebuild it at merge time.

## One-time prerequisites (build machine, Windows x64)

- **Rust** stable + the [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/)
  (MSVC build tools, WebView2 — preinstalled on Windows 11).
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
- **Node 18+**.
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
`src-tauri/icons/icon.ico` is committed but is a low-res placeholder. For a real
release, generate the full icon set from a 1024×1024 source:
```powershell
npm run tauri icon path\to\icon-1024.png
```

### 4. Build the installer
```powershell
npm install
npm run tauri build         # compiles release + bundles the NSIS installer
```
The installer lands in `src-tauri/target/release/bundle/nsis/BackLog_<version>_x64-setup.exe`
(and/or the MSI under `bundle/msi/`). It bundles the app, the two sidecars, and
`resources/name.gbnf`. It does **not** bundle the ML models (they're large and
gitignored) — see step 5.

### 5. Models (shipped or first-run)
Run `python models/download_models.py` on an internet-connected machine to fetch
the two Qwen GGUFs and produce `models/models.lock.json` (commit the lock;
the weights stay untracked). Either ship the `models/` folder alongside the
installer, or have the operator run the download once (or use the in-app
downloader in Settings, which fetches the same bundle). The app's Settings tab
points at the model paths.

## Publish the release (no Actions minutes)

```powershell
# Tag the exact commit you built from, then attach the artifacts.
git tag v0.1.0
git push origin v0.1.0
gh release create v0.1.0 `
  "src-tauri\target\release\bundle\nsis\BackLog_0.1.0_x64-setup.exe" `
  --title "BackLog v0.1.0" `
  --notes "First pilot build. Built locally; see RELEASING.md."
```
`gh release create` uploads the files as release assets over the API — it does
**not** trigger a workflow. Users download the installer from the repo's Releases
page. Record the installer's SHA-256 in the release notes so downloaders can
verify it.

## Updating later
1. Make the change; if it touches Rust / frontend / sidecar, it needs a rebuild.
2. Re-run steps 1–4 on a Windows machine (step 1 only if the sidecar changed).
3. Follow "Cutting an updating release" below to build, sign, and publish.

## Auto-updater: signing key

BackLog self-updates via `tauri-plugin-updater`, checking a `latest.json` file
published to each GitHub Release (`releases/latest/download/latest.json` —
GitHub's "latest release" redirect resolves this with **zero Actions
minutes**, since it's just release-asset hosting). Updates are verified with
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

## Cutting an updating release

1. Bump `version` in `src-tauri/tauri.conf.json` (and `src-tauri/Cargo.toml`
   if you keep them in lockstep).
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
4. Installed copies of BackLog check that endpoint at startup (see
   `src/main.ts`'s `checkForUpdates`), compare the manifest's version against
   their own, and — if newer — verify the manifest's signature against the
   pubkey baked into their own `tauri.conf.json` at the time they were built.
   Only a signature that validates against that pubkey is ever installed, so
   a compromised or malformed release asset is rejected client-side rather
   than silently applied. On accept, the app downloads the installer,
   installs it passively (`plugins.updater.windows.installMode: "passive"`),
   and relaunches into the new version via `@tauri-apps/plugin-process`.
5. Every release published this way must be signed with the **same** private
   key (`C:/Users/zgbre/.backlog-signing/backlog-updater.key`) so its
   signature keeps validating against the pubkey already shipped in prior
   installs. Rotating the key breaks the update chain for everyone on an
   older version until they reinstall manually.
