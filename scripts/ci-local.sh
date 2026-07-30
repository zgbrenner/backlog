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
# Run by stdlib `unittest`, not pytest. Every test under those two directories
# imports nothing outside the standard library, so requiring a third-party test
# runner bought nothing and cost the gate its ability to run: `python3 -m pytest`
# died with "No module named pytest" on the release machine — the only machine
# that ever runs this script — so a 99-test suite was silently not a gate.
#
# `-t` points at the start directory itself because neither directory has an
# __init__.py, and plain `discover -s dir` then fails with "Start directory is
# not importable".
#
# Note on numpy: this used to claim "numpy stays absent on purpose", so that the
# sidecar's dependency-free classify/salience fallback stayed covered. That was
# not true of the ambient interpreter (numpy 2.4.4 is importable here), and
# `convertd.py` imports it lazily inside the ops, so this gate does not prove
# the fallback either way. It runs the suite; it does not pin the environment.
step python "unittest (sidecar)" \
  python3 -m unittest discover -s sidecar/tests -t sidecar/tests -q
step python "unittest (models)" \
  python3 -m unittest discover -s models/tests -t models/tests -q

# The manifest contract genuinely needs a third-party package
# (power-automate/requirements-dev.txt pins jsonschema). Rather than dying with
# a bare ModuleNotFoundError — which docs/RELEASE_CHECKLIST.md had resorted to
# documenting as expected — find an interpreter that has it, and if none does,
# say which command installs it.
python_with_jsonschema() {
  local candidate
  for candidate in python3 python ./sidecar/.venv-build/Scripts/python.exe; do
    if command -v "$candidate" >/dev/null 2>&1 &&
      "$candidate" -c "import jsonschema" >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

validate_power_automate() {
  local py
  if ! py="$(python_with_jsonschema)"; then
    echo "no interpreter on this machine can import jsonschema." >&2
    echo "install it with:  python3 -m pip install -r power-automate/requirements-dev.txt" >&2
    return 1
  fi
  "$py" power-automate/validate_examples.py
}

step python "Power Automate manifest contract" validate_power_automate

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
