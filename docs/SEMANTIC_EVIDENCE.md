# Semantic evidence filtering

BackLog reduces converted Markdown before the local naming model sees it, but it does not summarize, paraphrase, or invent replacement text. Every evidence lane contains exact text from the source document or deterministic metadata extracted from it.

## What runs locally

The semantic sidecar uses a quantized ONNX sentence embedding model to rank source paragraphs against document-specific probes. The entity lane compares deterministic full-document candidates against cached label embeddings. Both operations run on the workstation through ONNX Runtime. They have no network fallback.

If the model assets are missing, inference fails, the document already fits the evidence budget, or projected savings are below the configured materiality floor, BackLog bypasses semantic reduction and uses exact deterministic evidence instead.

## Reversible trace

For each filtered document, BackLog writes `<sha256>.evidence.json` beside the cached `<sha256>.md` file. The trace records:

- routing and bypass decisions;
- model availability and model identity;
- selected exact paragraphs with source offsets and scores;
- extracted exact entities with source offsets and confidence;
- per-lane budgets and truncation state;
- source, bundle, and savings measurements.

The trace can contain document text. It therefore follows the same retention rules as cached Markdown and is never written into the long-lived ledger event stream.

The encrypted ledger receives a source-free metrics line only, such as routing, character counts, savings, paragraph counts, and model availability. Names, entities, excerpts, filenames, and document identifiers are excluded.

## Retention and failure behavior

Successful documents delete both cached Markdown and the trace after the manifest is durable unless `retain_cache` is explicitly enabled. Flagged or in-progress documents keep both artifacts so Needs Review can explain exactly what BackLog used. Startup cleanup treats the pair as one retention unit.

Trace writes are atomic. BackLog never exposes a partially written final trace. If the trace cannot be saved, BackLog refuses to process the reduced input, moves the document to Needs Review, and records `TRACE_WRITE_FAILED` rather than silently continuing without an audit trail.

## Trust boundary

Semantic ranking and entity extraction only choose evidence. They do not approve a filename. The local SLM proposes a date, subject, and description from the evidence bundle, and the deterministic Rust checker remains the final authority before any manifest can be emitted.
