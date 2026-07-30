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
$tagExit = $LASTEXITCODE

if ($tagExit -eq 0) {
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
    return
}

if ($tagExit -ne 2) {
    throw "Could not check release tag $Tag"
}

# A GitHub draft created with --target does not create its tag until the draft
# is published. In that state, verify the immutable draft target instead.
$draftJson = & gh release view $Tag --json isDraft,tagName,targetCommitish 2>$null
$draftExit = $LASTEXITCODE
if ($draftExit -ne 0) {
    throw "Could not resolve release tag or draft $Tag"
}
$draft = $draftJson | ConvertFrom-Json
if (-not $draft.isDraft) {
    throw "Release $Tag is published but its tag is missing"
}
if ($draft.tagName -ne $Tag) {
    throw "Draft release tag name does not match $Tag"
}
if (
    -not $draft.targetCommitish -or
    $draft.targetCommitish.ToLowerInvariant() -ne $ExpectedSha.ToLowerInvariant()
) {
    throw "Draft release $Tag does not target the exact CI-tested commit"
}

Write-Host "Draft release $Tag still targets $ExpectedSha." -ForegroundColor Green
