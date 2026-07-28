# Dependency compatibility and redistribution review

**Reviewed:** 2026-07-28

> **Status (this branch).** This document describes the *licensing-clean* stack
> (Qwen3 SLM, Lingua language ID, RapidOCR 3) chosen to drop the CC-BY-SA
> fastText `lid.176` model and the Liquid-licensed LFM2.5. **The swap has
> landed**: `sidecar/requirements.txt`, `sidecar/convertd.py`,
> `models/download_models.py`, and `src-tauri/src/{slm,config}.rs` all reflect
> the Qwen3 + Lingua + RapidOCR set described below. Do not publish a public
> model bundle until the redistribution gate at the bottom of this document is
> also complete.
>
> **Slim, torch-free sidecar.** `torch`, `transformers`, `sentence-transformers`,
> and `gliclass` have been **removed** from `sidecar/requirements.in`/
> `requirements.txt` (previously ~1.1 GB of the sidecar's Python
> dependencies, torch alone ~500 MB installed). This drops the GLiClass
> doc-type `classify` lane and the Granite-embedding `salience` lane; the
> Ettin span lane was already optional and disabled by default
> (`BACKLOG_ETTIN_DIR` unset). `sidecar/convertd.py`'s `_gliclass`/`_granite`/
> `_ettin` loaders catch the resulting `ImportError` (or a missing/corrupt
> local model snapshot) and cache it as "unavailable"; `op_classify` returns
> `ok=true` with a neutral default label (`"correspondence"`, `score: 0.0`,
> `available: false`), `op_salience` returns `ok=true` with the first `top_k`
> sentence indices in document order (`available: false`), and
> `op_ettin_spans` returns `ok=true` with `{"spans": []}`. No op ever returns
> `ok=false` over a missing enhancement, so `src-tauri/src/filter.rs`'s
> `build_evidence` never flags a document over it: naming quality degrades
> slightly (a generic doc-type hint, unranked evidence) but conversion, OCR,
> language ID, and naming keep working. `models/download_models.py` and
> `src-tauri/src/model_download.rs::MODEL_FILES` fetch only the two Qwen3
> GGUFs now.

BackLog is an offline desktop application, but its installer and model bundle
combine several independently licensed projects. Pin exact artifacts and hashes
for every pilot. Do not publish a model bundle until the license and notice
requirements for the intended audience have been reviewed.

## Compatibility matrix

| Component | BackLog contract | Status and release rule |
|---|---|---|
| Tauri 2 | Target-triple external binaries (`externalBin`) plus a resource map for `resources/*` and the llama runtime DLLs | Supported. **Models are not installer resources.** `bundle.resources` maps only `resources/*` and `binaries/*.dll`; `lib.rs` rehomes both GGUF paths to `app_data_dir()/models` (`%APPDATA%\ai.sonomos.backlog\models`) at startup, which is where the in-app downloader and `BACKLOG_MODELS_DIR` also point. A path a user set through Settings' Browse dialog passes through untouched. |
| llama.cpp `llama-server` | Loopback-only `/health` and `/v1/chat/completions`; embedded Jinja chat templates; **`response_format: {"type": "json_schema"}` and `chat_template_kwargs`** | Verified against release `b10091`. Both request keys are required, not preferred: a build that accepts and ignores them yields free text, the checker rejects every proposal, and every document ends in `SLM_FAIL` blaming the model. Record `llama-server --version`, source release, and SHA-256 for every installer, plus the ~13 runtime DLLs the `.exe` loads. |
| Qwen3-0.6B GGUF | `Qwen3-0.6B-Q8_0.gguf`, primary naming tier | Official Qwen repository, Apache-2.0. Bundle only the exact file recorded in `models.lock.json`. |
| Qwen3-1.7B GGUF | `Qwen3-1.7B-Q8_0.gguf`, escalation tier | Official Qwen repository, Apache-2.0. Same lock and notice rule. |
| RapidOCR | Unified `rapidocr` 3.x result-object API with legacy tuple normalization | Supported. The deprecated `rapidocr-onnxruntime` distribution is not used. The final retry is enhanced 600-DPI classical OCR, not a separately licensed vision-language model. |
| Lingua 2.1.1 | Offline language identification with ISO 639-1 output | Apache-2.0 and Python 3.11 compatible. Packaged inside `convertd`; no external language weight file is downloaded at runtime. |
| GLiClass | Not shipped in the slim sidecar profile | Removed from `sidecar/requirements.txt`. `sidecar/convertd.py::op_classify` degrades to `ok=true` with a neutral default label (`available: false`) when gliclass/transformers are absent, which they always are on this profile. See "Deliberately excluded" below. |
| Granite embedding small English R2 | Not shipped in the slim sidecar profile | Removed from `sidecar/requirements.txt` (needs sentence-transformers, which needs torch). `sidecar/convertd.py::op_salience` degrades to `ok=true` with document-order sentence indices (`available: false`). See "Deliberately excluded" below. |
| Ettin encoder 32M | Training base for a locally trained token-classification head | MIT. The raw encoder is not an extractor. Leave the runtime lane disabled until a trained directory passes the documented F1 gates. `sidecar/convertd.py::op_ettin_spans` returns `ok=true` with `{"spans": []}` whenever `BACKLOG_ETTIN_DIR` is unset (the default) or transformers is absent (always true on the slim profile). |
| MarkItDown | `MarkItDown(enable_plugins=False).convert(...).text_content` | Supported by the reviewed 0.1.x range. Review transitive format-parser licenses in the frozen sidecar SBOM. |
| Python sidecar | 64-bit Python 3.11 **exactly** (`scripts/build-sidecar.ps1` throws otherwise; onnxruntime and rapidocr publish no 3.13/3.14 wheels), PyInstaller 6.x, offline Hugging Face Hub environment, **no torch/transformers/sentence-transformers/gliclass** (the slim, torch-free profile) | Windows pilot contract. The app injects the app-data models directory through `BACKLOG_MODELS_DIR`. `huggingface_hub` stays a listed dependency (it's a lightweight pure-Python HTTP client with no torch pull-through) even though `convertd.py` doesn't import it directly today; `HF_HUB_OFFLINE`/`TRANSFORMERS_OFFLINE`/`HF_DATASETS_OFFLINE` remain forced regardless. |

## Primary upstream references

- Tauri resources: <https://v2.tauri.app/develop/resources/>
- Tauri Windows installer options (`webviewInstallMode`, `nsis.installMode`):
  <https://v2.tauri.app/distribute/windows-installer/>
- Tauri external binaries: <https://v2.tauri.app/develop/sidecar/>
- llama.cpp server: <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md>
- Qwen3 0.6B GGUF: <https://huggingface.co/Qwen/Qwen3-0.6B-GGUF>
- Qwen3 1.7B GGUF: <https://huggingface.co/Qwen/Qwen3-1.7B-GGUF>
- RapidOCR: <https://github.com/RapidAI/RapidOCR>
- Lingua Python: <https://github.com/pemistahl/lingua-py>
- GLiClass (not shipped; naming enhancement only): <https://github.com/Knowledgator/GLiClass>
- Granite embeddings (not shipped; naming enhancement only): <https://huggingface.co/ibm-granite/granite-embedding-small-english-r2>
- Ettin (not shipped; optional trained lane only): <https://huggingface.co/jhu-clsp/ettin-encoder-32m>
- MarkItDown: <https://github.com/microsoft/markitdown>

## Deliberately excluded from the distributable runtime

- Liquid LFM2.5 text and vision weights, because their custom license requires a
  separate commercial-distribution determination for some organizations.
- `lid.176.ftz` and the fastText language detector, because that prebuilt model
  is distributed under CC-BY-SA terms that complicate an embedded commercial
  installer.
- The old `rapidocr-onnxruntime` Python distribution, because current RapidOCR
  consolidates the runtime API in the `rapidocr` package.
- `torch`, `transformers`, `sentence-transformers`, and `gliclass` (and, by
  extension, the GLiClass doc-type classifier and Granite embedding model),
  because dropping them cuts the sidecar's Python dependency footprint by
  roughly 3x (torch alone was ~500 MB installed) and they are only used by
  optional naming enhancements. `sidecar/convertd.py`'s `classify` and
  `salience` ops degrade to deterministic, dependency-free fallbacks instead
  of failing -- see the "Slim, torch-free sidecar" note at the top of this
  document.

## Redistribution gate

Before any installer or model bundle leaves the internal pilot group:

0. **`NOTICE.md` enumerates every redistributed component** — the two Apache-2.0
   Qwen3 GGUFs, the MIT llama.cpp binaries and their DLLs, the embedded WebView2
   runtime, and the full PyInstaller-frozen dependency set from
   `sidecar/requirements.lock`. Confirm it still matches this build;
1. archive the exact model cards and license files represented by the lockfile;
2. confirm commercial distribution rights and required notices;
3. generate software and model bills of materials;
4. record hashes for the installer, model ZIP, sidecars, llama-server, and every
   model payload file;
5. verify the frozen sidecar from a fresh 64-bit Python 3.11 environment;
6. confirm no real customer or Vistage document appears in fixtures, logs,
   release artifacts, screenshots, or training data; and
7. obtain the required legal, security, and pilot-owner approval.

The release procedure (`RELEASING.md`) requires SHA-256-pinned archives for
llama-server (Build step 2, hash checked inline before extraction) and for the model
bundle (`models.lock.json`, verified by `models/download_models.py
--verify-only`). `npm run tauri build` never downloads model weights: the
models are not installer resources at all — they reach the machine through the
in-app downloader or a hand copy into `%APPDATA%\ai.sonomos.backlog\models`
(Build step 6). `scripts/verify-binaries.ps1` (Build step 4) is the gate that stops a
dev-stubbed or truncated binary reaching the bundle.
