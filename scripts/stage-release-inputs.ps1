<#
.SYNOPSIS
  Download and stage the hash-pinned models and llama.cpp runtime for a
  Windows release build.

.DESCRIPTION
  This script is intentionally release-only. It downloads the exact primary
  naming model, semantic ONNX model/tokenizer, and llama.cpp archive, verifies
  each before copying anything into the Tauri bundle, rejects the development
  marker for executable/model payloads, and stages the Visual C++ runtime files
  imported by llama.cpp. It never downloads the optional 1.7B escalation model.
#>
[CmdletBinding()]
param(
    [string] $DownloadDir = "",
    [switch] $Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = [IO.Path]::GetFullPath((Resolve-Path (Join-Path $PSScriptRoot "..")).Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
$BinDir = Join-Path $RepoRoot "src-tauri\binaries"
$ModelDir = Join-Path $RepoRoot "src-tauri\resources\models"
$Model = Join-Path $ModelDir "Qwen3-0.6B-Q8_0.gguf"
$SemanticDir = Join-Path $ModelDir "semantic\all-MiniLM-L6-v2"
$SemanticModel = Join-Path $SemanticDir "model.onnx"
$SemanticVocab = Join-Path $SemanticDir "vocab.txt"

$StubMarker = "BACKLOG-DEV-STUB-DO-NOT-SHIP"
$PrimaryModelUrl = "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf?download=true"
$PrimaryModelSha256 = "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031"
$SemanticRevision = "751bff37182d3f1213fa05d7196b954e230abad9"
$SemanticModelUrl = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/$SemanticRevision/onnx/model_quantized.onnx?download=true"
$SemanticModelSha256 = "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1"
$SemanticVocabUrl = "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/$SemanticRevision/vocab.txt?download=true"
$SemanticVocabSha256 = "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3"
$LlamaArchiveUrl = "https://github.com/ggml-org/llama.cpp/releases/download/b10091/llama-b10091-bin-win-cpu-x64.zip"
$LlamaArchiveSha256 = "b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee"
$LlamaServerSha256 = "78af9cfb34f346b0de1e4f9c1577061cb3d55e8be55c8d540fde878e56bd0fe2"

if (-not $DownloadDir) {
    $tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $DownloadDir = Join-Path $tempRoot "backlog-release-inputs"
}
$DownloadDir = [IO.Path]::GetFullPath($DownloadDir).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)

function Test-PathWithin {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Root
    )
    $pathWithSeparator = "$Path$([IO.Path]::DirectorySeparatorChar)"
    $rootWithSeparator = "$($Root.TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar))$([IO.Path]::DirectorySeparatorChar)"
    return $pathWithSeparator.StartsWith($rootWithSeparator, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-SafeDownloadDirectory {
    param([Parameter(Mandatory)][string] $Path)

    $root = [IO.Path]::GetPathRoot($Path).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $profile = [Environment]::GetFolderPath([Environment+SpecialFolder]::UserProfile)
    $profile = if ($profile) { [IO.Path]::GetFullPath($profile).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar) } else { "" }
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $isDedicatedTempPath = Test-PathWithin -Path $Path -Root $tempRoot
    if (-not $Path -or $Path -eq $root -or $Path -eq $RepoRoot -or
        (Test-PathWithin -Path $Path -Root $RepoRoot) -or
        (Test-PathWithin -Path $RepoRoot -Root $Path) -or
        ($profile -and -not $isDedicatedTempPath -and ($Path -eq $profile -or (Test-PathWithin -Path $Path -Root $profile) -or (Test-PathWithin -Path $profile -Root $Path)))) {
        throw "Refusing unsafe release download directory '$Path'. Use a dedicated temporary directory outside the repository and user profile."
    }
}

Assert-SafeDownloadDirectory -Path $DownloadDir
$DownloadMarker = Join-Path $DownloadDir ".backlog-release-inputs.marker"

if ($Clean) {
    if ((Test-Path $DownloadDir) -and -not (Test-Path $DownloadMarker -PathType Leaf)) {
        throw "Refusing to recursively delete '$DownloadDir': the BackLog staging marker is missing."
    }
    if (Test-Path $BinDir) { Remove-Item -Recurse -Force $BinDir }
    if (Test-Path $ModelDir) { Remove-Item -Recurse -Force $ModelDir }
    if (Test-Path $DownloadDir) { Remove-Item -Recurse -Force $DownloadDir }
}
New-Item -ItemType Directory -Force $BinDir, $ModelDir, $SemanticDir, $DownloadDir | Out-Null
if (-not (Test-Path $DownloadMarker -PathType Leaf)) {
    [IO.File]::WriteAllText($DownloadMarker, "BackLog release staging directory. Do not use for unrelated data.`n")
}

function Test-CarriesStubMarker {
    param([Parameter(Mandatory)][string] $Path)
    $markerBytes = [Text.Encoding]::ASCII.GetBytes($StubMarker)
    $stream = [IO.File]::OpenRead($Path)
    try {
        if ($stream.Length -lt $markerBytes.Length) { return $false }
        $head = [byte[]]::new($markerBytes.Length)
        $read = $stream.Read($head, 0, $head.Length)
        if ($read -ne $head.Length) { return $false }
        return [Text.Encoding]::ASCII.GetString($head) -eq $StubMarker
    }
    finally {
        $stream.Dispose()
    }
}

function Assert-Hash {
    param(
        [Parameter(Mandatory)][string] $Path,
        [Parameter(Mandatory)][string] $Expected,
        [Parameter(Mandatory)][string] $Label
    )
    $actual = (Get-FileHash $Path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Expected) {
        if ($Label -eq "Primary model") {
            throw "Primary model hash mismatch: expected $Expected, computed $actual"
        }
        throw "$Label hash mismatch: expected $Expected, computed $actual"
    }
}

function Stage-VerifiedDownload {
    param(
        [Parameter(Mandatory)][string] $Url,
        [Parameter(Mandatory)][string] $DownloadName,
        [Parameter(Mandatory)][string] $Destination,
        [Parameter(Mandatory)][string] $ExpectedSha256,
        [Parameter(Mandatory)][string] $Label,
        [switch] $RejectStubMarker
    )
    $download = Join-Path $DownloadDir "$DownloadName.download"
    Invoke-WebRequest -Uri $Url -OutFile $download -UseBasicParsing
    Assert-Hash $download $ExpectedSha256 $Label
    if ($RejectStubMarker -and (Test-CarriesStubMarker $download)) {
        throw "$Label contains $StubMarker"
    }
    $parent = Split-Path -Parent $Destination
    New-Item -ItemType Directory -Force $parent | Out-Null
    Move-Item -Force $download $Destination
    Assert-Hash $Destination $ExpectedSha256 "staged $Label"
}

# Download to a temporary path and verify before replacing the bundle resource.
Stage-VerifiedDownload `
    -Url $PrimaryModelUrl `
    -DownloadName "Qwen3-0.6B-Q8_0.gguf" `
    -Destination $Model `
    -ExpectedSha256 $PrimaryModelSha256 `
    -Label "Primary model" `
    -RejectStubMarker
Stage-VerifiedDownload `
    -Url $SemanticModelUrl `
    -DownloadName "semantic-all-MiniLM-L6-v2-model.onnx" `
    -Destination $SemanticModel `
    -ExpectedSha256 $SemanticModelSha256 `
    -Label "semantic\all-MiniLM-L6-v2\model.onnx"
Stage-VerifiedDownload `
    -Url $SemanticVocabUrl `
    -DownloadName "semantic-all-MiniLM-L6-v2-vocab.txt" `
    -Destination $SemanticVocab `
    -ExpectedSha256 $SemanticVocabSha256 `
    -Label "semantic\all-MiniLM-L6-v2\vocab.txt"

$llamaArchive = Join-Path $DownloadDir "llama-b10091-bin-win-cpu-x64.zip"
Invoke-WebRequest -Uri $LlamaArchiveUrl -OutFile $llamaArchive -UseBasicParsing
Assert-Hash $llamaArchive $LlamaArchiveSha256 "llama.cpp archive"

$llamaDir = Join-Path $DownloadDir "llama-cpu"
if (Test-Path $llamaDir) { Remove-Item -Recurse -Force $llamaDir }
Expand-Archive $llamaArchive -DestinationPath $llamaDir

$servers = @(Get-ChildItem $llamaDir -Recurse -File -Filter "llama-server.exe")
if ($servers.Count -ne 1) {
    throw "Expected exactly one llama-server.exe in the pinned archive; found $($servers.Count)"
}
$server = $servers[0]
Assert-Hash $server.FullName $LlamaServerSha256 "llama-server.exe"
if (Test-CarriesStubMarker $server.FullName) {
    throw "llama-server.exe contains $StubMarker"
}
Copy-Item $server.FullName (Join-Path $BinDir "llama-server-x86_64-pc-windows-msvc.exe")

$runtimeDlls = @(Get-ChildItem $server.Directory.FullName -File -Filter "*.dll")
if ($runtimeDlls.Count -eq 0) {
    throw "Pinned llama.cpp archive contains no runtime DLLs beside llama-server.exe"
}
Copy-Item $runtimeDlls.FullName $BinDir

# The pinned llama runtime imports the VC143 CRT. Locate the exact redist that
# ships with the runner's installed MSVC toolchain and stage its three required
# files app-locally; verify-binaries.ps1 then reads every import table and fails
# if any non-Windows DLL is still unresolved.
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { throw "vswhere.exe is missing from the Windows runner" }
$vsInstall = (& $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
    -property installationPath).Trim()
if (-not $vsInstall) { throw "Visual Studio C++ build tools are not installed" }
$redistRoot = Join-Path $vsInstall "VC\Redist\MSVC"
$crtFiles = @("vcruntime140.dll", "vcruntime140_1.dll", "msvcp140.dll")
$crtDir = Get-ChildItem $redistRoot -Directory |
    Sort-Object Name -Descending |
    ForEach-Object { Join-Path $_.FullName "x64\Microsoft.VC143.CRT" } |
    Where-Object {
        $candidate = $_
        ($crtFiles | Where-Object { -not (Test-Path (Join-Path $candidate $_)) }).Count -eq 0
    } |
    Select-Object -First 1
if (-not $crtDir) {
    throw "No installed VC143 redist contains all required CRT files"
}
foreach ($name in $crtFiles) {
    Copy-Item (Join-Path $crtDir $name) $BinDir
}

Write-Host "Staged hash-verified release inputs:" -ForegroundColor Green
Write-Host "  primary model  $PrimaryModelSha256"
Write-Host "  semantic model $SemanticModelSha256"
Write-Host "  semantic vocab $SemanticVocabSha256"
Write-Host "  llama archive  $LlamaArchiveSha256"
Write-Host "  llama server   $LlamaServerSha256"
Write-Host "  runtime DLLs   $($runtimeDlls.Count + $crtFiles.Count)"
