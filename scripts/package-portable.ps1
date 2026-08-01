<#
.SYNOPSIS
  Build the installer-free BackLog Windows x64 payload as a ZIP.

.DESCRIPTION
  This is the portable companion to the NSIS build. It packages the raw Tauri
  executable with the exact release resources that the installed layout uses:
  the primary model, semantic model/tokenizer, llama-server and its app-local
  runtime DLLs, and the PyInstaller --onedir convertd tree.

  The portable ZIP carries a pinned x64 fixed WebView2 runtime under
  webview2-fixed/ and a double-click BackLog-Portable.cmd launcher. The
  launcher sets WEBVIEW2_BROWSER_EXECUTABLE_FOLDER before starting BackLog.exe,
  so the ZIP does not rely on an Evergreen runtime or another download.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string] $Version,
    [string] $Output = "",
    [string] $AppExe = "",
    [string] $BinDir = "",
    [string] $ResourceDir = "",
    [string] $Readme = "",
    [string] $WebView2RuntimeDir = "",
    [string] $Launcher = ""
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

if (-not $AppExe) { $AppExe = Join-Path $RepoRoot "src-tauri\target\release\backlog.exe" }
if (-not $BinDir) { $BinDir = Join-Path $RepoRoot "src-tauri\binaries" }
if (-not $ResourceDir) { $ResourceDir = Join-Path $RepoRoot "src-tauri\resources" }
if (-not $Readme) { $Readme = Join-Path $RepoRoot "docs\PORTABLE.md" }
if (-not $WebView2RuntimeDir) {
    $WebView2RuntimeDir = Join-Path $RepoRoot "src-tauri\portable-inputs\webview2-fixed"
}
if (-not $Launcher) { $Launcher = Join-Path $RepoRoot "scripts\BackLog-Portable.cmd" }
if (-not $Output) {
    $Output = Join-Path $RepoRoot "src-tauri\target\release\BackLog_${Version}_x64-portable.zip"
}
if (-not [IO.Path]::IsPathRooted($Output)) { $Output = Join-Path $RepoRoot $Output }
$Output = [IO.Path]::GetFullPath($Output)

function Require-File {
    param([Parameter(Mandatory)][string] $Path, [Parameter(Mandatory)][string] $Label)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label is missing: $Path"
    }
    if ((Get-Item -LiteralPath $Path).Length -eq 0) {
        throw "$Label is empty: $Path"
    }
}

function Require-Directory {
    param([Parameter(Mandatory)][string] $Path, [Parameter(Mandatory)][string] $Label)
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        throw "$Label is missing: $Path"
    }
}

function Invoke-NodeChecked {
    param([Parameter(Mandatory)][string[]] $Arguments)
    & node @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "node $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

$artifactName = "BackLog_${Version}_x64-portable.zip"
if ([IO.Path]::GetFileName($Output) -ne $artifactName) {
    throw "portable output must be named $artifactName"
}

$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$stage = Join-Path $tempRoot "backlog-portable-$([guid]::NewGuid().ToString('N'))"

try {
    Require-File $AppExe "Tauri release executable"
    Require-Directory $BinDir "release binaries directory"
    Require-Directory $ResourceDir "release resources directory"
    Require-File $Readme "portable instructions"
    Require-Directory $WebView2RuntimeDir "fixed WebView2 runtime directory"
    Require-File (Join-Path $WebView2RuntimeDir "msedgewebview2.exe") "fixed WebView2 browser executable"
    Require-File (Join-Path $WebView2RuntimeDir "runtime-manifest.json") "fixed WebView2 runtime manifest"
    Require-File $Launcher "portable launcher"

    $resourceFiles = @(
        @{ Relative = "name.gbnf"; Label = "naming grammar" },
        @{ Relative = "models\Qwen3-0.6B-Q8_0.gguf"; Label = "primary model" },
        @{ Relative = "models\semantic\all-MiniLM-L6-v2\model.onnx"; Label = "semantic model" },
        @{ Relative = "models\semantic\all-MiniLM-L6-v2\vocab.txt"; Label = "semantic vocabulary" }
    )
    foreach ($asset in $resourceFiles) {
        Require-File (Join-Path $ResourceDir $asset.Relative) $asset.Label
    }

    $llama = Join-Path $BinDir "llama-server-x86_64-pc-windows-msvc.exe"
    Require-File $llama "llama-server"
    $runtimeDlls = @(Get-ChildItem -LiteralPath $BinDir -File -Filter "*.dll" |
        Where-Object { $_.Name -ine "_placeholder.dll" })
    if ($runtimeDlls.Count -eq 0) {
        throw "no app-local runtime DLLs were staged beside llama-server"
    }

    $convertd = Join-Path $BinDir "convertd"
    Require-Directory $convertd "convertd onedir tree"
    Require-File (Join-Path $convertd "convertd.exe") "convertd executable"

    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $stage "resources\models\semantic\all-MiniLM-L6-v2") | Out-Null

    Copy-Item -LiteralPath $AppExe -Destination (Join-Path $stage "BackLog.exe")
    Copy-Item -LiteralPath $llama -Destination (Join-Path $stage "llama-server-x86_64-pc-windows-msvc.exe")
    foreach ($dll in $runtimeDlls) {
        Copy-Item -LiteralPath $dll.FullName -Destination (Join-Path $stage $dll.Name)
    }
    Copy-Item -LiteralPath $convertd -Destination (Join-Path $stage "convertd") -Recurse -Force
    Copy-Item -LiteralPath $Readme -Destination (Join-Path $stage "README-PORTABLE.md")
    Copy-Item -LiteralPath $Launcher -Destination (Join-Path $stage "BackLog-Portable.cmd")
    Copy-Item -LiteralPath $WebView2RuntimeDir -Destination (Join-Path $stage "webview2-fixed") -Recurse -Force
    foreach ($asset in $resourceFiles) {
        $destination = Join-Path $stage (Join-Path "resources" $asset.Relative)
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
        Copy-Item -LiteralPath (Join-Path $ResourceDir $asset.Relative) -Destination $destination
    }

    $portableContract = Join-Path $RepoRoot "scripts\portable-contract.mjs"
    Invoke-NodeChecked @(
        $portableContract, "write",
        "--root", $stage,
        "--version", $Version,
        "--artifact", $artifactName
    )
    Invoke-NodeChecked @(
        $portableContract, "verify",
        "--root", $stage,
        "--version", $Version
    )

    $outputParent = Split-Path -Parent $Output
    New-Item -ItemType Directory -Force -Path $outputParent | Out-Null
    if (Test-Path -LiteralPath $Output) { Remove-Item -LiteralPath $Output -Force }
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $Output -CompressionLevel Optimal -Force
    if (-not (Test-Path -LiteralPath $Output -PathType Leaf)) {
        throw "portable ZIP was not created: $Output"
    }

    & (Join-Path $PSScriptRoot "validate-portable-package.ps1") -Archive $Output -Version $Version
    if ($LASTEXITCODE -ne 0) { throw "portable ZIP validation failed" }
    $zipInfo = Get-Item -LiteralPath $Output
    $zipHash = (Get-FileHash -LiteralPath $Output -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host "Portable ZIP created and verified:" -ForegroundColor Green
    Write-Host "  $Output"
    Write-Host "  size: $($zipInfo.Length) bytes"
    Write-Host "  SHA-256: $zipHash"
    Write-Host "  WebView2: fixed x64 runtime bundled under webview2-fixed; double-click BackLog-Portable.cmd."
}
finally {
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}
