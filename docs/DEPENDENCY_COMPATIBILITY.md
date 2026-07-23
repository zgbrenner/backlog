# Dependency compatibility and redistribution review

**Reviewed:** 2026-07-22

BackLog is an offline desktop application, but its installer and model bundle
combine several independently licensed projects. Pin exact artifacts and hashes
for every pilot. Do not publish a model bundle until the license and notice
requirements for the intended audience have been reviewed.

## Compatibility matrix

| Component | BackLog contract | Status and release rule |
|---|---|---|
| Tauri 2 | Target-triple external binaries plus a Windows resource map for locked models | Supported. Windows resources are copied under `$RESOURCE/models`, and Rust resolves relative defaults against the installed resource directory. |
| llama.cpp `llama-server` | Loopback-only `/health` and `/v1/chat/completions`; embedded Jinja chat templates; JSON Schema `response_format` | Supported by current upstream. Record `llama-server --version`, source release, and SHA-256 for every installer. |
| Qwen3-0.6B GGUF | `Qwen3-0.6B-Q8_0.gguf`, primary naming tier | Official Qwen repository, Apache-2.0. Bundle only the exact file recorded in `models.lock.json`. |
| Qwen3-1.7B GGUF | `Qwen3-1.7B-Q8_0.gguf`, escalation tier | Official Qwen repository, Apache-2.0. Same lock and notice rule. |
| RapidOCR | Unified `rapidocr` 3.x result-object API with legacy tuple normalization | Supported. The deprecated `rapidocr-onnxruntime` distribution is not used. The final retry is enhanced 600-DPI classical OCR, not a separately licensed vision-language model. |
| Lingua 2.1.1 | Offline language identification with ISO 639-1 output | Apache-2.0 and Python 3.11 compatible. Packaged inside `convertd`; no external language weight file is downloaded at runtime. |
| GLiClass | `gliclass>=0.1.18,<0.2` with a complete local snapshot | Apache-2.0 model and library. Version floor includes the supported Transformers 5 API surface. |
| Granite embedding small English R2 | Complete local sentence-transformers directory | Apache-2.0. Preserve the complete snapshot and lock every payload file. |
| Ettin encoder 32M | Training base for a locally trained token-classification head | MIT. The raw encoder is not an extractor. Leave the runtime lane disabled until a trained directory passes the documented F1 gates. |
| MarkItDown | `MarkItDown(enable_plugins=False).convert(...).text_content` | Supported by the reviewed 0.1.x range. Review transitive format-parser licenses in the frozen sidecar SBOM. |
| Python sidecar | 64-bit Python 3.11, PyInstaller 6.x, offline Hugging Face/Transformers environment | Windows pilot contract. The app injects the installed model resource directory through `BACKLOG_MODELS_DIR`. |

## Primary upstream references

- Tauri resources: <https://v2.tauri.app/develop/resources/>
- Tauri external binaries: <https://v2.tauri.app/develop/sidecar/>
- llama.cpp server: <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md>
- Qwen3 0.6B GGUF: <https://huggingface.co/Qwen/Qwen3-0.6B-GGUF>
- Qwen3 1.7B GGUF: <https://huggingface.co/Qwen/Qwen3-1.7B-GGUF>
- RapidOCR: <https://github.com/RapidAI/RapidOCR>
- Lingua Python: <https://github.com/pemistahl/lingua-py>
- GLiClass: <https://github.com/Knowledgator/GLiClass>
- Granite embeddings: <https://huggingface.co/ibm-granite/granite-embedding-small-english-r2>
- Ettin: <https://huggingface.co/jhu-clsp/ettin-encoder-32m>
- MarkItDown: <https://github.com/microsoft/markitdown>

## Deliberately excluded from the distributable runtime

- Liquid LFM2.5 text and vision weights, because their custom license requires a
  separate commercial-distribution determination for some organizations.
- `lid.176.ftz` and the fastText language detector, because that prebuilt model
  is distributed under CC-BY-SA terms that complicate an embedded commercial
  installer.
- The old `rapidocr-onnxruntime` Python distribution, because current RapidOCR
  consolidates the runtime API in the `rapidocr` package.

## Redistribution gate

Before any installer or model bundle leaves the internal pilot group:

1. archive the exact model cards and license files represented by the lockfile;
2. confirm commercial distribution rights and required notices;
3. generate software and model bills of materials;
4. record hashes for the installer, model ZIP, sidecars, llama-server, and every
   model payload file;
5. verify the frozen sidecar from a fresh 64-bit Python 3.11 environment;
6. confirm no real customer or Vistage document appears in fixtures, logs,
   release artifacts, screenshots, or training data; and
7. obtain the required legal, security, and pilot-owner approval.

The Windows workflow requires caller-supplied, SHA-256-pinned archives for both
llama-server and the reviewed model bundle. It never silently downloads model
weights while producing an installer.
