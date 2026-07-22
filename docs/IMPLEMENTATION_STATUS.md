# BackLog implementation status

**Updated:** 2026-07-22

This branch consolidates the original prototype, the reliability implementation,
and the independent reproducibility contribution merged from PR #4. The
original completion plan remains in
`docs/superpowers/plans/2026-07-21-finish-backlog.md`; current architecture
changes are recorded in `docs/ARCHITECTURE_AMENDMENTS.md`.

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
- locked npm inputs, pinned Rust 1.97.1 metadata, CI diagnostics, and a
  hash-pinned Windows NSIS packaging workflow;
- Tauri Windows resource mapping for the verified Qwen, GLiClass, Granite, and
  model-lock assets;
- dependency, security, pilot, release, and architecture-amendment documents.

## Locally executed validation

The following validation runs without downloading model weights and has passed
in the available execution environment:

- 12 sidecar protocol, OCR-adapter, date, language-code, and packet tests;
- 6 model-specification and model-lock integrity tests; and
- Python bytecode compilation for the updated sidecar and model tooling.

The CI configuration additionally runs the static frontend/Tauri command smoke
contract, TypeScript build, rustfmt, Clippy with warnings denied, the complete
Rust test suite, Python contract tests, Power Automate schema validation, and a
source snapshot.

## Externally blocked validation

GitHub Actions currently creates jobs that terminate before any job step begins.
The returned jobs have no step list, so the red status is an account, runner, or
billing gate rather than evidence that a repository command failed. Do not mark
CI green or infer a source failure until jobs execute normally.

The current remote Rust integration therefore still requires a fresh successful
run of rustfmt, Clippy, and `cargo test` on the exact PR head. The Windows
workflow also still requires reviewed SHA-256 inputs for llama-server and the
model-bundle ZIP.

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

Until those gates are complete, PR #2 should remain a draft and any generated
installer must be labeled an unsigned internal pilot candidate.
