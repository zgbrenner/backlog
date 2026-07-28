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
  --collect-all magika \
  --collect-all pypdfium2 \
  --collect-all onnxruntime \
  --collect-all cv2 \
  --collect-all shapely \
  --collect-all pyclipper \
  --collect-all pdfminer \
  --collect-all pptx \
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
- `requirements.txt` asks for `markitdown[pdf,docx,pptx,xls,xlsx]`. Base
  `markitdown` ships no document parsers at all -- pdfminer/pdfplumber,
  mammoth+lxml, python-pptx, openpyxl and pandas+xlrd are all extras -- so
  dropping the extras produces a binary that quarantines every Office file it
  is given. `xls` is not optional here either: `routing.rs` sends
  `application/vnd.ms-excel` to `Route::Native`, and markitdown's XLS branch
  is `pd.read_excel(engine="xlrd")`.
- The `--collect-all` flags pull in data files (weights loaders, `*.pyi`,
  native libs, CMap tables, python-pptx's default template, version metadata)
  that PyInstaller otherwise misses, so the frozen binary can load models and
  parse documents at runtime instead of failing on first use. `onnxruntime`,
  `cv2`, `shapely` and `pyclipper` are RapidOCR's native halves and need
  `--collect-all`, not `--hidden-import`.

## Smoke test before bundling

`ping` returns `{}` and every heavy component sits behind a lazy factory, so a
ping-only check passes a build with a missed hidden import or an uncollected
ONNX data file and fails on the customer's first real document. Drive the
three fixtures in `sidecar/fixtures/` through ONE process instead -- that is
one request per lane: markitdown/mammoth, markitdown/pdfminer, and
rapidocr+onnxruntime. `scripts/build-sidecar.ps1` does exactly this and fails
the build on an empty conversion; `sidecar/tests/test_convertd_unit.py`
(`FixtureConversionTests`) runs the same fixtures against an unfrozen
interpreter.

The `\` line-continuation above and the quoting below are POSIX-shell syntax
and DIFFER on Windows.

```
# POSIX shell
printf '%s\n' \
  '{"id":1,"op":"versions"}' \
  '{"id":2,"op":"convert","path":"fixtures/sample_letter.docx"}' \
  '{"id":3,"op":"convert","path":"fixtures/sample_text.pdf"}' \
  '{"id":4,"op":"ocr","path":"fixtures/sample_scan.png","dpi":300}' \
  | dist/convertd
```

Every line must come back `"ok": true`; ids 2 and 3 must carry non-empty
`markdown`, and id 4 must report `"ocr_used": true` with `page_count >= 1`.

Regenerate the fixtures with `python3 sidecar/fixtures/make_fixtures.py`
(standard library only -- it must not need the parsers it exists to test).
