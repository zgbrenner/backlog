<#
.SYNOPSIS
  Point this clone's git hooks at the tracked .githooks/ directory.

.DESCRIPTION
  Run this first, once per clone, before writing any code. GitHub Actions has
  never executed for this repository (docs/KNOWN_ISSUES.md item 11), so the
  hooks in .githooks/ are the only thing enforcing the gates in
  .github/workflows/ci.yml. Until this has run, nothing checks anything.

  Why core.hooksPath and not a symlink into .git/hooks: .git/hooks is not
  tracked, so a symlink made there dies with the clone that made it and the next
  clone silently has no enforcement, the same failure mode as a CI file nobody
  can run. .githooks/ is tracked, so it travels with the repo and this one
  command is all a fresh clone needs.

      pre-commit  cargo fmt --check + the five file-reading gates, ~2s
      pre-push    scripts/ci-local.sh - all five CI jobs, ~3 min

  Idempotent. Re-running is also how you repair a clone whose core.hooksPath was
  unset or pointed somewhere else.

      pwsh scripts/install-hooks.ps1     # or: bash scripts/install-hooks.sh
#>
$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $root
try {
    $hooks = @("pre-commit", "pre-push")

    git config core.hooksPath .githooks

    # --- verify, rather than assume -----------------------------------------
    # The thing this replaces was a hook nobody had installed, so "the command
    # printed something reassuring" is not evidence.
    $actual = (git config --get core.hooksPath)
    if ($actual -ne ".githooks") {
        if (-not $actual) { $actual = "unset" }
        throw "core.hooksPath is '$actual', expected '.githooks'."
    }

    foreach ($hook in $hooks) {
        $path = ".githooks/$hook"
        if (-not (Test-Path $path)) {
            throw "$path is missing; git would silently run no $hook hook."
        }
        # Git runs hooks through its own bundled sh. A CRLF hook dies with
        # "bad interpreter: No such file or directory", which reads like a
        # missing bash rather than a line-ending problem, so name it precisely.
        if ([System.IO.File]::ReadAllBytes((Resolve-Path $path)) -contains 13) {
            throw ("$path has CRLF line endings; git's sh rejects it with " +
                "'bad interpreter'. .gitattributes pins .githooks/** to LF, " +
                "so re-check out the file.")
        }
        # A hook committed as mode 100644 is ignored on Linux and macOS, which
        # would leave a fresh clone unenforced, precisely what the tracked
        # directory is for. Windows cannot set the bit; git records it.
        $indexed = (git ls-files -s -- $path)
        if ($indexed -and $indexed.StartsWith("100644")) {
            Write-Host "note: $path is recorded in git as mode 100644, so clones on Linux"
            Write-Host "      and macOS will skip it. Fix once, then commit the mode change:"
            Write-Host ("        git update-index --chmod=+x " +
                (($hooks | ForEach-Object { ".githooks/$_" }) -join " "))
        }
    }

    Write-Host "core.hooksPath -> .githooks (verified)." -ForegroundColor Green
    Write-Host "  pre-commit  cargo fmt --check + the five file-reading gates, ~2s"
    Write-Host "  pre-push    scripts/ci-local.sh - all five CI jobs, ~3 min"
    Write-Host "Bypass either with BACKLOG_SKIP_HOOKS=1, or 'git <commit|push> --no-verify'."
}
finally {
    Pop-Location
}
