# BackLog portable Windows package

The portable package is the easiest way to move BackLog to another Windows
x64 laptop. It is a ZIP, not an installer: extract it anywhere you can write
files and double-click **`BackLog-Portable.cmd`**.

The ZIP includes the BackLog app, the document-conversion runtime, the CPU
llama server and its runtime DLLs, the verified everyday Qwen3 0.6B model, the
semantic evidence model, and a pinned fixed WebView2 runtime. It does not need
Python, Visual C++ Redistributable, an administrator password, or a separate
WebView2 download to launch.

The fixed runtime pin is WebView2 `151.0.4129.59` x64: the complete CAB is
`304114944` bytes with SHA-256
`056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc`, and it
contains 256 expanded files. `runtime-manifest.json` and
`portable-manifest.json` repeat those values. The release stager downloads the
CAB in 16 MiB HTTP ranges and uses 7za when present, with the pinned
`windows-2022` inbox CAB extractor as a verified equivalent fallback; a single
download response is never accepted.

## Install-free setup

1. Download `BackLog_<version>_x64-portable.zip` from the [GitHub Releases
   page](https://github.com/zgbrenner/backlog/releases).
2. In File Explorer, right-click the ZIP, choose **Extract All**, and extract
   the complete folder. Do not run the app from inside the ZIP preview.
3. Double-click **`BackLog-Portable.cmd`**. This launcher selects the bundled
   WebView2 runtime and starts BackLog from its extracted folder.
4. Complete the normal first-run setup in **Settings**.

Keep the extracted folder together. If Windows reports that a runtime file is
missing, delete that incomplete extraction and extract the ZIP again. The
portable package does not update itself in place; download a newer portable ZIP
when a new release is published.

BackLog stores its ledger, logs, configuration, and downloaded optional models
in the current Windows user's app-data directory, not beside the executable.
Extract the runtime to a local writable NTFS directory; network/UNC locations
are not supported by fixed WebView2 deployment.

The optional Qwen3 1.7B model is not included. It can still be downloaded from
Settings when needed; the everyday 0.6B model is enough to start on an 8 GB
machine. For the measured memory guidance, see [`SIZING.md`](SIZING.md).

## Troubleshooting

- **Nothing happens when I double-click the ZIP.** Extract it first; Windows
  cannot run a portable app reliably from the compressed preview.
- **The launcher says the runtime is missing.** Re-extract the entire ZIP and
  make sure `webview2-fixed` is beside `BackLog.exe`.
- **Windows 10 reports a WebView2 access failure.** The launcher applies the
  read/execute ACLs automatically; fixed WebView2 v120+ can require them on
  Windows 10. If a managed policy blocks that change, run these commands from
  the extracted package directory or use the NSIS installer:

  ```powershell
  icacls .\webview2-fixed /grant "*S-1-15-2-2:(OI)(CI)(RX)" /T
  icacls .\webview2-fixed /grant "*S-1-15-2-1:(OI)(CI)(RX)" /T
  ```
- **Windows shows a SmartScreen warning.** The release may not have a trusted
  Authenticode publisher certificate yet. Only continue when the ZIP came from
  the official BackLog Releases page; see [`SECURITY.md`](SECURITY.md).
- **I need a managed install or automatic updater.** Use the NSIS installer
  from the same release instead. The portable ZIP is deliberately a manual,
  installer-free distribution.

For release validation, run `node scripts/portable-contract.mjs verify` on the
extracted tree and `scripts/validate-portable-package.ps1` on the ZIP. Both
require the launcher, fixed runtime metadata, exact file set, and pinned model
hashes.
