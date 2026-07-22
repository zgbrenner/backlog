# Dependency compatibility and redistribution review

**Reviewed:** 2026-07-21

BackLog is an offline desktop application, but its installer and model bundle
combine several independently licensed projects. Pin exact release artifacts
and hashes for every pilot. Do not publish or redistribute model weights until
their license terms have been reviewed for the intended audience.

## Compatibility matrix

| Component | BackLog contract | Status and release rule |
|---|---|---|
| Tauri 2 | External binaries named with the target triple in `src-tauri/binaries` | Supported. The installed name is resolved without the build suffix. |
| llama.cpp `llama-server` | `/health`, `/v1/chat/completions`, JSON-schema `response_format`; `/completion` + GBNF fallback | Supported by current upstream. Record `llama-server --version` and SHA-256 for each build. |
| LFM2.5-350M GGUF | `LFM2.5-350M-Q8_0.gguf` | Repository and filename verified. Review the LFM Open License before redistribution. |
| LFM2.5-1.2B-Instruct GGUF | `LFM2.5-1.2B-Instruct-Q4_K_M.gguf` | Repository and filename verified. Same license gate. |
| LFM2.5-VL-450M-Extract | Local Transformers model with `trust_remote_code=True` | Requires Transformers 5.1 or newer and the complete pinned snapshot. Optional. |
| RapidOCR | Unified `rapidocr` 3.x result-object API | Supported. The older `rapidocr-onnxruntime` package is intentionally not used. |
| GLiClass | `gliclass>=0.1.18,<0.2` | Version floor includes Transformers 5 compatibility. |
| Granite embedding small English R2 | Local sentence-transformers directory | Supported. Keep the complete snapshot and hash lock. |
| Ettin encoder 32M | Base model for a locally trained token-classification head | The raw encoder is not an extractor. Leave disabled until a trained directory passes the documented ship gates. |
| MarkItDown | `MarkItDown(enable_plugins=False).convert(...).text_content` | Supported by the pinned 0.1.x range. |
| Python sidecar | 64-bit Python 3.11 frozen with PyInstaller 6.x | Windows pilot build contract. Runtime model paths are injected by Rust. |

## Primary upstream references

- Tauri external binaries: <https://v2.tauri.app/develop/sidecar/>
- llama.cpp server: <https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md>
- llama.cpp grammars: <https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md>
- Liquid LFM2.5 350M GGUF: <https://huggingface.co/LiquidAI/LFM2.5-350M-GGUF>
- Liquid LFM2.5 1.2B GGUF: <https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct-GGUF>
- Liquid VL Extract: <https://huggingface.co/LiquidAI/LFM2.5-VL-450M-Extract>
- RapidOCR: <https://github.com/RapidAI/RapidOCR>
- GLiClass: <https://github.com/Knowledgator/GLiClass>
- Granite embeddings: <https://huggingface.co/ibm-granite/granite-embedding-small-english-r2>
- Ettin: <https://huggingface.co/jhu-clsp/ettin-encoder-32m>
- MarkItDown: <https://github.com/microsoft/markitdown>

## Redistribution gate

Before any installer or model bundle leaves the internal pilot group:

1. archive the exact model cards and licenses used by the lockfile;
2. confirm whether each license permits the intended commercial distribution;
3. include all required notices and attribution;
4. produce a software and model bill of materials;
5. record hashes for the installer, sidecars, llama-server, and every model file;
6. confirm that no real customer or Vistage document was included in fixtures,
   logs, release artifacts, or training data; and
7. obtain the appropriate legal and security approval.

The GitHub Windows workflow intentionally accepts a caller-supplied,
hash-pinned llama-server binary and does not silently download or republish
model weights.
