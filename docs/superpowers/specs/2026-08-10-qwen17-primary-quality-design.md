# Qwen3 1.7B primary quality experiment and adaptive rollout

**Status:** approved design; experiment and implementation not yet run

**Date:** 2026-08-10
**Target release if the quality gate passes:** BackLog 0.11.0

## Decision to make

Determine whether a 14 GiB-class, CPU-only BackLog installation should use
Qwen3-1.7B-Q8_0 for every naming attempt when that optional model is installed,
instead of using Qwen3-0.6B-Q8_0 for the first two attempts and 1.7B only for
the third.

Quality is the primary objective. A total wall-clock slowdown of up to 2.0x is
acceptable only when the larger-primary policy produces a repeatable,
measurable improvement in grounded filenames and descriptions. The experiment
must isolate the primary-model change: it must not substitute a 4B escalation
model or change prompts, evidence selection, validation, concurrency, or
conversion behavior between arms.

## Alternatives considered

### 1. Adaptive 1.7B primary when installed — selected rollout

On machines above 9 GiB of installed RAM, use 1.7B for all naming attempts
when its verified GGUF is present. If it is absent, continue safely with the
bundled 0.6B primary and the existing optional-escalation behavior. This makes
quality available without making first launch depend on a network download or
breaking air-gapped installs.

### 2. Mandatory 1.7B on the 14 GiB class

Block processing until the optional 1.8 GB model is downloaded. This is
rejected because the installer and portable ZIP cannot carry both weights
within GitHub's 2 GiB per-asset limit, and a missing optional download must not
make an otherwise complete offline installation unusable.

### 3. Keep 1.7B as third-attempt escalation only

This remains the fallback if the controlled experiment does not cross the
quality gate. It is not selected in advance: the point of the experiment is to
determine whether spending the 14 GiB target's available compute on ordinary
documents improves results.

## Experimental arms

Both arms use the current main-branch code, Q8_0 weights, pinned llama.cpp
binary, current onedir `convertd`, live semantic evidence model, grammar,
system prompt, operator notes, evidence budgets, checker, retry count,
`slm_parallel=1`, `convert_workers=3`, and the same ordered inputs.

| Arm | Attempts 1 and 2 | Attempt 3 | Purpose |
|---|---|---|---|
| Control | Qwen3 0.6B | Qwen3 1.7B with wider evidence | Current production policy |
| Candidate | Qwen3 1.7B | The same Qwen3 1.7B with wider evidence | Isolates using 1.7B as primary |

The candidate uses one physical 1.7B server for all three attempts. It must not
launch a duplicate server for the third attempt. The wider third-rung evidence
and validator feedback remain unchanged.

Each arm runs three complete passes. Arm order alternates by pass
(control/candidate, candidate/control, control/candidate) to reduce warm-cache
and background-load bias. Outputs are written to distinct run directories and
scored only after every pass finishes.

## Stratified hard corpus

Create a deterministic 60-document synthetic corpus with a committed generator,
manifest, and scorer. Generated document contents contain no user data. Each
fixture records expected parties, document type, controlling date and source,
required description facts, forbidden combinations, and difficulty stratum.

The corpus contains 12 documents in each stratum:

1. **Ambiguous parties:** several legitimate entities, decoys, subsidiaries,
   and signatories; the subject must identify an allowed controlling party and
   never concatenate unrelated parties.
2. **Deep dates:** governing dates appear late in long documents while headers,
   footers, examples, and metadata contain plausible competing dates.
3. **Long evidence:** the decisive party and action are outside the simple head
   slice, exercising semantic selection and page-window behavior.
4. **Conflicting context:** prior agreements, quoted correspondence, templates,
   and superseded dates must not displace the current document's facts.
5. **Description grounding:** two to four required facts must be summarized
   without introducing forbidden entities, dates, amounts, or actions.

Use a balanced mix of TXT, text-layer PDF, DOCX, and scanned PDF. Unreadable,
encrypted, zero-byte, and corrupt fixtures remain in the ordinary reliability
suite, not this quality denominator, because neither model can name evidence it
cannot receive.

## Scoring

The scorer consumes only the corpus manifest and BackLog's emitted manifests.
It must not use original filenames as evidence. Every per-document metric is
binary and paired between arms:

- **subject faithfulness:** every entity-shaped subject phrase is allowed by
  the fixture ground truth;
- **controlling party:** the subject contains at least one expected controlling
  party and no forbidden party combination;
- **date correctness:** proposed date and `date_source` match an allowed pair;
- **document type/action:** subject identifies the expected type or action;
- **description recall:** all required facts appear, using normalized aliases;
- **description faithfulness:** no forbidden entity, date, amount, or action is
  introduced;
- **completion:** the document reaches the expected terminal state without an
  SLM/transport failure.

Critical errors are fabricated entities, fabricated dates or amounts,
forbidden party concatenations, and incorrect controlling dates. A document's
composite quality pass requires all six content metrics. Completion is reported
separately so a model cannot improve its score by flagging difficult inputs.

The scorer emits per-run JSON and a comparison report containing counts,
paired disagreements, absolute percentage-point deltas, critical-error counts,
median/p95 file time, total wall time, and observed process-memory peaks. The
report includes every differing output for manual blind inspection after the
deterministic score is fixed.

## Decision gate

Promote the candidate only when all conditions hold:

1. Candidate composite quality is at least **5 percentage points** above the
   control across the pooled paired observations.
2. Candidate quality is higher in each of the three pass pairs; the win cannot
   come from one anomalous run.
3. Candidate critical-error count is lower than control and no critical-error
   category regresses.
4. Date correctness, checker enforcement, completion rate, manifest integrity,
   and file-safety invariants do not regress.
5. Candidate total wall time is at most **2.0x** control. Median and p95 are
   reported but the agreed cap applies to total wall time.
6. The measured naming processes stay within the existing `<=17 GiB` modeled
   ceiling. Because the current host has 32 GB, this is not represented as a
   physical 14 GB acceptance run; deterministic tier tests and measured process
   peaks support the budget, while a physical target-device run remains a
   separate release note boundary.

If any condition fails, retain the current production policy. **Do not merge
the experiment branch or publish a new release merely because the experiment
ran.**

## Adaptive implementation if the candidate wins

Introduce one backend-owned runtime selection decision rather than changing
the persisted custom paths behind the operator's back:

- at or below 9 GiB, retain the 0.6B collapsed policy;
- above 9 GiB, if the verified configured 1.7B file exists, resolve both
  primary and escalation attempts to that file;
- above 9 GiB without the optional file, use the bundled 0.6B primary and keep
  readiness functional, with the existing download action explaining the
  available quality upgrade;
- preserve explicit operator-supplied model paths and fail closed on unsafe or
  unverifiable files;
- expose the effective policy in readiness/version diagnostics so support can
  distinguish configured paths from the models actually serving requests.

Settings copy should call 1.7B the optional **quality model**, not a backup
model. It must explain that it improves naming only when installed and may take
up to twice as long. Reset-to-recommended must select the adaptive policy, not
overwrite custom model paths.

No model is downloaded automatically. The installer and portable ZIP remain
self-contained with 0.6B and preserve their current four-asset signed release
contract. The existing in-app, resumable, SHA-256-verified download remains the
way to add 1.7B.

## Tests and verification

Before merge, if the candidate wins:

1. Commit the generator, ground-truth manifest, scorer, and machine-readable
   comparison report so the decision is reproducible without committing model
   outputs derived from user documents.
2. Add unit/contract tests for RAM boundary selection, optional-model absence,
   verified presence, custom paths, physical-slot collapse, readiness copy,
   and diagnostics.
3. Run the real three-pass A/B and record exact model/binary hashes and sidecar
   capability probe results.
4. Run Rust workspace tests, locked all-target Clippy with warnings denied,
   formatting, Python/model tests, frontend build/typecheck, UI harness,
   release-policy tests, Power Automate manifest validation, and npm/cargo
   audits.
5. Obtain an independent final review after the final diff.
6. Merge through a reviewed PR only when required checks pass. Confirm the
   merge tree equals the reviewed tree.
7. Publish signed stable `v0.11.0` from exact successful main CI and verify the
   tag, installer, portable ZIP, signature, updater manifest, four-asset set,
   hashes, and public release metadata.

## Rollback and reporting boundaries

The adaptive selector is isolated behind one backend decision and can be
reverted without changing ledger, manifests, checker rules, or stored document
state. A machine missing 1.7B always falls back to the already-supported 0.6B
path.

The release may claim a controlled synthetic-corpus quality improvement and a
verified signed Windows package. It may not claim physical 14 GB acceptance or
real-tenant Power Automate Flow 1/Flow 2 execution unless those are separately
performed.
