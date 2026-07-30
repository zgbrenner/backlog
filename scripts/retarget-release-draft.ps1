[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$')]
    [string]$Tag,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string]$ExpectedSha,

    [Parameter(Mandatory = $true)]
    [string]$ExpectedName
)

$ErrorActionPreference = "Stop"
if (-not $env:GITHUB_REPOSITORY -or $env:GITHUB_REPOSITORY -notmatch '^[^/]+/[^/]+$') {
    throw "GITHUB_REPOSITORY is unavailable or invalid"
}

$releaseJson = & gh release view $Tag `
    --json databaseId,isDraft,name,tagName,targetCommitish 2>$null
$releaseExit = $LASTEXITCODE
if ($releaseExit -ne 0) {
    throw "Could not inspect draft release $Tag"
}
$release = $releaseJson | ConvertFrom-Json
if (-not $release.isDraft -or $release.name -ne $ExpectedName -or $release.tagName -ne $Tag) {
    throw "Release $Tag is not the expected unpublished draft"
}
if ($release.targetCommitish -eq $ExpectedSha) {
    Write-Host "Draft release $Tag already targets $ExpectedSha." -ForegroundColor Green
    return
}
if ($release.targetCommitish -notmatch '^[0-9a-fA-F]{40}$') {
    throw "Draft release $Tag does not have an immutable commit target"
}

$tagRows = @(& git ls-remote --exit-code --tags origin "refs/tags/$Tag" "refs/tags/${Tag}^{}")
$tagExit = $LASTEXITCODE
if ($tagExit -eq 0 -or $tagRows.Count -ne 0) {
    throw "Release tag $Tag already exists; refusing to retarget its draft"
}
if ($tagExit -ne 2) {
    throw "Could not prove that release tag $Tag is absent"
}

$oldSha = $release.targetCommitish.ToLowerInvariant()
$newSha = $ExpectedSha.ToLowerInvariant()
$comparisonJson = & gh api "repos/$env:GITHUB_REPOSITORY/compare/$oldSha...$newSha"
$comparisonExit = $LASTEXITCODE
if ($comparisonExit -ne 0) {
    throw "Could not compare the old and new draft targets"
}
$comparison = $comparisonJson | ConvertFrom-Json
if ($comparison.status -ne "ahead") {
    throw "New release target is not a tested descendant of the existing draft target"
}

$null = & gh api --method PATCH `
    "repos/$env:GITHUB_REPOSITORY/releases/$($release.databaseId)" `
    -f "target_commitish=$ExpectedSha"
if ($LASTEXITCODE -ne 0) {
    throw "Could not advance draft release $Tag to the tested descendant"
}

./scripts/assert-release-tag.ps1 -Tag $Tag -ExpectedSha $ExpectedSha
Write-Host "Draft release $Tag advanced from $oldSha to $newSha." -ForegroundColor Green
