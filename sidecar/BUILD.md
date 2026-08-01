# Building the convertd sidecar binary

The Tauri app expects a PyInstaller **onedir** tree at
`src-tauri/binaries/convertd/` — `convertd.exe` beside its `_internal/`
folder — which `tauri.conf.json` ships through `bundle.resources`, not
`externalBin`. On Windows the resource root is the directory holding the app
executable, so the installed layout is `<install dir>\convertd\convertd.exe`.

It used to be a single `--onefile` executable, and that is why it moved: a
onefile exe unpacks its whole ~250 MB payload into a fresh `%TEMP%\_MEI*`
directory on *every* launch. On a machine whose antivirus scans each unpacked
file that measured 34-52 seconds per start, which is longer than the readiness
probe waits, so a perfectly good install reported its document reader as
blocked — and left a quarter-gigabyte of temp behind each time. A onedir tree
unpacks nothing and starts in about a second. `externalBin` can only carry one
file, hence `bundle.resources`.

## Steps (run on the target OS)

Prefer `scripts/build-sidecar.ps1`, which does all of this, smoke-tests the
result against real fixtures and stages it. The equivalent by hand:

```
cd sidecar
python -m venv .venv
.venv\Scripts\activate            # Windows
pip install -r requirements.txt pyinstaller

pyinstaller --onedir --name convertd \
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

# Windows example: stage the whole tree, not one file. Clear the destination
# first so a stale _internal/ entry cannot survive into a release.
rmdir /s /q ..\src-tauri\binaries\convertd 2>nul
xcopy /e /i /y dist\convertd ..\src-tauri\binaries\convertd
```

## Runtime environment variables

- `BACKLOG_MODELS_DIR`: path to the models directory (defaults to `../models`
  relative to the executable during dev). The semantic evidence lane expects
  `semantic/all-MiniLM-L6-v2/model.onnx` and `vocab.txt` under this directory;
  `sidecar/semantic.py` verifies both against the pinned SHA-256 values before
  creating the ONNX Runtime session.
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
  `langid`, semantic evidence, and naming are unaffected when their required
  local assets are present. `BACKLOG_ETTIN_DIR` unset (the default) already
  disables the Ettin lane the same way.
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
three fixtures in `sidecar/fixtures/` plus two real semantic requests through
ONE process instead -- that is one request per lane: markitdown/mammoth,
markitdown/pdfminer, rapidocr+onnxruntime, semantic paragraph ranking, and
semantic entity extraction. `scripts/build-sidecar.ps1` does exactly this and
fails the build on an empty conversion, a missing or mismatched semantic asset,
or either semantic operation reporting anything other than `available: true`;
`sidecar/tests/test_convertd_unit.py` (`FixtureConversionTests`) runs the same
document fixtures against an unfrozen interpreter.

The `\` line-continuation above and the quoting below are POSIX-shell syntax
and DIFFER on Windows.

```
# POSIX shell
printf '%s\n' \
  '{"id":1,"op":"versions"}' \
  '{"id":2,"op":"convert","path":"fixtures/sample_letter.docx"}' \
  '{"id":3,"op":"convert","path":"fixtures/sample_text.pdf"}' \
  '{"id":4,"op":"ocr","path":"fixtures/sample_scan.png","dpi":300}' \
  '{"id":5,"op":"rank_paragraphs","paragraphs":[{"index":0,"text":"Jane Doe terminates effective July 31, 2026 under the Acme Corporation agreement.","start_char":0,"end_char":83}],"probes":["employment termination date"],"top_k":1,"min_score":0.0}' \
  '{"id":6,"op":"extract_entities","paragraphs":[{"index":0,"text":"Jane Doe terminates effective July 31, 2026 under the Acme Corporation agreement.","start_char":0,"end_char":83}],"threshold":0.0}' \
  | dist/convertd
```

Every line must come back `"ok": true`; ids 2 and 3 must carry non-empty
`markdown`, id 4 must report `"ocr_used": true` with `page_count >= 1`, and
ids 5 and 6 must report `"available": true`.

Regenerate the fixtures with `python3 sidecar/fixtures/make_fixtures.py`
(standard library only -- it must not need the parsers it exists to test).
