<#
.SYNOPSIS
  Expand and validate a BackLog portable ZIP without executing it.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Archive,
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$')]
    [string] $Version
)

$ErrorActionPreference = "Stop"
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not [IO.Path]::IsPathRooted($Archive)) { $Archive = Join-Path $RepoRoot $Archive }
$Archive = [IO.Path]::GetFullPath($Archive)
if (-not (Test-Path -LiteralPath $Archive -PathType Leaf)) {
    throw "portable ZIP is missing: $Archive"
}
if ([IO.Path]::GetExtension($Archive) -ine ".zip") {
    throw "portable artifact must be a .zip: $Archive"
}

$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$expanded = Join-Path $tempRoot "backlog-portable-verify-$([guid]::NewGuid().ToString('N'))"
try {
    New-Item -ItemType Directory -Force -Path $expanded | Out-Null
    Expand-Archive -LiteralPath $Archive -DestinationPath $expanded -Force
    & node (Join-Path $PSScriptRoot "portable-contract.mjs") verify `
        --root $expanded `
        --version $Version
    if ($LASTEXITCODE -ne 0) {
        throw "portable-contract.mjs rejected $Archive"
    }
    Write-Host "Portable ZIP archive verified: $Archive" -ForegroundColor Green
}
finally {
    if (Test-Path -LiteralPath $expanded) {
        Remove-Item -LiteralPath $expanded -Recurse -Force
    }
}
