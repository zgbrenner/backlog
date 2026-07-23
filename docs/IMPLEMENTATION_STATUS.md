# BackLog implementation status

**Updated:** 2026-07-22

This branch consolidates the original prototype, the reliability implementation,
and the independent reproducibility contribution.

> **Status (this branch).** Built on the CI-free `backlog-core` foundation. The
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
- stable delivery envelopes for manual and Power Automate intake;
- manifest schema v2 with strict and Power Automate-compatible schemas;
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
- locked npm inputs, pinned Rust 1.97.1 metadata, local build diagnostics, and a
  hash-pinned Windows NSIS packaging workflow;
- in-app, hash-verified downloader (and the equivalent `models/download_models.py`
  staging script) for the two Qwen3 GGUFs and `models.lock.json`, landing under
  the installed app's data directory (the slim, torch-free sidecar profile
  fetches no GLiClass/Granite snapshots -- see `docs/DEPENDENCY_COMPATIBILITY.md`);
- dependency, security, pilot, release, and architecture-amendment documents.

## Locally executed validation

The following validation runs without downloading model weights and has passed
in the available execution environment:

- 12 sidecar protocol, OCR-adapter, date, language-code, and packet tests;
- 6 model-specification and model-lock integrity tests; and
- Python bytecode compilation for the updated sidecar and model tooling.

The full local validation set additionally includes the static frontend/Tauri
command smoke contract, `npm run build`, `cargo fmt --manifest-path
src-tauri/Cargo.toml --all -- --check`, `cargo clippy --manifest-path
src-tauri/Cargo.toml --all-targets -- -D warnings`, the complete Rust test
suite (`cargo test -p backlog-core` and `cargo test --workspace`), Python
contract tests, `python power-automate/validate_examples.py`, and a source
snapshot. This project does not use GitHub Actions (no CI minutes are
available); every one of these commands must be run locally before a release
is cut.

## Remaining local validation before release

A fresh, successful local run of `cargo fmt`, `cargo clippy`, and `cargo test`
against the exact release commit is required before packaging, since no
automated CI run backstops the commit. The Windows workflow also still
requires reviewed SHA-256 inputs for llama-server and the model-bundle ZIP.
The release build itself is produced locally on a Windows machine and uploaded
manually to a GitHub Release; it is not built by GitHub Actions.

## Release gates that cannot be completed in source alone

- produce and review `models/models.lock.json` from the final downloaded bundle;
- generate a hash-pinned `sidecar/requirements.lock` for any signed release;
- build, install, repair, upgrade, and uninstall the Windows NSIS package;
- confirm the installed app resolves bundled model resources;
- observe runtime network traffic with networking disabled and with a connection
  monitor;
- build both Power Automate flows in the target tenant and force checkpoint
  failures;
- execute the 50, 200, and 500-document staged pilot; and
- obtain security, legal, operational, and pilot-owner approval.

Until those gates are complete, this work should remain unmerged/unreleased and
any generated installer must be labeled an unsigned internal pilot candidate.
