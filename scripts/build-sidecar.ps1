<#
.SYNOPSIS
  Build the convertd Python sidecar into a single Windows executable for Tauri.

.DESCRIPTION
  Creates an isolated Python 3.11 venv (the ML deps have no 3.13/3.14 wheels),
  installs the pinned requirements, freezes the resolved set to
  sidecar/requirements.lock, runs PyInstaller, smoke-tests the binary, and copies
  it to src-tauri/binaries/ with the target-triple suffix Tauri expects.

  Python 3.11 is obtained via `uv` (userspace, no admin) by default. Pass
  -Python to use a specific interpreter instead.

.EXAMPLE
  pwsh scripts/build-sidecar.ps1
.EXAMPLE
  pwsh scripts/build-sidecar.ps1 -Clean -TargetTriple x86_64-pc-windows-msvc
#>
[CmdletBinding()]
param(
    [string]$Python = "",
    [string]$TargetTriple = "x86_64-pc-windows-msvc",
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$SidecarDir = Join-Path $RepoRoot "sidecar"
$VenvDir = Join-Path $SidecarDir ".venv-build"
$BinDir = Join-Path $RepoRoot "src-tauri/binaries"
$VenvPy = Join-Path $VenvDir "Scripts/python.exe"

Push-Location $SidecarDir
try {
    if ($Clean -and (Test-Path $VenvDir)) {
        Write-Host "Removing existing build venv..." -ForegroundColor Yellow
        Remove-Item -Recurse -Force $VenvDir
    }

    # 1. Create the Python 3.11 venv.
    if (-not (Test-Path $VenvPy)) {
        if ($Python) {
            Write-Host "Creating venv with $Python ..." -ForegroundColor Cyan
            & $Python -m venv $VenvDir
        }
        elseif (Get-Command uv -ErrorAction SilentlyContinue) {
            Write-Host "Creating venv with uv (standalone Python 3.11) ..." -ForegroundColor Cyan
            uv venv --python 3.11 $VenvDir
        }
        else {
            throw "No -Python given and 'uv' not found. Install uv (https://docs.astral.sh/uv/) or pass -Python C:\path\to\python3.11.exe"
        }
    }

    # Confirm the interpreter is 3.11 and 64-bit.
    $ver = & $VenvPy -c "import sys,struct; print(f'{sys.version_info.major}.{sys.version_info.minor}'); print(struct.calcsize('P')*8)"
    $pyver, $bits = $ver -split "`n" | ForEach-Object { $_.Trim() }
    if ($pyver -ne "3.11") { throw "Build venv is Python $pyver; the pinned deps require 3.11." }
    if ($bits -ne "64") { throw "Build venv is $bits-bit; a 64-bit interpreter is required." }

    # 2. Install deps. Prefer uv (fast) if present, else pip. Hash-checking mode
    #    (a lock with --hash) rejects unhashed args, so PyInstaller installs
    #    separately.
    $useUv = [bool](Get-Command uv -ErrorAction SilentlyContinue)
    $lock = Join-Path $SidecarDir "requirements.lock"
    Write-Host "Installing sidecar dependencies..." -ForegroundColor Cyan
    if (Test-Path $lock) {
        # requirements.lock is a fully-pinned (==) freeze for reproducibility.
        if ($useUv) { uv pip install --python $VenvPy -r $lock }
        else { & $VenvPy -m pip install -r $lock }
    }
    else {
        Write-Warning "requirements.lock absent; installing from reviewed pins in requirements.txt and writing a fresh lock."
        if ($useUv) { uv pip install --python $VenvPy -r requirements.txt }
        else { & $VenvPy -m pip install -r requirements.txt }
        & $VenvPy -m pip freeze | Out-File -Encoding utf8 $lock
        Write-Host "Wrote reproducible lock: $lock" -ForegroundColor Green
    }
    if ($useUv) { uv pip install --python $VenvPy "pyinstaller>=6.11,<7" }
    else { & $VenvPy -m pip install "pyinstaller>=6.11,<7" }

    # 3. Freeze convertd.py. --collect-all pulls the data files PyInstaller
    #    otherwise misses (model loaders, native libs, version metadata).
    #    Slim, torch-free profile: no torch/transformers/sentence_transformers/
    #    gliclass --collect-all entries. classify/salience/ettin_spans degrade
    #    to deterministic fallbacks at runtime instead (see convertd.py).
    Write-Host "Running PyInstaller (slim, torch-free profile)..." -ForegroundColor Cyan
    & $VenvPy -m PyInstaller --clean --noconfirm --onefile --name convertd `
        --collect-all rapidocr `
        --collect-all lingua `
        --collect-all markitdown `
        --collect-all magika `
        --collect-all pypdfium2 `
        --hidden-import onnxruntime `
        convertd.py
    if ($LASTEXITCODE -ne 0) { throw "PyInstaller failed." }

    $built = Join-Path $SidecarDir "dist/convertd.exe"
    if (-not (Test-Path $built)) { throw "Expected build output missing: $built" }

    # 4. Smoke test: the protocol must answer a ping.
    Write-Host "Smoke-testing the built sidecar..." -ForegroundColor Cyan
    $resp = '{"id":1,"op":"ping"}' | & $built
    $parsed = $resp | ConvertFrom-Json
    if (-not ($parsed.ok -eq $true -and $parsed.id -eq 1)) {
        throw "Smoke test failed; sidecar did not answer ping with ok=true. Got: $resp"
    }
    Write-Host "Smoke test passed: $resp" -ForegroundColor Green

    # 5. Place it where Tauri's externalBin expects it.
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    $dest = Join-Path $BinDir "convertd-$TargetTriple.exe"
    Copy-Item -Force $built $dest
    $sha = (Get-FileHash -Algorithm SHA256 $dest).Hash.ToLower()
    Write-Host ""
    Write-Host "convertd sidecar built:" -ForegroundColor Green
    Write-Host "  $dest"
    Write-Host "  SHA-256: $sha"
    Write-Host "  Now stage llama-server (see RELEASING.md step 2) and run 'npm run tauri build'."
}
finally {
    Pop-Location
}
