<#
.SYNOPSIS
  Create placeholder sidecar binaries so the full workspace build/tests run on
  a fresh checkout that hasn't produced the real sidecars yet.

.DESCRIPTION
  tauri-build validates the externalBin sidecars and the bundle.resources DLL
  glob at compile time. On a fresh clone those files are absent (they are
  gitignored — built by scripts/build-sidecar.ps1 / downloaded per RELEASING.md),
  so `cargo build` / `cargo test --workspace` / `cargo clippy` fail before
  compiling. This stages marked placeholders to unblock local development.

  The stubs are NOT zero bytes. A zero-byte file is indistinguishable from a
  truncated real build, and the realistic accident — stub to get tests green,
  cut a frontend-only release, skip the sidecar rebuild — then produced an
  installer that built clean, installed clean, and failed only when the first
  document reached the SLM lane on a user's machine, with no logs. Each stub
  carries the marker below, which scripts/verify-binaries.ps1 refuses.

  The deterministic TRUST CORE needs none of this:  cargo test -p backlog-core

  Before a real release build, stage the real binaries and then run:
      pwsh scripts/verify-binaries.ps1
#>
$ErrorActionPreference = "Stop"

# Kept byte-identical in scripts/dev-stubs.sh and scripts/verify-binaries.ps1.
# Deliberately not "MZ"-prefixed, so a stub also fails a plain PE-magic test.
$StubMarker = "BACKLOG-DEV-STUB-DO-NOT-SHIP"

$bin = Join-Path $PSScriptRoot "..\src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
foreach ($f in @(
    "llama-server-x86_64-pc-windows-msvc.exe",
    "_placeholder.dll"
)) {
    $p = Join-Path $bin $f
    # Never overwrite a real binary; only create one, or upgrade a legacy
    # zero-byte stub so verify-binaries.ps1 can name it as a stub rather than
    # as a truncated build.
    if ((-not (Test-Path $p)) -or ((Get-Item $p).Length -eq 0)) {
        Set-Content -Path $p -Value $StubMarker -Encoding ascii -NoNewline
    }
}

# convertd ships as a bundle.resources directory tree (PyInstaller --onedir
# output), not a triple-suffixed externalBin exe, so tauri-build's
# "binaries/convertd/": "convertd/" glob just needs the directory to exist
# with at least one file in it -- a single stubbed convertd.exe is enough to
# unblock a build. It is deliberately not staged with a placeholder
# _internal/, which real code never reads before spawning the process.
$convertdDir = Join-Path $bin "convertd"
New-Item -ItemType Directory -Force -Path $convertdDir | Out-Null
$convertdExe = Join-Path $convertdDir "convertd.exe"
if ((-not (Test-Path $convertdExe)) -or ((Get-Item $convertdExe).Length -eq 0)) {
    Set-Content -Path $convertdExe -Value $StubMarker -Encoding ascii -NoNewline
}

# tauri-build also validates the bundled model glob. Keep this in sync with
# scripts/dev-stubs.sh so a fresh Windows checkout can run the same workspace
# tests as CI without downloading model weights. The release staging script
# replaces this marker with the hash-verified model before packaging.
$model = Join-Path $PSScriptRoot "..\src-tauri\resources\models\Qwen3-0.6B-Q8_0.gguf"
$modelDir = Split-Path -Parent $model
New-Item -ItemType Directory -Force -Path $modelDir | Out-Null
if ((-not (Test-Path $model)) -or ((Get-Item $model).Length -eq 0)) {
    Set-Content -Path $model -Value $StubMarker -Encoding ascii -NoNewline
}

$semanticDir = Join-Path $PSScriptRoot "..\src-tauri\resources\models\semantic\all-MiniLM-L6-v2"
New-Item -ItemType Directory -Force -Path $semanticDir | Out-Null
foreach ($asset in @("model.onnx", "vocab.txt")) {
    $path = Join-Path $semanticDir $asset
    if ((-not (Test-Path $path)) -or ((Get-Item $path).Length -eq 0)) {
        Set-Content -Path $path -Value $StubMarker -Encoding ascii -NoNewline
    }
}

Write-Host "Dev stub sidecar binaries created in src-tauri/binaries/." -ForegroundColor Green
Write-Host "They carry the marker '$StubMarker' and will fail scripts/verify-binaries.ps1."
Write-Host "The marker model was staged in src-tauri/resources/models/ and will fail release verification."
Write-Host "Real binaries: scripts/build-sidecar.ps1 (convertd) + RELEASING.md (llama-server)."
