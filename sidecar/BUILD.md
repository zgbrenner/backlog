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
  --collect-all rapidocr \
  --collect-all lingua \
  --collect-all markitdown \
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

- This is the slim, torch-free sidecar profile: `requirements.txt` has no
  torch/transformers/sentence-transformers/gliclass, so the frozen binary
  should land well under the ~450 MB a torch-inclusive build used to produce
  (torch alone was ~500 MB installed). The `classify`, `salience`, and
  `ettin_spans` ops still answer `ok=true` -- they degrade to deterministic
  fallbacks (a neutral doc type, document-order sentences, no spans) instead
  of using gliclass/granite/Ettin models, per `convertd.py`'s module
  docstring and its `_gliclass`/`_granite`/`_ettin` loaders. `convert`, `ocr`,
  `langid`, and naming are unaffected. `BACKLOG_ETTIN_DIR` unset (the default)
  already disables the Ettin lane the same way.
- The `rapidocr`/`lingua`/`markitdown`/`pypdfium2` `--collect-all` flags pull
  in their data files (weights loaders, `*.pyi`, native libs, version
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
