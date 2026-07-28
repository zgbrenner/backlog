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
    "convertd-x86_64-pc-windows-msvc.exe",
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
Write-Host "Dev stub sidecar binaries created in src-tauri/binaries/." -ForegroundColor Green
Write-Host "They carry the marker '$StubMarker' and will fail scripts/verify-binaries.ps1."
Write-Host "Real binaries: scripts/build-sidecar.ps1 (convertd) + RELEASING.md (llama-server)."
