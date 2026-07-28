#!/usr/bin/env bash
# Create empty placeholder sidecar binaries so the full workspace build/tests run
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
set -euo pipefail

bin="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/binaries"
mkdir -p "$bin"

host="$(rustc -vV | awk '/^host: /{print $2}')"
if [ -z "$host" ]; then
  echo "could not determine host target triple from 'rustc -vV'" >&2
  exit 1
fi

stage() { # stage <triple>
  local triple="$1" ext=""
  case "$triple" in *windows*) ext=".exe" ;; esac
  for tool in convertd llama-server; do
    local f="$bin/${tool}-${triple}${ext}"
    [ -e "$f" ] || : > "$f"
  done
}

stage "$host"
stage x86_64-pc-windows-msvc

# bundle.resources globs binaries/*.dll; an empty glob is a hard error there.
[ -e "$bin/_placeholder.dll" ] || : > "$bin/_placeholder.dll"

echo "Dev stub sidecar binaries staged in src-tauri/binaries/ for host '$host' and x86_64-pc-windows-msvc."
