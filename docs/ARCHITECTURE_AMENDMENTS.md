# BackLog architecture amendments

**Effective:** 2026-07-22

This document supersedes conflicting implementation details in
`2026-07-21 Sortition Pipeline Design.md`. The original design remains useful
for the product goals, deterministic trust boundary, evidence-filter strategy,
and staged pilot approach. The decisions below describe the implemented branch.

## Identity and replay

The content SHA-256 is not the physical-file or manifest identity.

- `Sha256` identifies byte content and allows expensive conversion/naming work to
  be reused.
- `InstanceId` identifies one stable physical delivery path.
- `ManifestId` is replay-stable for that instance and is the Power Automate
  idempotency key.
- Filename reservations are transactionally keyed to the physical instance.

This replaces the original statement that the content hash is the correlation
key everywhere.

## Naming models

The distributable runtime uses:

- Qwen3-0.6B Q8_0 GGUF as the primary structured naming tier; and
- Qwen3-1.7B Q8_0 GGUF as the escalation tier.

Both are served locally by llama.cpp over `/v1/chat/completions`. BackLog uses
the GGUF's embedded Jinja chat template, disables the model's thinking mode for
this extraction task, and requests a strict JSON Schema response. The old GBNF
file remains a model-neutral compatibility resource, not the primary decoding
contract.

Liquid text and vision weights are not part of the reviewed installer model
bundle.

## OCR

The scanned-document retry ladder is:

1. RapidOCR at 300 DPI;
2. RapidOCR at 400 DPI; and
3. enhanced grayscale/autocontrast RapidOCR at 600 DPI.

A failed final pass produces `UNREADABLE`. The runtime does not invoke a
vision-language OCR fallback.

## Language detection

Lingua 2.1.1 replaces fastText `lid.176`. It runs entirely inside the frozen
sidecar and returns lower-case ISO 639-1 language codes where available.

## Bundled resources

The Windows installer is built only after receiving:

- a SHA-256-pinned llama-server executable; and
- a SHA-256-pinned model-bundle ZIP containing `models.lock.json`.

Tauri copies the verified Qwen, GLiClass, and Granite assets under the installed
resource directory's `models/` path. Rust resolves relative default model paths
against that directory and injects its parent into `BACKLOG_MODELS_DIR` before
starting `convertd`.

## Runtime reliability

- Watcher events first receive a durable delivery directory.
- Pause waits and replays the still-present delivery rather than consuming its
  only filesystem event.
- Sidecar calls have a deadline; timeout or stream failure kills the process and
  the next request starts a clean instance.
- A failure becomes terminal only after its flagged manifest is durable.
- Existing valid manifests repair ledger state during restart.

## Power Automate boundary

Power Automate remains an I/O and commit layer. It never authors names or
reimplements the deterministic checker. Flow 1 uses stable delivery envelopes;
Flow 2 commits schema-v2 manifests with checkpoints and `ManifestId`
idempotency.
