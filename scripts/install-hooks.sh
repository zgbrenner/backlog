#!/usr/bin/env bash
# Point this clone's git hooks at the tracked .githooks/ directory.
#
# Run this first, once per clone, before writing any code. GitHub Actions has
# never executed for this repository (docs/KNOWN_ISSUES.md item 11), so the
# hooks in .githooks/ are the only thing enforcing the gates in
# .github/workflows/ci.yml. Until this has run, nothing checks anything.
#
# Why core.hooksPath and not a symlink into .git/hooks: .git/hooks is not
# tracked, so a symlink made there dies with the clone that made it and the next
# clone silently has no enforcement — the same failure mode as a CI file nobody
# can run. .githooks/ is tracked, so it travels with the repo and this one
# command is all a fresh clone needs.
#
# Idempotent. Re-running is also how you repair a clone whose core.hooksPath was
# unset or pointed somewhere else.
#
#   bash scripts/install-hooks.sh        # or: pwsh scripts/install-hooks.ps1
set -euo pipefail

cd "$(cd "$(dirname "$0")/.." && pwd)"

HOOKS=(pre-commit pre-push)

git config core.hooksPath .githooks
# Harmless on Windows (core.fileMode is false there); required on Linux/macOS,
# where git skips a hook that is not executable.
chmod +x "${HOOKS[@]/#/.githooks/}" 2>/dev/null || true

# --- verify, rather than assume ---------------------------------------------
# The thing this replaces was a hook nobody had installed, so "the command
# printed something reassuring" is not evidence.
actual="$(git config --get core.hooksPath || true)"
if [ "$actual" != ".githooks" ]; then
  echo "FAILED: core.hooksPath is '${actual:-unset}', expected '.githooks'." >&2
  exit 1
fi

for hook in "${HOOKS[@]}"; do
  path=".githooks/$hook"
  if [ ! -f "$path" ]; then
    echo "FAILED: $path is missing; git would silently run no $hook hook." >&2
    exit 1
  fi
  # Git runs hooks through its own bundled sh. A CRLF hook dies with
  # "bad interpreter: No such file or directory", which reads like a missing
  # bash rather than a line-ending problem, so name it precisely.
  if head -n 1 "$path" | grep -q $'\r'; then
    echo "FAILED: $path has CRLF line endings; git's sh rejects it with 'bad interpreter'." >&2
    echo "        .gitattributes pins .githooks/** to LF, so re-check out the file." >&2
    exit 1
  fi
  bash -n "$path" || { echo "FAILED: $path is not valid shell." >&2; exit 1; }
  # A hook committed as mode 100644 is ignored on Linux and macOS, which would
  # leave a fresh clone unenforced — precisely what the tracked directory is for.
  if git ls-files --error-unmatch "$path" >/dev/null 2>&1 &&
    [ "$(git ls-files -s "$path" | cut -d' ' -f1)" = "100644" ]; then
    echo "note: $path is recorded in git as mode 100644, so clones on Linux and"
    echo "      macOS will skip it. Fix once, then commit the mode change:"
    echo "        git update-index --chmod=+x ${HOOKS[*]/#/.githooks/}"
  fi
done

echo "core.hooksPath -> .githooks (verified)."
echo "  pre-commit  cargo fmt --check + the five file-reading gates, ~2s"
echo "  pre-push    scripts/ci-local.sh - all five CI jobs, ~3 min"
echo "Bypass either with BACKLOG_SKIP_HOOKS=1, or 'git <commit|push> --no-verify'."
