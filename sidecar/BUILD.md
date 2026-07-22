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
pip install --require-hashes -r requirements.txt
pip install "pyinstaller>=6.11,<7"   # separate: hash mode rejects unhashed args

pyinstaller --onefile --name convertd \
  --collect-all rapidocr_onnxruntime \
  --collect-all gliclass \
  --collect-all markitdown \
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
- Smoke test before bundling:
  `echo {"id":1,"op":"ping"} | dist/convertd` should print `{"id": 1, "ok": true}`.
