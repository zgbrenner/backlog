# Qwen3 1.7B Primary Quality Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Determine with a controlled hard-corpus A/B whether Qwen3-1.7B should serve every naming attempt on the 14 GiB-class target, and ship adaptive signed BackLog 0.11.0 only if it clears the approved quality and performance gates.

**Architecture:** Add a deterministic synthetic hard-corpus generator, ground-truth scorer, and Windows A/B runner comparing the current 0.6B/1.7B ladder with a collapsed 1.7B/1.7B ladder under identical evidence and concurrency. If and only if the candidate wins, add one backend effective-model-pair selector used by runtime startup, readiness, and diagnostics; keep persisted paths and the bundled 0.6B fallback intact.

**Tech Stack:** Python 3.11, pytest, PowerShell, Rust/Tauri, Tokio, llama.cpp CPU server, Qwen3 Q8_0 GGUFs, TypeScript/Vite, GitHub Actions and GitHub Releases.

## Global Constraints

- Candidate total wall time must be no more than 2.0x control.
- Composite quality must improve by at least 5 percentage points and win all three paired passes.
- Critical factual errors must decrease, with no critical-error category regression.
- Prompts, evidence budgets, checker rules, conversion behavior, `slm_parallel=1`, and `convert_workers=3` stay identical between arms.
- Candidate is Qwen3-1.7B for all attempts; no 4B model is used.
- Do not download automatically or add 1.7B to the installer/portable ZIP.
- At or below 9 GiB, retain collapsed 0.6B. Above 9 GiB, prefer canonical 1.7B only when installed.
- Preserve explicit custom paths and fail-closed checker, path, manifest, and release guarantees.
- Do not merge or publish 0.11.0 unless every decision gate passes.
- Report the 32 GB test host separately from deterministic 14 GiB tier coverage.

## File Map

| Responsibility | Files |
|---|---|
| Hard fixtures | `scripts/model-quality/generate_hard_corpus.py`, `hard-corpus-manifest.json`, generator tests |
| Scoring | `scripts/model-quality/score_ab.py`, scorer tests |
| Six-run orchestration | `scripts/model-quality/run_ab.ps1`, runner contract tests, optional report support in `pipeline.rs` |
| Frozen result | `docs/model-quality/qwen17-primary-ab-2026-08-10.{json,md}` |
| Runtime selection | `src-tauri/src/config.rs`, `lib.rs`, `preflight.rs` |
| UI/docs | `src/main.ts`, UI harness fixtures/assertions, model/user/security/sizing docs |
| Release | changelog and all five version authorities |

---

### Task 1: Deterministic Stratified Hard Corpus

**Files:**
- Create: `scripts/model-quality/generate_hard_corpus.py`
- Create: `scripts/model-quality/hard-corpus-manifest.json`
- Create: `scripts/model-quality/tests/test_generate_hard_corpus.py`

**Interfaces:**
- Produces `generate_corpus(output_dir: Path) -> list[dict[str, object]]`.
- CLI: `python scripts/model-quality/generate_hard_corpus.py --output <dir> --manifest <path>`.
- Each ground-truth row has `id`, `filename`, `format`, `stratum`, allowed/controlling parties, forbidden party combinations, allowed date/source pairs, document types, required description facts, and forbidden description facts.

- [ ] **Step 1: Write failing balance and determinism tests**

```python
def test_manifest_has_balanced_strata(tmp_path):
    rows = generate_corpus(tmp_path)
    assert len(rows) == 60
    counts = Counter(row["stratum"] for row in rows)
    assert len(counts) == 5
    assert set(counts.values()) == {12}

def test_generation_is_byte_deterministic(tmp_path):
    generate_corpus(tmp_path / "a")
    generate_corpus(tmp_path / "b")
    assert tree_hash(tmp_path / "a") == tree_hash(tmp_path / "b")
```

- [ ] **Step 2: Confirm tests fail before implementation**

Run: `python -m pytest scripts/model-quality/tests/test_generate_hard_corpus.py -q`

Expected: import failure because the generator does not exist.

- [ ] **Step 3: Implement the 60-fixture generator**

Use `random.Random(170060)` for deterministic template choices. Generate 12 fixtures for each approved stratum and a balanced TXT/text-PDF/DOCX/scanned-PDF mix. Normalize DOCX ZIP timestamps so repeat output is byte-identical. Never use user documents.

```python
STRATA = ("ambiguous_parties", "deep_dates", "long_evidence",
          "conflicting_context", "description_grounding")

def generate_corpus(output_dir: Path) -> list[dict[str, object]]:
    rows = [build_fixture(s, i) for s in STRATA for i in range(12)]
    for row in rows:
        write_fixture(output_dir, row)
    return rows
```

- [ ] **Step 4: Run tests and generate committed ground truth**

Run: `python -m pytest scripts/model-quality/tests/test_generate_hard_corpus.py -q`

Run: `python scripts/model-quality/generate_hard_corpus.py --output "$env:TEMP\backlog-qwen17-hard-corpus" --manifest scripts/model-quality/hard-corpus-manifest.json`

- [ ] **Step 5: Commit**

```powershell
git add scripts/model-quality
git commit -m "test: add stratified naming quality corpus"
```

---

### Task 2: Deterministic Paired Scorer

**Files:**
- Create: `scripts/model-quality/score_ab.py`
- Create: `scripts/model-quality/tests/test_score_ab.py`

**Interfaces:**
- Consumes Task 1 ground truth and `Outbox/_manifests/*.json` only.
- Produces `score_run(ground_truth, outbox) -> RunScore` and `compare_runs(control, candidate) -> Comparison`.
- CLI writes canonical JSON and Markdown reports.

- [ ] **Step 1: Write failing content and gate tests**

```python
def test_fabricated_party_is_critical(fixture, emitted):
    emitted["new_filename"] = "2026-04-02 Invented Holdings - Agreement.pdf"
    score = score_document(fixture, emitted)
    assert not score.subject_faithful
    assert "fabricated_entity" in score.critical_errors

def test_two_x_is_inclusive(comparison):
    comparison.control_total_seconds = 100.0
    comparison.candidate_total_seconds = 200.0
    assert evaluate_gate(comparison).throughput_pass
```

- [ ] **Step 2: Confirm tests fail**

Run: `python -m pytest scripts/model-quality/tests/test_score_ab.py -q`

- [ ] **Step 3: Implement normalized ground-truth-only scoring**

```python
@dataclass(frozen=True)
class DocumentScore:
    fixture_id: str
    composite_pass: bool
    subject_faithful: bool
    controlling_party: bool
    date_correct: bool
    document_type: bool
    description_recall: bool
    description_faithful: bool
    completed: bool
    critical_errors: tuple[str, ...]
```

Use Unicode NFKC, lowercase, and punctuation/whitespace collapse. Match only aliases declared in ground truth. Never read original filenames or document bodies. Keep completion outside the composite denominator so flagging hard inputs cannot inflate quality.

- [ ] **Step 4: Implement decision output**

Include pass scores, pooled rates, paired disagreements, percentage-point delta, critical counts by category, total ratio, median/p95, process peaks, differing outputs, each gate result, `promote`, and reasons.

- [ ] **Step 5: Run tests and commit**

Run: `python -m pytest scripts/model-quality/tests/test_score_ab.py -q`

```powershell
git add scripts/model-quality/score_ab.py scripts/model-quality/tests/test_score_ab.py
git commit -m "test: add paired naming quality scorer"
```

---

### Task 3: Windows A/B Runner and Provenance

**Files:**
- Create: `scripts/model-quality/run_ab.ps1`
- Create: `scripts/model-quality/tests/test_run_contract.py`
- Modify: `src-tauri/src/pipeline.rs:5650-5900`

**Interfaces:**
- Produces six isolated run directories under explicit `-WorkRoot`.
- Adds optional `BACKLOG_E2E_REPORT` output containing per-file state/timing without document text, evidence, or descriptions.

- [ ] **Step 1: Write failing runner contract tests**

```python
def test_arm_order_alternates():
    assert parse_arm_order(RUNNER) == [
        "control", "candidate", "candidate", "control", "control", "candidate"
    ]

def test_runner_uses_current_sidecar_and_single_slots():
    text = RUNNER.read_text(encoding="utf-8")
    assert "convertd\\convertd.exe" in text
    assert "BACKLOG_E2E_PARALLEL" in text and "'1'" in text
    assert "BACKLOG_E2E_WORKERS" in text and "'3'" in text
```

- [ ] **Step 2: Add a failing Rust report-serialization test**

Assert report JSON contains original fixture name, terminal state, elapsed milliseconds, flag code, and manifest path, while excluding document text/evidence/description.

- [ ] **Step 3: Implement optional harness reporting**

Keep behavior byte-identical when `BACKLOG_E2E_REPORT` is absent. Atomically write after all tasks join when present.

- [ ] **Step 4: Implement `run_ab.ps1`**

Resolve and validate all paths before cleaning run-owned directories. Hash GGUFs, llama EXE/DLLs, grammar, convertd, semantic assets, Git commit, and dirty state. Probe `versions` and semantic availability. Sample aggregate private bytes/working set for llama/convertd.

Control uses primary 0.6B and escalation 1.7B. Candidate points both env paths to 1.7B. Both use parallel 1 and workers 3.

- [ ] **Step 5: Run focused tests and commit**

Run: `python -m pytest scripts/model-quality/tests/test_run_contract.py -q`

Run: `$env:CARGO_BUILD_JOBS='1'; $env:CARGO_PROFILE_DEV_DEBUG='0'; cargo test -p backlog --lib e2e_report --locked`

```powershell
git add scripts/model-quality src-tauri/src/pipeline.rs
git commit -m "test: orchestrate reproducible Qwen primary A/B"
```

---

### Task 4: Execute Real A/B and Freeze Decision

**Files:**
- Create after completion: `docs/model-quality/qwen17-primary-ab-2026-08-10.json`
- Create after completion: `docs/model-quality/qwen17-primary-ab-2026-08-10.md`

- [ ] **Step 1: Preflight exact real inputs**

Require GGUF hashes matching `models.lock.json`, current onedir convertd with semantic capability available, schema-capable llama, a clean tree, and runner-calculated free disk headroom.

- [ ] **Step 2: Run three pass pairs**

```powershell
& scripts/model-quality/run_ab.ps1 -WorkRoot 'C:\Users\zgbre\backlog-e2e\qwen17-primary-20260810' -Primary06 "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-0.6B-Q8_0.gguf" -Quality17 "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-1.7B-Q8_0.gguf" -Convertd "$env:LOCALAPPDATA\BackLog\convertd\convertd.exe" -Llama "$env:LOCALAPPDATA\BackLog\llama-server.exe"
```

Expected: six runs, 60 terminal documents each, no missing manifests, no degraded semantic lane.

- [ ] **Step 3: Score once and inspect differences blind**

Freeze scorer output before inspecting differing proposals. A ground-truth defect invalidates both paired observations and requires rerunning affected passes; never patch aliases based on which arm won.

- [ ] **Step 4: Apply the gate**

If `promote` is false, stop Tasks 5-8, do not merge, and do not release. If true, commit both reports.

```powershell
git add docs/model-quality
git commit -m "docs: record Qwen 1.7B primary A/B result"
```

---

### Task 5: Adaptive Effective-Model Selector (Only if Promote)

**Files:**
- Modify: `src-tauri/src/config.rs:896-938,1210-1335`
- Modify: `src-tauri/src/lib.rs:1140-1435`
- Modify: `src-tauri/src/preflight.rs:61-570`

**Interfaces:**
- Produces `NamingPolicy::{Primary06, Adaptive17, Custom}`.
- Produces `EffectiveModelPair<'a> { primary: &'a Path, escalation: &'a Path, policy: NamingPolicy }`.
- Produces `Config::effective_model_pair_for_ram(gib)` and `effective_model_pair()`.

- [ ] **Step 1: Write failing tests**

Cover 9/10/14/17/18 GiB, optional absent/present, canonical and custom paths. Assert 14 GiB plus canonical installed 1.7B resolves both slots to 1.7B; absence stays on functional 0.6B; custom paths remain unchanged.

- [ ] **Step 2: Confirm focused tests fail**

Run: `$env:CARGO_BUILD_JOBS='1'; $env:CARGO_PROFILE_DEV_DEBUG='0'; cargo test -p backlog --lib config::tests::effective_model --locked`

- [ ] **Step 3: Implement selector and use it at production `SlmLane::new` sites**

```rust
if canonical && gib.is_some_and(|value| value > 9) && quality_path.is_file() {
    return EffectiveModelPair::collapsed(quality_path, NamingPolicy::Adaptive17);
}
```

Do not mutate persisted paths. Preserve existing fallback and custom-path handling.

- [ ] **Step 4: Extend readiness and diagnostics**

Expose policy and effective basenames, never absolute paths. Missing 1.7B remains a warning/download action, not a readiness failure.

- [ ] **Step 5: Run focused tests and commit**

Run: `$env:CARGO_BUILD_JOBS='1'; $env:CARGO_PROFILE_DEV_DEBUG='0'; cargo test -p backlog --lib config::tests preflight::tests lib::tests --locked`

```powershell
git add src-tauri/src/config.rs src-tauri/src/lib.rs src-tauri/src/preflight.rs
git commit -m "feat: prefer installed Qwen 1.7B on capable machines"
```

---

### Task 6: UI and Documentation (Only if Promote)

**Files:**
- Modify: `src/main.ts`, `scripts/ui-harness/fixtures.ts`, `scripts/ui-harness/shoot.mjs`
- Modify: `README.md`, `docs/USER_GUIDE.md`, `docs/TROUBLESHOOTING.md`, `docs/SIZING.md`, `docs/PORTABLE.md`, `docs/PRIVACY.md`, `docs/SECURITY.md`

- [ ] **Step 1: Add failing UI harness assertions**

Installed scenario must say the quality model is active and may take up to twice as long. Missing scenario must say BackLog remains ready on the everyday model and offers the optional download.

- [ ] **Step 2: Update frontend types and copy**

Replace user-facing “backup model” with “quality model” without renaming persisted config/IPC fields. Show effective policy and basenames in diagnostics.

- [ ] **Step 3: Update docs with exact measured result and boundaries**

Record A/B scores, throughput, adaptive threshold, unchanged offline/privacy/release contracts, modeled 14 GiB coverage, and lack of physical 14 GB/real-tenant testing.

- [ ] **Step 4: Verify and commit**

Run: `npm run check`

Run: `npm run harness:shots`

Run: `npm run check:release`

```powershell
git add src/main.ts scripts/ui-harness README.md docs
git commit -m "docs: explain adaptive Qwen quality mode"
```

---

### Task 7: Version 0.11.0 and Full Verification (Only if Promote)

**Files:**
- Modify: `CHANGELOG.md`, `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock`, `src-tauri/tauri.conf.json`

- [ ] **Step 1: Bump all version authorities to 0.11.0**

Add a dated changelog separating measured synthetic quality, throughput, deterministic 14 GiB coverage, and remaining physical/tenant boundaries.

- [ ] **Step 2: Run full verification sequentially**

Run: `npm ci; npm audit --audit-level=high; npm run check; npm run check:release; npm run harness:shots`

Run: `python -m pytest sidecar/tests models/tests scripts/model-quality/tests -q`

Run: `python power-automate/validate_examples.py`

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'
$env:CARGO_PROFILE_DEV_DEBUG='0'
cargo fmt --check --manifest-path src-tauri/Cargo.toml
cargo test --workspace --manifest-path src-tauri/Cargo.toml --locked
cargo clippy --workspace --all-targets --all-features --manifest-path src-tauri/Cargo.toml --locked -- -D warnings
cargo audit --file src-tauri/Cargo.lock
```

Run: `node .github/scripts/check-versions.mjs`

- [ ] **Step 3: Obtain independent final review**

Provide design, plan, immutable report, diff, test evidence, and tree hash. After any material fix, rerun affected gates and obtain a fresh ship verdict.

- [ ] **Step 4: Commit release metadata**

```powershell
git add CHANGELOG.md package.json package-lock.json src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/tauri.conf.json
git commit -m "release: prepare BackLog 0.11.0"
```

---

### Task 8: GitHub Merge and Signed Release (Only if Promote)

- [ ] **Step 1: Push branch and open PR**

Include A/B gate table, exact commit/tree, local checks, physical-hardware limitation, and tenant-flow boundary. Never force-push.

- [ ] **Step 2: Require exact-head checks**

Wait for frontend, Rust workspace, trust core, sidecar/tooling, version/release, CodeQL, and policy success. Fix failures on the branch without bypassing protection.

- [ ] **Step 3: Fresh final review and squash merge**

Confirm reviewed tree unchanged, merge, fast-forward local main, and verify clean `HEAD == origin/main`.

- [ ] **Step 4: Wait for exact-main CI and guarded release**

Require signed installer, portable ZIP, fixed WebView2, updater signature verification, and exact four-asset publication.

- [ ] **Step 5: Audit public v0.11.0**

Verify stable tag at merge commit, exact four assets, recorded digests, `latest.json` version/URL/signature, and clean synchronized local main.
