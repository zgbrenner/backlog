[CmdletBinding()]
param(
    [string]$Python = "python",
    [string]$TargetTriple = "x86_64-pc-windows-msvc",
    [switch]$Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$SidecarDir = Join-Path $RepoRoot "sidecar"
$VenvDir = Join-Path $SidecarDir ".venv-build"
$VenvPython = Join-Path $VenvDir "Scripts\python.exe"
$BinariesDir = Join-Path $RepoRoot "src-tauri\binaries"
$Destination = Join-Path $BinariesDir "convertd-$TargetTriple.exe"

if ($Clean -and (Test-Path $VenvDir)) {
    Remove-Item -Recurse -Force $VenvDir
}

$PythonVersion = & $Python -c "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')"
if ($LASTEXITCODE -ne 0 -or $PythonVersion.Trim() -ne "3.11") {
    throw "BackLog's reproducible Windows sidecar build requires 64-bit Python 3.11. Found: $PythonVersion"
}

if (-not (Test-Path $VenvPython)) {
    & $Python -m venv $VenvDir
}

& $VenvPython -m pip install --upgrade pip
& $VenvPython -m pip install -r (Join-Path $SidecarDir "requirements.txt") "pyinstaller>=6.11,<7"

Push-Location $SidecarDir
try {
    & $VenvPython -m PyInstaller `
        --clean `
        --noconfirm `
        --onefile `
        --name convertd `
        --collect-all rapidocr `
        --collect-all gliclass `
        --collect-all markitdown `
        --collect-all sentence_transformers `
        --hidden-import onnxruntime `
        --hidden-import fasttext `
        convertd.py
    if ($LASTEXITCODE -ne 0) {
        throw "PyInstaller failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

New-Item -ItemType Directory -Force -Path $BinariesDir | Out-Null
Copy-Item -Force (Join-Path $SidecarDir "dist\convertd.exe") $Destination

$Response = '{"id":1,"op":"ping"}' | & $Destination
if ($LASTEXITCODE -ne 0) {
    throw "convertd ping exited with code $LASTEXITCODE"
}
$Parsed = $Response | ConvertFrom-Json
if (-not $Parsed.ok -or $Parsed.id -ne 1) {
    throw "convertd ping returned an invalid response: $Response"
}

& $VenvPython -m pip freeze | Set-Content -Encoding UTF8 (Join-Path $SidecarDir "sidecar-build-lock.txt")
$Hash = (Get-FileHash -Algorithm SHA256 $Destination).Hash.ToLowerInvariant()
Write-Host "Built $Destination"
Write-Host "SHA256 $Hash"
