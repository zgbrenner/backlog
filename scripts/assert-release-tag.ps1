[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Tag,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string]$ExpectedSha
)

$ErrorActionPreference = "Stop"
$baseRef = "refs/tags/$Tag"
$peeledRef = "refs/tags/${Tag}^{}"
$rows = @(& git ls-remote --exit-code --tags origin $baseRef $peeledRef)
if ($LASTEXITCODE -ne 0) {
    throw "Could not resolve release tag $Tag"
}

$parsed = @(
    foreach ($row in $rows) {
        $parts = $row.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[0] -match '^[0-9a-fA-F]{40}$') {
            [PSCustomObject]@{ Sha = $parts[0]; Ref = $parts[1] }
        }
    }
)
$resolved = $parsed | Where-Object { $_.Ref -eq $peeledRef } | Select-Object -First 1
if (-not $resolved) {
    $resolved = $parsed | Where-Object { $_.Ref -eq $baseRef } | Select-Object -First 1
}
if (-not $resolved) {
    throw "Release tag $Tag did not resolve to a commit"
}
if ($resolved.Sha.ToLowerInvariant() -ne $ExpectedSha.ToLowerInvariant()) {
    throw "Release tag $Tag does not point at the exact CI-tested commit"
}

Write-Host "Release tag $Tag still points at $ExpectedSha." -ForegroundColor Green
