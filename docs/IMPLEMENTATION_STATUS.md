# BackLog implementation status

**Updated:** 2026-07-28

This branch consolidates the original prototype, the reliability implementation,
and the independent reproducibility contribution.

> **Status (this branch).** Built on the `backlog-core` foundation, which now
> backs five Linux CI jobs (`.github/workflows/ci.yml`). The
> licensing-clean model swap (Qwen3 / Lingua / RapidOCR, replacing LFM2.5 /
> fastText / rapidocr-onnxruntime) described below **has landed on this
> branch**. The runtime-preflight readiness check has now been ported too. The
> prior effort's *instance-aware* review UI was intentionally not adopted; this
> branch keeps its simpler sha256-keyed review flow (Needs-Review -> correct
> fields -> resubmit). (Historical planning docs
> from the prior effort, including `ARCHITECTURE_AMENDMENTS.md`, were not
> carried over here.)
>
> **Slim, torch-free sidecar has also landed.** `torch`, `transformers`,
> `sentence-transformers`, and `gliclass` are removed from
> `sidecar/requirements.in`/`requirements.txt`/`requirements.lock`, cutting the
> sidecar's Python dependency footprint roughly 3x (torch alone was ~500 MB
> installed). This drops the GLiClass doc-type `classify` lane and the
> Granite-embedding `salience` lane from the shipped bundle; both ops in
> `sidecar/convertd.py` degrade to `ok=true` deterministic fallbacks
> (`available: false`) rather than erroring, so the core pipeline (convert,
> OCR, language ID, harvest, naming, checker) is unaffected and no document is
> ever flagged over a missing naming enhancement. `models/download_models.py`
> and `src-tauri/src/model_download.rs` now fetch only the two Qwen3 GGUFs.
> See `docs/DEPENDENCY_COMPATIBILITY.md` for the full rationale.

## Implemented in source

- separate content, physical-delivery, and manifest identities;
- transactional, case-insensitive filename reservations;
- manifest schema v3 (adds the `dismissed` status and a non-empty
  `model_versions` requirement on `ok`) with strict and Power
  Automate-compatible schemas;
- checkpointed Flow 2 instructions and replay-safe Flow 1 instructions;
- Unicode-safe harvesting and deterministic date-evidence validation;
- pause-safe watcher behavior and bounded sidecar communication;
- durable timeout manifests and manifest-before-quarantine ordering;
- runtime configuration validation and live preflight;
- backend-owned running and paused state in the desktop UI;
- accessible review controls and inline deterministic validation errors;
- Qwen3-0.6B primary and Qwen3-1.7B escalation through llama.cpp chat
  completions with JSON Schema output and thinking disabled;
- RapidOCR 3 compatibility, enhanced 600-DPI final retry, and offline Lingua
  language identification;
- removal of Liquid model weights, fastText `lid.176`, shell permission, opener
  permission, and the corresponding unused Rust plugins from the reviewed
  distributable runtime;
- model download and verification tooling that detects missing, changed,
  untracked, and unsafe paths;
- locked npm inputs, a pinned toolchain (`rust-toolchain.toml`), build
  diagnostics, and a Windows NSIS packaging procedure (`RELEASING.md`) whose
  binaries are gated by `scripts/verify-binaries.ps1`;
- in-app, hash-verified downloader (and the equivalent `models/download_models.py`
  staging script) for the two Qwen3 GGUFs and `models.lock.json`, landing under
  the installed app's data directory (the slim, torch-free sidecar profile
  fetches no GLiClass/Granite snapshots -- see `docs/DEPENDENCY_COMPATIBILITY.md`);
- documentation for each audience it has: `docs/USER_GUIDE.md` and
  `docs/TROUBLESHOOTING.md` (the operator), `docs/PRIVACY.md` (the person whose
  documents these are), `docs/SECURITY.md` and `NOTICE.md` (IT and legal),
  `RELEASING.md` + `docs/RELEASE_CHECKLIST.md` (the releaser),
  `docs/DECISIONS.md` and `docs/KNOWN_ISSUES.md` (the next engineer).

## Automated validation

`.github/workflows/ci.yml` runs all of this on ubuntu-latest on every push,
with no Windows, no sidecar binaries and no model weights. Fresh counts from
the current tree:

| Check | Result |
|---|---|
| `cargo test -p backlog-core` | 49 passed |
| `cargo test --workspace --all-targets` | 199 passed (150 app + 49 core) |
| `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings |
| `cargo fmt --all -- --check` | clean |
| `npm run check` (`tsc --noEmit` + `vite build`) | passes |
| `npm run harness:shots` | 13 scenarios, both themes, no console error |
| `python -m pytest sidecar/tests models/tests` | 96 passed, 3 skipped |
| `python power-automate/validate_examples.py` | passes |
| `node .github/scripts/check-versions.mjs` | versions agree |
| `node .github/scripts/check-troubleshooting-coverage.mjs` | 53 user-visible codes, all documented |
| `node .github/scripts/check-troubleshooting-coverage.test.mjs` | the gate fails on 5 removed codes, as intended |
| `node .github/scripts/check-stub-marker.mjs` | marker contract holds across 3 scripts |

## Remaining validation before release

Everything CI cannot reach, because it needs Windows and real artifacts: the
DPAPI key path, the NSIS bundle, install/repair/upgrade/uninstall, and an
end-to-end run with the real sidecars and model weights. The release build is
produced locally on a Windows machine and uploaded manually to a GitHub
Release — see `RELEASING.md` and `docs/RELEASE_CHECKLIST.md`.

## Release gates that cannot be completed in source alone

- produce and review `models/models.lock.json` from the final downloaded bundle;
- generate a hash-pinned `sidecar/requirements.lock` for any signed release;
- build, install, repair, upgrade, and uninstall the Windows NSIS package;
- confirm the installed app resolves its app-data model paths, and that the
  in-app downloader works from an empty models directory;
- observe runtime network traffic with networking disabled and with a connection
  monitor;
- build both Power Automate flows in the target tenant and force checkpoint
  failures;
- execute the 50, 200, and 500-document staged pilot; and
- obtain security, legal, operational, and pilot-owner approval.

Until those gates are complete, this work should remain unmerged/unreleased and
any generated installer must be labeled an unsigned internal pilot candidate.
