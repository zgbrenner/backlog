[CmdletBinding()]
param(
    [string]$Python = "python",
    [string]$TargetTriple = "x86_64-pc-windows-msvc",
    [switch]$Clean,
    [switch]$RequireLockedDependencies
)

$ErrorActionPreference = "Stop"
$RepoRoot = Split-Path -Parent $PSScriptRoot
$SidecarDir = Join-Path $RepoRoot "sidecar"
$VenvDir = Join-Path $SidecarDir ".venv-build"
$VenvPython = Join-Path $VenvDir "Scripts\python.exe"
$BinariesDir = Join-Path $RepoRoot "src-tauri\binaries"
$Destination = Join-Path $BinariesDir "convertd-$TargetTriple.exe"
$RequirementsLock = Join-Path $SidecarDir "requirements.lock"
$RequirementsIntent = Join-Path $SidecarDir "requirements.txt"

if ($Clean -and (Test-Path $VenvDir)) {
    Remove-Item -Recurse -Force $VenvDir
}

$PythonVersion = & $Python -c "import struct,sys; print(str(sys.version_info.major)+'.'+str(sys.version_info.minor)+':'+str(struct.calcsize('P')*8))"
if ($LASTEXITCODE -ne 0 -or $PythonVersion.Trim() -ne "3.11:64") {
    throw "BackLog's Windows sidecar build requires 64-bit Python 3.11. Found: $PythonVersion"
}

if (-not (Test-Path $VenvPython)) {
    & $Python -m venv $VenvDir
}

if (Test-Path $RequirementsLock) {
    & $VenvPython -m pip install --require-hashes -r $RequirementsLock
} elseif ($RequireLockedDependencies) {
    throw "sidecar/requirements.lock is required for a release build. Regenerate it from requirements.in with pip-compile --generate-hashes."
} else {
    Write-Warning "Building an unsigned pilot sidecar from reviewed version ranges because requirements.lock is absent."
    & $VenvPython -m pip install -r $RequirementsIntent
}
& $VenvPython -m pip install "pyinstaller>=6.11,<7"

Push-Location $SidecarDir
try {
    & $VenvPython -m PyInstaller `
        --clean `
        --noconfirm `
        --onefile `
        --name convertd `
        --collect-all rapidocr `
        --collect-all lingua `
        --collect-all gliclass `
        --collect-all markitdown `
        --collect-all sentence_transformers `
        --hidden-import onnxruntime `
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
