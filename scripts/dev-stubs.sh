#!/usr/bin/env bash
# Create empty placeholder sidecar binaries so the full workspace build/tests run
# on a fresh checkout. tauri-build validates externalBin + the bundle.resources
# DLL glob at compile time; those files are gitignored (built via
# scripts/build-sidecar.ps1 / downloaded per RELEASING.md). The trust core needs
# none of this: `cargo test -p backlog-core`.
set -euo pipefail
bin="$(cd "$(dirname "$0")/.." && pwd)/src-tauri/binaries"
mkdir -p "$bin"
for f in convertd-x86_64-pc-windows-msvc.exe llama-server-x86_64-pc-windows-msvc.exe _placeholder.dll; do
  [ -e "$bin/$f" ] || : > "$bin/$f"
done
echo "Dev stub sidecar binaries created in src-tauri/binaries/."
