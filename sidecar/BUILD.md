# Building the convertd sidecar binary

The Tauri app expects a single-file executable named `convertd` (or
`convertd.exe`) in `src-tauri/binaries/`, matching `externalBin` in
`tauri.conf.json`. Tauri requires target-triple suffixed copies, e.g.
`convertd-x86_64-pc-windows-msvc.exe`.

## Steps (run on the target OS)

```
cd sidecar
python -m venv .venv
.venv\Scripts\activate            # Windows
pip install -r requirements.txt pyinstaller

pyinstaller --onefile --name convertd \
  --collect-all rapidocr_onnxruntime \
  --collect-all gliclass \
  --collect-all markitdown \
  --collect-all torch \
  --collect-all transformers \
  --collect-all sentence_transformers \
  --collect-all pypdfium2 \
  --hidden-import onnxruntime \
  convertd.py

# Windows example:
copy dist\convertd.exe ..\src-tauri\binaries\convertd-x86_64-pc-windows-msvc.exe
```

## Runtime environment variables

- `BACKLOG_MODELS_DIR`: path to the models directory (defaults to `../models`
  relative to the executable during dev).
- `BACKLOG_ETTIN_DIR`: path to the fine-tuned Ettin token classifier. Unset
  disables the Ettin lane; the app degrades gracefully.

## Notes

- First PyInstaller build is slow; expect a 600 MB-1 GB binary because torch
  ships inside. If that offends you (it should), build a second slim profile
  without torch and set `BACKLOG_ETTIN_DIR` empty: gliclass also needs torch,
  so the slim profile drops classify + ettin + VL fallback and keeps
  convert/ocr/langid/salience via onnxruntime only. Slice 2-3 works fully on
  the slim profile.
- The `torch`/`transformers`/`sentence_transformers`/`pypdfium2` `--collect-all`
  flags pull in their data files (weights loaders, `*.pyi`, native libs, version
  metadata) that PyInstaller otherwise misses, so the frozen binary can load
  models at runtime instead of failing on import.
- Smoke test before bundling. The `\` line-continuation above and the `echo`
  quoting below are POSIX-shell syntax and DIFFER on Windows.
  - POSIX shell:
    `echo '{"id":1,"op":"ping"}' | dist/convertd` prints `{"id": 1, "ok": true}`.
  - Windows cmd:
    `echo {"id":1,"op":"ping"} | dist\convertd.exe`
  - Windows PowerShell (`echo` is `Write-Output`; single-quote the JSON so the
    braces and inner double quotes pass through literally):
    `'{"id":1,"op":"ping"}' | dist\convertd.exe`
