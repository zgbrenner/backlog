# Building the convertd sidecar binary

The Tauri app expects a single-file executable named `convertd` (or
`convertd.exe`) in `src-tauri/binaries/`, matching `externalBin` in
`tauri.conf.json`. Tauri requires target-triple suffixed copies, e.g.
`convertd-x86_64-pc-windows-msvc.exe`.

## Steps (run on the target OS)

```powershell
cd sidecar
python -m venv .venv
.venv\Scripts\activate
python -m pip install --require-virtualenv -r requirements.txt "pyinstaller>=6,<7"

pyinstaller --onefile --name convertd `
  --collect-all rapidocr `
  --collect-all lingua `
  --collect-all gliclass `
  --collect-all markitdown `
  --hidden-import onnxruntime `
  convertd.py

Copy-Item dist\convertd.exe ..\src-tauri\binaries\convertd-x86_64-pc-windows-msvc.exe
```

## Runtime environment variables

- `BACKLOG_MODELS_DIR`: path to the models directory. During development it
  defaults to `../models` relative to the sidecar source tree.
- `BACKLOG_ETTIN_DIR`: path to the fine-tuned Ettin token classifier. Unset
  disables the Ettin lane and the app degrades gracefully.

## Notes

- The full build is large because GLiClass and the optional trained Ettin lane
  use PyTorch. A separately tested slim profile may omit those two lanes while
  retaining MarkItDown conversion, RapidOCR, Lingua language detection,
  Granite salience, and deterministic validation.
- Smoke test before bundling:

  ```powershell
  '{"id":1,"op":"ping"}' | dist\convertd.exe
  ```

  The process must print a single response containing `{"id": 1, "ok": true}`.

## Reproducible release input

`requirements.in` is the reviewed dependency intent. On the Windows release
machine, regenerate a hash-pinned lock with the approved Python 3.11 interpreter:

```powershell
python -m pip install "pip-tools>=7,<8"
pip-compile --generate-hashes --resolver=backtracking --output-file requirements.lock requirements.in
python -m pip install --require-hashes -r requirements.lock
```

Commit the generated lock only after the sidecar protocol tests and installer
smoke test pass with that exact file.
