<#
.SYNOPSIS
  Create empty placeholder sidecar binaries so the full workspace build/tests
  run on a fresh checkout that hasn't produced the real sidecars yet.

.DESCRIPTION
  tauri-build validates the externalBin sidecars and the bundle.resources DLL
  glob at compile time. On a fresh clone those files are absent (they are
  gitignored — built by scripts/build-sidecar.ps1 / downloaded per RELEASING.md),
  so `cargo build` / `cargo test --workspace` / `cargo clippy` fail before
  compiling. This stages zero-byte placeholders to unblock local development.

  The deterministic TRUST CORE needs none of this:  cargo test -p backlog-core

  Remove these before a real release build (the release flow stages the actual
  binaries): Remove-Item src-tauri/binaries/*
#>
$ErrorActionPreference = "Stop"
$bin = Join-Path $PSScriptRoot "..\src-tauri\binaries"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
foreach ($f in @(
    "convertd-x86_64-pc-windows-msvc.exe",
    "llama-server-x86_64-pc-windows-msvc.exe",
    "_placeholder.dll"
)) {
    $p = Join-Path $bin $f
    if (-not (Test-Path $p)) { New-Item -ItemType File -Path $p | Out-Null }
}
Write-Host "Dev stub sidecar binaries created in src-tauri/binaries/." -ForegroundColor Green
Write-Host "Real binaries: scripts/build-sidecar.ps1 (convertd) + RELEASING.md (llama-server)."
