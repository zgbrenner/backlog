#!/usr/bin/env bash
# Create placeholder sidecar binaries so the full workspace build/tests run
# on a fresh checkout. tauri-build validates externalBin + the bundle.resources
# DLL glob at compile time; those files are gitignored (built via
# scripts/build-sidecar.ps1 / downloaded per RELEASING.md). The trust core needs
# none of this: `cargo test -p backlog-core`.
#
# The stub names must carry the *host* target triple, because that is what
# tauri-build resolves externalBin against. Hardcoding the Windows triple made
# this script a no-op on a Linux/macOS dev box or CI runner — the build still
# failed with "resource path binaries/convertd-<host-triple> doesn't exist".
# The Windows triple is always staged too, so a cross-build stays covered.
#
# Each stub carries STUB_MARKER rather than being zero bytes. A zero-byte file
# is indistinguishable from a truncated real build, so the realistic accident —
# stub to get tests green, cut a frontend-only release, skip the sidecar
# rebuild — produced an installer that built clean, installed clean and failed
# only when the first document reached the SLM lane on a user's machine. A
# marked file is provably a stub, which is what scripts/verify-binaries.ps1
# refuses before a release build.
set -euo pipefail

# Kept byte-identical in scripts/dev-stubs.ps1 and scripts/verify-binaries.ps1.
# Deliberately not "MZ"-prefixed, so a stub also fails a plain PE-magic test.
STUB_MARKER="BACKLOG-DEV-STUB-DO-NOT-SHIP"

bin="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/binaries"
mkdir -p "$bin"

host="$(rustc -vV | awk '/^host: /{print $2}')"
if [ -z "$host" ]; then
  echo "could not determine host target triple from 'rustc -vV'" >&2
  exit 1
fi

stub() { # stub <path>
  printf '%s\n' "$STUB_MARKER" > "$1"
}

stage() { # stage <triple>
  local triple="$1" ext=""
  case "$triple" in *windows*) ext=".exe" ;; esac
  for tool in convertd llama-server; do
    local f="$bin/${tool}-${triple}${ext}"
    # An already-present REAL binary must survive. This script gets run
    # casually, and clobbering a freshly built convertd.exe with a marker on
    # the release machine would be a silent, expensive regression.
    if [ ! -e "$f" ]; then
      stub "$f"
    elif [ ! -s "$f" ]; then
      stub "$f" # upgrade a legacy zero-byte stub in place
    fi
  done
}

stage "$host"
stage x86_64-pc-windows-msvc

# bundle.resources globs binaries/*.dll; an empty glob is a hard error there.
if [ ! -s "$bin/_placeholder.dll" ]; then
  stub "$bin/_placeholder.dll"
fi

echo "Dev stub sidecar binaries staged in src-tauri/binaries/ for host '$host' and x86_64-pc-windows-msvc."
echo "They carry the marker '$STUB_MARKER' and will fail scripts/verify-binaries.ps1."
