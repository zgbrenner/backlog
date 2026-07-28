# Third-party notices

BackLog itself is licensed under `LICENSE` (proprietary, source-available).
This file enumerates every component BackLog **redistributes** — inside the
NSIS installer, inside the frozen `convertd` sidecar, or in the model bundle
the in-app downloader fetches — so the redistribution gate in
`docs/DEPENDENCY_COMPATIBILITY.md` can actually be closed.

Compile-time-only Rust and npm dependencies are covered by the generated SBOMs
required by the same gate; they are not listed individually here.

> **Maintenance rule.** This file is a release artifact. If you add a bundled
> binary, a model file, or a sidecar dependency, add it here in the same
> change. `sidecar/requirements.lock` is the authoritative sidecar list; the
> table below is generated from it and must be regenerated when the lock moves.
> The versions below are those of `sidecar/requirements.lock` as committed.

---

## 1. Model weights (fetched by the in-app downloader or `models/download_models.py`)

| Component | Version / artifact | License | Source |
|---|---|---|---|
| Qwen3-0.6B GGUF | `Qwen3-0.6B-Q8_0.gguf` | Apache-2.0 | <https://huggingface.co/Qwen/Qwen3-0.6B-GGUF> |
| Qwen3-1.7B GGUF | `Qwen3-1.7B-Q8_0.gguf` | Apache-2.0 | <https://huggingface.co/Qwen/Qwen3-1.7B-GGUF> |

Both are quantized redistributions of models released by Alibaba Cloud under
Apache-2.0. Apache-2.0 §4 requires that the license text and any `NOTICE` file
from the upstream repository travel with a redistribution: archive the model
card and `LICENSE` file for the exact revision recorded in
`models/models.lock.json` alongside the release evidence.

The GGUFs are **not** in the installer (`tauri.conf.json`'s `bundle.resources`
maps only `resources/*` and `binaries/*.dll`). They land in
`%APPDATA%\ai.sonomos.backlog\models` at first run. If you instead ship a model
ZIP, that ZIP is a redistribution and carries the same obligation.

## 2. Native binaries in the installer

| Component | Version / artifact | License | Source |
|---|---|---|---|
| llama.cpp `llama-server.exe` | release `b10091`, `llama-b10091-bin-win-cpu-x64.zip` | MIT | <https://github.com/ggml-org/llama.cpp> |
| llama.cpp runtime DLLs (~13: `llama*.dll`, `ggml*.dll`, `mtmd.dll`) | same release | MIT | same |
| `libomp140.x86_64.dll` (LLVM OpenMP runtime, shipped in the same zip) | as distributed by llama.cpp | Apache-2.0 with LLVM exception | <https://llvm.org> |
| Microsoft WebView2 Evergreen runtime installer | bundled by `bundle.windows.webviewInstallMode: offlineInstaller` | Microsoft WebView2 distribution terms | <https://developer.microsoft.com/microsoft-edge/webview2/> |

The exact SHA-256 of every one of these is recorded per release
(`RELEASING.md` step 2, `scripts/verify-binaries.ps1`).

## 3. `convertd` sidecar (PyInstaller-frozen; every dependency is redistributed inside the `.exe`)

Frozen from `sidecar/requirements.lock`. This is the **slim, torch-free**
profile: no `torch`, `transformers`, `sentence-transformers`, or `gliclass`.

| Package | Version | License |
|---|---|---|
| antlr4-python3-runtime | 4.9.3 | BSD-3-Clause |
| anyio | 4.14.2 | MIT |
| beautifulsoup4 | 4.15.0 | MIT |
| certifi | 2026.7.22 | MPL-2.0 |
| cffi | 2.1.0 | MIT |
| charset-normalizer | 3.4.9 | MIT |
| click | 8.4.2 | BSD-3-Clause |
| cobble | 0.1.4 | BSD-2-Clause |
| colorama | 0.4.6 | BSD-3-Clause |
| colorlog | 6.12.0 | MIT |
| cryptography | 49.0.0 | Apache-2.0 OR BSD-3-Clause |
| defusedxml | 0.7.1 | PSF-2.0 |
| et-xmlfile | 2.0.0 | MIT |
| filelock | 3.32.0 | Unlicense |
| flatbuffers | 25.12.19 | Apache-2.0 |
| fsspec | 2026.6.0 | BSD-3-Clause |
| h11 | 0.16.0 | MIT |
| hf-xet | 1.5.2 | Apache-2.0 |
| httpcore | 1.0.9 | BSD-3-Clause |
| httpx | 0.28.1 | BSD-3-Clause |
| huggingface-hub | 1.25.1 | Apache-2.0 |
| idna | 3.18 | BSD-3-Clause |
| lingua-language-detector | 2.1.1 | Apache-2.0 |
| lxml | 6.1.1 | BSD-3-Clause |
| magika | 0.6.3 | Apache-2.0 |
| mammoth | 1.11.0 | BSD-2-Clause |
| markdownify | 1.2.3 | MIT |
| markitdown | 0.1.6 | MIT |
| numpy | 2.4.6 | BSD-3-Clause |
| omegaconf | 2.3.1 | BSD-3-Clause |
| onnxruntime | 1.28.0 | MIT |
| opencv-python | 5.0.0.93 | Apache-2.0 |
| openpyxl | 3.1.5 | MIT |
| packaging | 26.2 | Apache-2.0 OR BSD-2-Clause |
| pandas | 3.0.5 | BSD-3-Clause |
| pdfminer.six | 20251230 | MIT |
| pdfplumber | 0.11.9 | MIT |
| pillow | 12.3.0 | MIT-CMU |
| protobuf | 7.35.1 | BSD-3-Clause |
| pyclipper | 1.4.0 | MIT |
| pycparser | 3.0 | BSD-3-Clause |
| pypdfium2 | 4.30.0 | Apache-2.0 OR BSD-3-Clause (bundles PDFium: BSD-3-Clause) |
| python-dateutil | 2.9.0.post0 | Apache-2.0 OR BSD-3-Clause |
| python-dotenv | 1.2.2 | BSD-3-Clause |
| python-pptx | 1.0.2 | MIT |
| pyyaml | 6.0.3 | MIT |
| rapidocr | 3.9.2 | Apache-2.0 |
| requests | 2.34.2 | Apache-2.0 |
| shapely | 2.1.2 | BSD-3-Clause |
| six | 1.17.0 | MIT |
| soupsieve | 2.9.1 | MIT |
| tqdm | 4.70.0 | MPL-2.0 AND MIT |
| typing-extensions | 4.16.0 | PSF-2.0 |
| tzdata | 2026.3 | Apache-2.0 |
| urllib3 | 2.7.0 | MIT |
| xlrd | 2.0.2 | BSD-3-Clause |
| xlsxwriter | 3.2.9 | BSD-2-Clause |

Plus the CPython 3.11 runtime PyInstaller embeds (PSF-2.0) and PyInstaller's
own bootloader (GPL-2.0 **with** the PyInstaller bootloader exception, which
permits distributing a frozen application under any license).

RapidOCR's ONNX detection/recognition/classification models are packaged inside
the `rapidocr` wheel and are therefore redistributed too; they are Apache-2.0,
from <https://github.com/RapidAI/RapidOCR>.

> The license column is a starting point recorded from each project's declared
> metadata, not a legal opinion. The redistribution gate requires generating an
> SBOM from the actual frozen environment (`pip-licenses` or equivalent against
> the build venv) and archiving the license text of every row.

## 4. Deliberately NOT redistributed

Listed because their absence is a licensing decision, not an oversight — see
`docs/DEPENDENCY_COMPATIBILITY.md` for the reasoning:

- Liquid LFM2.5 text and vision weights (custom license).
- fastText `lid.176.ftz` (CC-BY-SA).
- GLiClass and IBM Granite embedding snapshots (torch-only naming
  enhancements; the ops degrade to deterministic fallbacks without them).
- Ettin encoder 32M (MIT) — a **training** input under `training/`, never
  shipped in the installer.
