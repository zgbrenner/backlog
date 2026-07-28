#!/usr/bin/env bash
# Run every gate .github/workflows/ci.yml runs, on this machine.
#
# This exists because GitHub Actions does not run for this repository: every
# run in ci.yml's history failed within seconds, none ever assigned a runner
# (runner_id 0, empty runner_name, log download 404) — a private repo whose
# account has no Actions allowance left. See docs/KNOWN_ISSUES.md item 11.
# A CI file nobody can run is theatre, so the gates have to be executable
# locally or they are not gates at all.
#
# Keep this in lockstep with ci.yml. The version-agreement job below checks
# the two files still list the same jobs, so drift fails loudly here rather
# than being discovered the day someone turns Actions on.
#
#   ./scripts/ci-local.sh            # everything
#   ./scripts/ci-local.sh trust-core # one job (trust-core|workspace|frontend|python|version-drift)
#
# Exits non-zero on the first failure with the job named, so it is usable as a
# pre-push hook:  ln -s ../../scripts/ci-local.sh .git/hooks/pre-push

set -uo pipefail

cd "$(cd "$(dirname "$0")/.." && pwd)"

only="${1:-all}"
failed=()
started=$(date +%s)

# Colour only when attached to a terminal, so a redirected log stays readable.
if [ -t 1 ]; then
  bold=$'\033[1m'; red=$'\033[31m'; green=$'\033[32m'; dim=$'\033[2m'; off=$'\033[0m'
else
  bold=''; red=''; green=''; dim=''; off=''
fi

step() { # step <job> <label> <command...>
  local job="$1" label="$2"; shift 2
  [ "$only" = "all" ] || [ "$only" = "$job" ] || return 0
  printf '%s>> %-16s %s%s\n' "$bold" "$job" "$label" "$off"
  if "$@"; then
    printf '   %s%sok%s\n' "$green" "" "$off"
  else
    printf '   %s%sFAILED%s  (%s)\n' "$red" "$bold" "$off" "$label"
    failed+=("$job: $label")
  fi
}

# --- trust-core -------------------------------------------------------------
# First and cheap, exactly as in ci.yml: no system libraries, no sidecars, no
# icon, so a checker regression surfaces in seconds.
step trust-core "cargo test -p backlog-core" \
  cargo test --manifest-path src-tauri/Cargo.toml -p backlog-core --locked
step trust-core "cargo clippy -p backlog-core" \
  cargo clippy --manifest-path src-tauri/Cargo.toml -p backlog-core --all-targets --locked -- -D warnings

# --- workspace --------------------------------------------------------------
# tauri-build validates externalBin and the resources DLL glob at compile time,
# so without the stubs the app crate does not reach a single test.
step workspace "stage dev stub sidecars" bash scripts/dev-stubs.sh
step workspace "dev-stub marker contract" node .github/scripts/check-stub-marker.mjs
step workspace "cargo test --workspace" \
  cargo test --manifest-path src-tauri/Cargo.toml --workspace --all-targets --locked
step workspace "cargo clippy --workspace" \
  cargo clippy --manifest-path src-tauri/Cargo.toml --workspace --all-targets --locked -- -D warnings
step workspace "cargo fmt --check" \
  cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check

# --- frontend ---------------------------------------------------------------
step frontend "npm run check" npm run check
# The harness renders the real frontend against a mock Tauri IPC and exits
# non-zero on any console error — the only automated proof the UI still boots.
step frontend "UI harness" npm run harness:shots

# --- python -----------------------------------------------------------------
# numpy stays absent on purpose: the sidecar's classify/salience lanes promise
# a dependency-free fallback, and installing it here would hide the regression.
step python "pytest" python3 -m pytest sidecar/tests models/tests -q
step python "Power Automate manifest contract" python3 power-automate/validate_examples.py

# --- version-drift ----------------------------------------------------------
step version-drift "versions agree" node .github/scripts/check-versions.mjs
step version-drift "every code documented" node .github/scripts/check-troubleshooting-coverage.mjs
# The gate above was green for a whole review cycle while DISMISSED was
# undocumented. This proves it still fails, per source of the vocabulary.
step version-drift "coverage gate self-test" node .github/scripts/check-troubleshooting-coverage.test.mjs
# The other half of "the docs match the app": a row that tells the user to
# press a button by the wrong name is a dead end on the screen they reach when
# they are already stuck.
step version-drift "button labels match the docs" node .github/scripts/check-button-labels.mjs
step version-drift "ci.yml and ci-local.sh agree" node .github/scripts/check-ci-parity.mjs

# --- summary ----------------------------------------------------------------
elapsed=$(( $(date +%s) - started ))
echo
if [ ${#failed[@]} -eq 0 ]; then
  printf '%s%sAll gates passed%s in %ss.\n' "$bold" "$green" "$off" "$elapsed"
  printf '%sScreenshots of every UI state: dist-harness/shots/%s\n' "$dim" "$off"
  exit 0
fi
printf '%s%s%d gate(s) failed%s in %ss:\n' "$bold" "$red" "${#failed[@]}" "$off" "$elapsed"
printf '  - %s\n' "${failed[@]}"
exit 1
