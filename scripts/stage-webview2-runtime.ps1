<#
.SYNOPSIS
  Download, verify, and stage the pinned x64 fixed WebView2 runtime.

.DESCRIPTION
  The portable ZIP must carry its own fixed WebView2 runtime. This script is
  the only release step that downloads that runtime: it verifies the exact
  Microsoft CAB size and SHA-256, downloads it in 16 MiB HTTP byte ranges,
  expands the complete CAB with 7za (or the pinned runner's inbox CAB
  extractor), and writes a manifest beside the unpacked files. The resulting
  directory is passed to package-portable.ps1 and copied into the ZIP as
  webview2-fixed/.
#>
[CmdletBinding()]
param(
    [string] $Destination = "",
    [string] $DownloadDir = "",
    [switch] $Clean
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

$WebView2Version = "151.0.4129.59"
$WebView2Url = "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/3cb717d2-b86d-4160-a13e-f3860141dc7f/Microsoft.WebView2.FixedVersionRuntime.151.0.4129.59.x64.cab"
$WebView2Sha256 = "056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc"
$WebView2CabSize = [int64]304114944
$WebView2FileCount = 256

if (-not $Destination) {
    $Destination = Join-Path $RepoRoot "src-tauri\portable-inputs\webview2-fixed"
}
if (-not $DownloadDir) {
    $downloadRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
    $DownloadDir = Join-Path $downloadRoot "backlog-webview2-fixed-download"
}
$Destination = [IO.Path]::GetFullPath($Destination)
$DownloadDir = [IO.Path]::GetFullPath($DownloadDir)
if ([StringComparer]::OrdinalIgnoreCase.Equals($Destination, $DownloadDir)) {
    throw "WebView2 destination and download directory must be different paths"
}
$CabName = "Microsoft.WebView2.FixedVersionRuntime.$WebView2Version.x64.cab"
$CabPath = Join-Path $DownloadDir $CabName
$ExtractPath = Join-Path $DownloadDir "expanded-$WebView2Version"
$ChunkSize = [int64](16MB)

if ($Clean -and (Test-Path -LiteralPath $Destination)) {
    Remove-Item -LiteralPath $Destination -Recurse -Force
}
if (Test-Path -LiteralPath $ExtractPath) {
    Remove-Item -LiteralPath $ExtractPath -Recurse -Force
}
New-Item -ItemType Directory -Path $DownloadDir -Force | Out-Null

try { Add-Type -AssemblyName System.Net.Http -ErrorAction Stop } catch { }

function Download-CabInRanges {
    param(
        [Parameter(Mandatory)][string] $Uri,
        [Parameter(Mandatory)][string] $OutputPath,
        [Parameter(Mandatory)][int64] $ExpectedSize,
        [Parameter(Mandatory)][int64] $RangeSize
    )

    $client = [System.Net.Http.HttpClient]::new()
    $client.Timeout = [TimeSpan]::FromMinutes(10)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd("BackLog-release-stager/1.0")
    $output = $null
    try {
        $output = [IO.File]::Open(
            $OutputPath,
            [IO.FileMode]::Create,
            [IO.FileAccess]::Write,
            [IO.FileShare]::None
        )
        for ([int64]$start = 0; $start -lt $ExpectedSize; $start += $RangeSize) {
            [int64]$end = [Math]::Min($ExpectedSize - 1, $start + $RangeSize - 1)
            $expectedChunkSize = $end - $start + 1
            $chunkPath = Join-Path $DownloadDir "chunk-$start-$end.bin"
            $chunkReady = $false
            $lastError = $null
            for ($attempt = 1; $attempt -le 3 -and -not $chunkReady; $attempt++) {
                if (Test-Path -LiteralPath $chunkPath) {
                    Remove-Item -LiteralPath $chunkPath -Force
                }
                $request = $null
                $response = $null
                $inputStream = $null
                $chunkOutput = $null
                try {
                    $request = [System.Net.Http.HttpRequestMessage]::new(
                        [System.Net.Http.HttpMethod]::Get,
                        $Uri
                    )
                    $request.Headers.Range = [System.Net.Http.Headers.RangeHeaderValue]::new($start, $end)
                    $response = $client.SendAsync(
                        $request,
                        [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead
                    ).GetAwaiter().GetResult()
                    if ([int]$response.StatusCode -ne 206) {
                        throw "WebView2 range $start-$end returned HTTP $([int]$response.StatusCode), not 206"
                    }
                    $contentRange = $response.Content.Headers.ContentRange
                    if (
                        $null -eq $contentRange -or
                        $contentRange.From -ne $start -or
                        $contentRange.To -ne $end -or
                        $contentRange.Length -ne $ExpectedSize
                    ) {
                        throw "WebView2 range response did not identify the requested byte range"
                    }
                    $inputStream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                    $chunkOutput = [IO.File]::Open(
                        $chunkPath,
                        [IO.FileMode]::Create,
                        [IO.FileAccess]::Write,
                        [IO.FileShare]::None
                    )
                    $buffer = New-Object byte[] (1MB)
                    while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $chunkOutput.Write($buffer, 0, $read)
                    }
                    $chunkOutput.Flush()
                    $chunkReady = (Get-Item -LiteralPath $chunkPath).Length -eq $expectedChunkSize
                    if (-not $chunkReady) {
                        throw "WebView2 range $start-$end returned the wrong byte count"
                    }
                } catch {
                    $lastError = $_.Exception
                } finally {
                    if ($chunkOutput) { $chunkOutput.Dispose() }
                    if ($inputStream) { $inputStream.Dispose() }
                    if ($response) { $response.Dispose() }
                    if ($request) { $request.Dispose() }
                }
                if (-not $chunkReady -and $attempt -lt 3) {
                    Start-Sleep -Seconds 2
                }
            }
            if (-not $chunkReady) {
                throw "could not download WebView2 byte range $start-$end after three attempts: $lastError"
            }
            $chunkBytes = [IO.File]::ReadAllBytes($chunkPath)
            $output.Write($chunkBytes, 0, $chunkBytes.Length)
            Remove-Item -LiteralPath $chunkPath -Force
            Write-Host "  downloaded bytes $start-$end of $ExpectedSize"
        }
        $output.Flush()
    } finally {
        if ($output) { $output.Dispose() }
        $client.Dispose()
    }
}

$cabInfo = Get-Item -LiteralPath $CabPath -ErrorAction SilentlyContinue
$cabIsValid = $false
if ($cabInfo -and $cabInfo.Length -eq $WebView2CabSize) {
    $cabIsValid = (Get-FileHash -LiteralPath $CabPath -Algorithm SHA256).Hash.ToLowerInvariant() -eq $WebView2Sha256
}
if (-not $cabIsValid) {
    Write-Host "Downloading fixed WebView2 $WebView2Version in $ChunkSize-byte ranges..."
    Download-CabInRanges -Uri $WebView2Url -OutputPath $CabPath -ExpectedSize $WebView2CabSize -RangeSize $ChunkSize
}
$cabInfo = Get-Item -LiteralPath $CabPath
if ($cabInfo.Length -ne $WebView2CabSize) {
    throw "WebView2 CAB size mismatch: expected $WebView2CabSize, got $($cabInfo.Length)"
}
$actualCabHash = (Get-FileHash -LiteralPath $CabPath -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualCabHash -ne $WebView2Sha256) {
    throw "WebView2 CAB SHA-256 mismatch: expected $WebView2Sha256, got $actualCabHash"
}

$sevenZip = Get-Command 7za.exe -ErrorAction SilentlyContinue
$sevenZipPath = $null
if ($sevenZip) { $sevenZipPath = $sevenZip.Source }
if (-not $sevenZip) { $sevenZip = Get-Command 7z.exe -ErrorAction SilentlyContinue }
if (-not $sevenZipPath -and $sevenZip) { $sevenZipPath = $sevenZip.Source }
if (-not $sevenZip) {
    foreach ($candidate in @(
        "C:\Program Files\7-Zip\7za.exe",
        "C:\Program Files\7-Zip\7z.exe",
        "C:\Program Files (x86)\7-Zip\7za.exe",
        "C:\Program Files (x86)\7-Zip\7z.exe"
    )) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $sevenZip = Get-Item -LiteralPath $candidate
            $sevenZipPath = $sevenZip.FullName
            break
        }
    }
}
$expand = $null
if (-not $sevenZip) { $expand = Get-Command expand.exe -ErrorAction SilentlyContinue }
if (-not $sevenZip -and -not $expand) {
    throw "7za.exe/7z.exe or Windows expand.exe is required to unpack the complete fixed WebView2 CAB"
}
New-Item -ItemType Directory -Path $ExtractPath -Force | Out-Null
if ($sevenZipPath) {
    & $sevenZipPath x $CabPath "-o$ExtractPath" -y | Out-Host
} else {
    & $expand.Source -F:* $CabPath $ExtractPath | Out-Host
}
if ($LASTEXITCODE -ne 0) {
    throw "fixed WebView2 extractor failed with exit code $LASTEXITCODE"
}

$browserFiles = @(Get-ChildItem -LiteralPath $ExtractPath -Recurse -File -Filter "msedgewebview2.exe")
if ($browserFiles.Count -ne 1) {
    throw "expected one msedgewebview2.exe in the fixed WebView2 CAB, found $($browserFiles.Count)"
}
$Source = $browserFiles[0].Directory.FullName
foreach ($required in @(
    "msedgewebview2.exe",
    "msedge.dll",
    "$WebView2Version.manifest"
)) {
    if (-not (Test-Path -LiteralPath (Join-Path $Source $required) -PathType Leaf)) {
        throw "fixed WebView2 runtime is missing $required"
    }
}
$sourceFiles = @(Get-ChildItem -LiteralPath $Source -Recurse -File)
if ($sourceFiles.Count -ne $WebView2FileCount) {
    throw "fixed WebView2 runtime file count mismatch: expected $WebView2FileCount, got $($sourceFiles.Count)"
}

if (Test-Path -LiteralPath $Destination) {
    Remove-Item -LiteralPath $Destination -Recurse -Force
}
New-Item -ItemType Directory -Path $Destination -Force | Out-Null
foreach ($sourceEntry in @(Get-ChildItem -LiteralPath $Source -Force)) {
    Copy-Item -LiteralPath $sourceEntry.FullName `
        -Destination (Join-Path $Destination $sourceEntry.Name) -Recurse -Force
}

$runtimeFiles = @(
    Get-ChildItem -LiteralPath $Destination -Recurse -File |
        ForEach-Object {
            $_.FullName.Substring($Destination.Length + 1).Replace("\", "/")
        } |
        Sort-Object
)
if ($runtimeFiles.Count -ne $WebView2FileCount) {
    throw "staged fixed WebView2 runtime file count mismatch: expected $WebView2FileCount, got $($runtimeFiles.Count)"
}
$runtimeManifest = [ordered]@{
    schema = 1
    product = "BackLog"
    version = $WebView2Version
    architecture = "x64"
    cab_url = $WebView2Url
    cab_sha256 = $WebView2Sha256
    cab_size = $WebView2CabSize
    file_count = $WebView2FileCount
    files = $runtimeFiles
}
$manifestJson = $runtimeManifest | ConvertTo-Json -Depth 4
$utf8NoBom = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText(
    (Join-Path $Destination "runtime-manifest.json"),
    "$manifestJson`n",
    $utf8NoBom
)

Write-Host "Fixed WebView2 runtime staged and verified:" -ForegroundColor Green
Write-Host "  version: $WebView2Version"
Write-Host "  CAB SHA-256: $WebView2Sha256"
Write-Host "  CAB size: $WebView2CabSize bytes"
Write-Host "  runtime files: $($runtimeFiles.Count)"
Write-Host "  directory: $Destination"
