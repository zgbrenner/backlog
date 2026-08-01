# Semantic Evidence Pipeline Design

## Goal

Upgrade BackLog's existing evidence filter without adding a generic summarizer or a second language-model pass. The shipped pipeline will use a lightweight, fully local semantic paragraph ranker and a cached-label entity extractor to reduce irrelevant context while preserving exact source text, source locations, deterministic evidence lanes, and the current checker-first trust model.

## Constraints

- Keep Qwen3-0.6B as the primary naming model and Qwen3-1.7B as optional escalation. Smaller-model benchmarking is explicitly out of scope.
- Remain fully offline at runtime.
- Keep the Windows package usable on an 8 GB RAM machine with no dedicated GPU.
- Do not add PyTorch, Transformers, sentence-transformers, GLiNER's PyTorch runtime, or a cloud dependency to the shipped sidecar.
- Every semantic result must be attributable to unchanged source text and a source paragraph index. No abstractive summary is permitted in the evidence path.
- Deterministic harvesting and checker validation remain mandatory even when semantic enhancements fail.
- The release package must be built only from the exact main commit that passed CI.

## Architecture

### Semantic runtime

Add a focused `sidecar/semantic.py` module backed by a small ONNX sentence-embedding model loaded from the existing local `BACKLOG_MODELS_DIR`. The module will use ONNX Runtime and a local tokenizer, cache the inference session, cache probe and label embeddings by normalized label set, and expose no network path.

The semantic model serves two bounded operations:

1. **Paragraph ranking.** Markdown is segmented into source paragraphs with stable indices. Paragraph embeddings are compared with document-independent probes and document-type-specific probes. Maximum probe similarity, document-centroid relevance, structural priors, and maximal-marginal-relevance diversity produce a compact set of exact source paragraphs. The result includes score, strongest probe, source index, and character offsets.
2. **Cached-label entity extraction.** Deterministic candidate generation finds dates, identifiers, amounts, subject-like lines, and plausible person/organization spans across the complete converted document. Candidate-plus-context embeddings are compared with cached embeddings for BackLog's fixed legal and business entity labels. High-confidence candidates become exact source spans with label, score, paragraph index, offsets, and optional normalized date.

A GLiNER-compatible architectural idea is retained, namely reusable label embeddings and zero/few-shot labels, without shipping GLiNER's heavier PyTorch stack.

### Conservative routing

The filter always creates deterministic lanes first. Semantic ranking is bypassed when the document already fits the target evidence budget or when the ranked selection saves less than the configured minimum material amount. If the ONNX model is unavailable, inference fails, or confidence is inadequate, BackLog records the reason and continues with deterministic evidence. Semantic compression can never turn a processable document into a processing failure.

### Evidence lanes

The SLM bundle remains a deterministic concatenation of independently budgeted lanes:

1. extracted entities and their source references;
2. dates found in document text;
3. embedded file-metadata dates;
4. subject and header lines;
5. case-caption lines;
6. headings;
7. semantically ranked exact paragraphs;
8. document opening;
9. signature block and ending.

No lane is an unconstrained summary. Duplicate exact text is removed across lanes without removing its provenance.

### Compression trace

Each filter run creates a serializable trace containing:

- source and bundle character counts;
- approximate source and bundle token counts;
- savings count and ratio;
- routing mode and bypass reason;
- semantic model identity and availability;
- selected paragraph indices, offsets, scores, and strongest probes;
- extracted entity source locations and scores;
- lane character allocation and truncation decisions.

The pipeline writes this trace beside the cached Markdown as `<sha>.evidence.json`, records a concise metric event in the encrypted ledger, and deletes the trace with the Markdown after successful emission unless cache retention is explicitly enabled. A flagged document keeps both artifacts for review. This makes every transformation measurable, reversible, and debuggable without retaining successful documents indefinitely.

## Error handling

Semantic initialization and inference failures return structured unavailable results rather than throwing through the pipeline. Invalid indices, non-finite scores, malformed spans, overlapping duplicates, and offsets outside the source paragraph are rejected at the Rust boundary. The existing deterministic filter, naming retry ladder, checker, quarantine behavior, and manifest semantics remain unchanged.

## Testing

- Python unit tests use a deterministic fake embedder to verify ranking relevance, diversity, cached label embeddings, full-document extraction, offset provenance, bypass behavior, and failure fallback.
- Python integration tests exercise the real local ONNX model when its verified assets are present.
- Rust tests verify lane ordering, provenance retention, minimum-savings bypass, compression metrics, UTF-8-safe budgets, trace serialization, widened retry preservation, and invalid sidecar payload rejection.
- The Windows sidecar build smoke test must execute conversion, OCR, real semantic ranking, and real entity extraction in one frozen process.
- CI runs Rust, frontend, Python, manifest, documentation, model-lock, dependency, license, secret, and workflow-contract gates. Security scans produce machine-readable artifacts.

## CI/CD and release

CI remains the single prerequisite for packaging and gains explicit semantic-contract and supply-chain jobs. The release workflow derives its version from the checked-out package metadata, verifies all version files agree, stages only hash-pinned model/runtime inputs, builds and smoke-tests the frozen sidecar, creates an SBOM and checksums, builds the Windows x64 installer, verifies updater signatures when signing keys exist, and publishes either a signed stable release or an explicitly unsigned prerelease. It must attach the installer, checksums, SBOM, semantic model manifest, and updater files appropriate to the release mode.

Release publication remains tied to the exact successful CI commit on `main`; existing draft-retargeting and tag-verification protections are preserved. This feature ships as BackLog 0.6.0.

## Non-goals

- Replacing the current SLMs.
- Training or benchmarking smaller SLMs.
- Abstractive document summarization.
- Adding cloud inference or runtime downloads.
- Replacing the deterministic checker.
