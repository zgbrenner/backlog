# Semantic Evidence Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a lightweight local semantic paragraph ranker, full-document cached-label entity extraction, measurable and reversible evidence routing, and a guarded BackLog 0.6.0 Windows release.

**Architecture:** Add a torch-free ONNX semantic module to the existing Python sidecar, expose structured ranked-paragraph and entity results through the Rust sidecar client, and assemble the SLM input from exact source-text lanes with compression telemetry and conservative bypass rules. Extend existing CI and exact-commit release workflows rather than creating parallel deployment machinery.

**Tech Stack:** Python 3.11, ONNX Runtime, tokenizers, Rust, Tauri 2, GitHub Actions, PowerShell, Node 22.

## Global Constraints

- No PyTorch, Transformers, sentence-transformers, GLiNER runtime, cloud inference, or runtime model downloads.
- Qwen3-0.6B and optional Qwen3-1.7B remain unchanged; smaller-model benchmarking is out of scope.
- Semantic failures never fail document processing.
- Every selected paragraph and entity must preserve exact source text, source paragraph index, and offsets.
- Successful-document Markdown and trace artifacts are purged unless `retain_cache` is enabled.
- Release artifacts must originate from the exact `main` commit that passed CI.

---

### Task 1: Semantic model and protocol contract

**Files:**
- Create: `sidecar/semantic.py`
- Modify: `sidecar/convertd.py`
- Modify: `sidecar/requirements.txt`
- Modify: `sidecar/requirements.lock`
- Test: `sidecar/tests/test_semantic.py`
- Test: `sidecar/tests/test_convertd_unit.py`

**Interfaces:**
- Produces `rank_paragraphs(paragraphs, probes, top_k, min_score) -> dict` with `available`, `model`, `results`, and metrics.
- Produces `extract_entities(paragraphs, labels, threshold, max_per_label) -> dict` with exact span provenance and cached-label metadata.
- Adds sidecar operations `rank_paragraphs` and `extract_entities`.

- [ ] Write failing tests for relevant paragraph ranking, MMR diversity, unavailable fallback, full-document extraction beyond 8,000 characters, exact offsets, and one-time label embedding.
- [ ] Run the focused Python tests and confirm the new operations are missing.
- [ ] Implement local tokenizer/session loading, normalized embedding, probe/label embedding caches, ranking, deterministic candidate generation, and extraction.
- [ ] Add convertd operation handlers that degrade to structured unavailable results on any optional-model failure.
- [ ] Run all sidecar tests.
- [ ] Commit the semantic sidecar unit.

### Task 2: Rust protocol types and validation

**Files:**
- Modify: `src-tauri/src/sidecar.rs`
- Test: `src-tauri/src/sidecar.rs`

**Interfaces:**
- Produces `SemanticRankResult`, `RankedParagraph`, `EntityExtractionResult`, and `EntitySpan`.
- Produces `Sidecar::rank_paragraphs` and `Sidecar::extract_entities`.

- [ ] Write failing tests that reject out-of-range indices, non-finite scores, and invalid offsets while accepting a valid structured payload.
- [ ] Run the focused Rust tests and confirm failure.
- [ ] Implement typed helpers and boundary validation.
- [ ] Run focused and workspace Rust tests.
- [ ] Commit the protocol unit.

### Task 3: Evidence lanes, routing, and trace

**Files:**
- Modify: `src-tauri/src/filter.rs`
- Modify: `src-tauri/src/config.rs`
- Test: `src-tauri/src/filter.rs`
- Test: `src-tauri/src/config.rs`

**Interfaces:**
- Produces stable source paragraphs, `EvidenceTrace`, `CompressionMetrics`, and lane metrics.
- `Evidence` retains ranked paragraphs and entities for retry reconstruction.

- [ ] Write failing tests for paragraph segmentation, lane ordering, minimum-savings bypass, source provenance, deduplication, UTF-8-safe truncation, and widened retry preservation.
- [ ] Run focused tests and confirm failure.
- [ ] Implement stable paragraph segmentation and semantic calls across the full converted Markdown.
- [ ] Implement conservative routing: bypass when source fits or semantic savings are below the configured floor.
- [ ] Assemble separately budgeted exact-text lanes and calculate compression metrics.
- [ ] Run focused and workspace Rust tests.
- [ ] Commit the filter unit.

### Task 4: Pipeline trace lifecycle and diagnostics

**Files:**
- Modify: `src-tauri/src/pipeline.rs`
- Modify: `src-tauri/src/lib.rs` if review-cache cleanup is centralized there
- Test: `src-tauri/src/pipeline.rs`

**Interfaces:**
- Writes `<sha>.evidence.json` beside `<sha>.md`.
- Logs one concise compression event to the encrypted ledger.
- Purges both artifacts on successful completion unless retention is enabled.

- [ ] Write failing tests for trace persistence, purge behavior, retained-cache behavior, and metric event contents.
- [ ] Run focused tests and confirm failure.
- [ ] Implement atomic trace writes and paired cleanup.
- [ ] Run focused and workspace tests.
- [ ] Commit the lifecycle unit.

### Task 5: Verified model packaging and frozen-sidecar smoke tests

**Files:**
- Modify: `models/download_models.py`
- Modify: `models/models.lock.json`
- Modify: `scripts/stage-release-inputs.ps1`
- Modify: `scripts/build-sidecar.ps1`
- Modify: `scripts/verify-binaries.ps1`
- Modify: `sidecar/BUILD.md`
- Modify: `docs/DEPENDENCY_COMPATIBILITY.md`
- Modify: `docs/SIZING.md`
- Test: `models/tests/test_download_models.py`

**Interfaces:**
- Stages a hash-pinned local ONNX embedding model and tokenizer under the runtime model directory.
- Frozen-sidecar smoke test must prove both new semantic operations use `available: true`.

- [ ] Write failing model-lock and packaging-contract tests.
- [ ] Run model and release-contract tests and confirm failure.
- [ ] Add exact semantic model files and hashes to the model staging contract.
- [ ] Extend PyInstaller collection and smoke tests.
- [ ] Run model tests and release validation.
- [ ] Commit the packaging unit.

### Task 6: CI compliance and supply-chain gates

**Files:**
- Modify: `.github/workflows/ci.yml`
- Create: `.github/workflows/codeql.yml`
- Modify: `scripts/ci-local.sh`
- Modify: `.github/scripts/check-ci-parity.mjs`
- Modify: `package.json`
- Create or modify: scripts required for dependency, license, secret, model-contract, and SBOM checks
- Test: `.github/scripts/*.test.mjs`

**Interfaces:**
- CI must cover semantic unit tests, model contracts, Rust/frontend/Python tests, dependency audits, license policy, secret scan, CodeQL, artifact generation, and workflow self-validation.

- [ ] Write failing workflow-contract tests for required jobs and commands.
- [ ] Run Node release/workflow tests and confirm failure.
- [ ] Add semantic and supply-chain jobs with pinned actions and least-privilege permissions.
- [ ] Keep hosted/local command parity for all locally reproducible checks.
- [ ] Upload test reports and security artifacts without exposing document data.
- [ ] Run all workflow-contract tests.
- [ ] Commit the CI unit.

### Task 7: Version-driven Windows release and 0.6.0 metadata

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `scripts/release-contract.mjs`
- Modify: `scripts/validate-release-workflow.mjs`
- Modify: related release tests
- Modify: `package.json`
- Modify: `package-lock.json`
- Modify: `src-tauri/Cargo.toml`
- Modify: `src-tauri/tauri.conf.json`
- Modify: `CHANGELOG.md` or release documentation
- Modify: `NOTICE.md`

**Interfaces:**
- Release derives `VERSION`, `TAG`, artifact paths, and titles from validated package metadata.
- Release attaches installer, SHA-256 checksums, SBOM, semantic model manifest, and signed updater assets when available.

- [ ] Write failing release-workflow tests for dynamic version derivation and required artifact verification.
- [ ] Run `npm run check:release` and confirm failure.
- [ ] Implement dynamic version setup while preserving exact-SHA gates, draft safety, signed stable behavior, and unsigned prerelease behavior.
- [ ] Bump all version authorities to 0.6.0 and update notices/release notes.
- [ ] Run release-contract, version-agreement, frontend, Rust, and Python checks.
- [ ] Commit the release unit.

### Task 8: Full verification, merge, and publication

**Files:**
- Delete: `.github/workflows/export-source.yml`
- Modify only files required by verification findings.

**Interfaces:**
- Produces a green PR, merged exact commit, and GitHub release with packaged Windows installer.

- [ ] Run the full local CI mirror.
- [ ] Review the complete diff for model licensing, privacy, release safety, and unintended SLM changes.
- [ ] Push the branch and require all PR checks to pass.
- [ ] Delete the temporary source-export workflow and re-run checks.
- [ ] Merge to `main` only after green CI.
- [ ] Verify the main CI result and release workflow target the merged commit.
- [ ] Verify the published BackLog v0.6.0 asset set, checksums, and release mode.
