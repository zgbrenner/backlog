<#
.SYNOPSIS
  Refuse to package a release whose sidecars are dev stubs, truncated builds,
  or the wrong file.

.DESCRIPTION
  tauri.conf.json's externalBin only checks that a path EXISTS, and preflight
  only checks that the file is non-empty and answers a probe on the user's
  machine — not on the build machine. So the installer builds clean, installs
  clean, and reports green with a placeholder inside it; the failure surfaces
  when the first real document reaches the SLM lane on a user's computer,
  where there are no build logs to explain it.

  Run this on the Windows release machine AFTER staging the real binaries
  (RELEASING.md Build steps 1-2) and BEFORE `npm run tauri build`. It asserts, for
  every required artifact:

    * the file exists and is non-empty;
    * it does not carry the scripts/dev-stubs.* marker;
    * it starts with the "MZ" DOS header and its PE header resolves (a
      truncated or text file fails here);
    * `_placeholder.dll` — the dev-only filler that exists purely so the
      bundle.resources *.dll glob is non-empty — is absent;
    * its SHA-256 matches -Expected, when supplied.

.PARAMETER Expected
  Optional map of file name -> SHA-256, e.g. the hashes recorded in the
  release evidence per RELEASING.md Build step 2 and docs/RELEASE_CHECKLIST.md:

      pwsh scripts/verify-binaries.ps1 -Expected @{
        "llama-server-x86_64-pc-windows-msvc.exe" = "B2D9...DEE"
      }

  Names not present in the map are checked for shape only, and reported as
  unpinned so the omission is visible rather than silent.

.EXAMPLE
  pwsh scripts/verify-binaries.ps1
#>
[CmdletBinding()]
param(
    [hashtable] $Expected = @{},
    [string] $BinDir = ""
)

$ErrorActionPreference = "Stop"

# Resolved here rather than as a param() default because Windows PowerShell 5.1
# binds parameters before $PSScriptRoot is populated, so the default expression
# saw an empty string and the script died on its own first line with
# "Cannot bind argument to parameter 'Path'". pwsh 7 populates it earlier and
# the param-block form worked there -- which is how a gate documented as
# `pwsh scripts/verify-binaries.ps1` came to be unrunnable on the shell that
# ships with Windows.
if (-not $BinDir) { $BinDir = Join-Path $PSScriptRoot "..\src-tauri\binaries" }

# Kept byte-identical in scripts/dev-stubs.sh and scripts/dev-stubs.ps1.
$StubMarker = "BACKLOG-DEV-STUB-DO-NOT-SHIP"

# The two externalBin sidecars, by the exact target-triple names tauri-build
# resolves. The llama runtime DLLs are checked as a group instead, since their
# names move with the llama.cpp release.
$RequiredBinaries = @(
    "convertd-x86_64-pc-windows-msvc.exe",
    "llama-server-x86_64-pc-windows-msvc.exe"
)

$failures = [System.Collections.Generic.List[string]]::new()
$notes = [System.Collections.Generic.List[string]]::new()

function Test-IsPortableExecutable {
    param([string] $Path)
    # 64 bytes is the minimum DOS header; e_lfanew lives at offset 0x3C and
    # points at the 4-byte "PE\0\0" signature.
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64) { return $false }
    if ($bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) { return $false }  # "MZ"
    $peOffset = [System.BitConverter]::ToInt32($bytes, 0x3C)
    if ($peOffset -le 0 -or ($peOffset + 4) -gt $bytes.Length) { return $false }
    return ($bytes[$peOffset] -eq 0x50 -and        # "P"
            $bytes[$peOffset + 1] -eq 0x45 -and    # "E"
            $bytes[$peOffset + 2] -eq 0x00 -and
            $bytes[$peOffset + 3] -eq 0x00)
}

function Test-CarriesStubMarker {
    param([string] $Path)
    $length = (Get-Item $Path).Length
    if ($length -gt 4096) { return $false }   # a stub is tiny by construction
    $text = [System.IO.File]::ReadAllText($Path)
    return $text.Contains($StubMarker)
}

if (-not (Test-Path $BinDir)) {
    Write-Error "binaries directory not found: $BinDir. Run RELEASING.md Build steps 1-2 first."
}
$BinDir = (Resolve-Path $BinDir).Path

foreach ($name in $RequiredBinaries) {
    $path = Join-Path $BinDir $name
    if (-not (Test-Path $path)) {
        $failures.Add("$name is missing. See RELEASING.md Build steps 1-2.")
        continue
    }
    $length = (Get-Item $path).Length
    if ($length -eq 0) {
        $failures.Add("$name is zero bytes.")
        continue
    }
    if (Test-CarriesStubMarker $path) {
        $failures.Add("$name is a dev stub from scripts/dev-stubs.*, not a real binary.")
        continue
    }
    if (-not (Test-IsPortableExecutable $path)) {
        $failures.Add("$name is not a valid PE image (no MZ/PE header) - truncated or wrong file.")
        continue
    }

    $hash = (Get-FileHash $path -Algorithm SHA256).Hash
    if ($Expected.ContainsKey($name)) {
        if ($hash -ne $Expected[$name].ToUpperInvariant()) {
            $failures.Add("$name SHA-256 $hash does not match the recorded $($Expected[$name]).")
            continue
        }
        Write-Host "ok    $name  ($length bytes, SHA-256 pinned)" -ForegroundColor Green
    }
    else {
        $notes.Add("$name is not hash-pinned. Record $hash in the release evidence.")
        Write-Host "ok    $name  ($length bytes, SHA-256 $hash)" -ForegroundColor Green
    }
}

# llama-server.exe is a thin stub that loads ~13 runtime DLLs; without them the
# SLM lane fails on the user's machine with a message that points at the model.
$dlls = @(Get-ChildItem -Path $BinDir -Filter "*.dll" -File -ErrorAction SilentlyContinue)
$placeholder = $dlls | Where-Object { $_.Name -eq "_placeholder.dll" }
if ($placeholder) {
    $failures.Add("_placeholder.dll is still present. Delete it; it exists only so the dev bundle.resources *.dll glob is non-empty.")
}
$realDlls = $dlls | Where-Object { $_.Name -ne "_placeholder.dll" }
if ($realDlls.Count -eq 0) {
    $failures.Add("no llama runtime DLLs in $BinDir. Copy llama-cpu\*.dll there (RELEASING.md Build step 2).")
}
else {
    foreach ($dll in $realDlls) {
        if (Test-CarriesStubMarker $dll.FullName) {
            $failures.Add("$($dll.Name) is a dev stub, not a real DLL.")
        }
        elseif (-not (Test-IsPortableExecutable $dll.FullName)) {
            $failures.Add("$($dll.Name) is not a valid PE image.")
        }
    }
    Write-Host "ok    $($realDlls.Count) llama runtime DLL(s)" -ForegroundColor Green
}

foreach ($note in $notes) { Write-Host "note  $note" -ForegroundColor Yellow }

if ($failures.Count -gt 0) {
    Write-Host ""
    Write-Host "Release binaries are NOT ready:" -ForegroundColor Red
    foreach ($failure in $failures) { Write-Host "  - $failure" -ForegroundColor Red }
    exit 1
}

Write-Host ""
Write-Host "All release binaries verified in $BinDir." -ForegroundColor Green
exit 0
