# Known issues and deferred work

This list describes limitations that remain after BackLog v0.8.0. Resolved
pipeline recovery, model fallback, first-run/download, CI-resource, and release
automation issues have been removed rather than kept as misleading open work.

## 1. Delivery identity is not a separate ledger entity

The current design derives `manifest_id` from content SHA-256 plus normalized
relative path. That gives replay stability and preserves separate physical
copies delivered at different paths, but content identity, delivery identity,
and manifest state are still coupled.

The deferred redesign would represent physical deliveries separately from
content, make manifest persistence transactional, and define resubmission and
duplicate behavior as first-class ledger operations. It is intentionally
outside v0.8.0 because it requires a data migration and coordinated Flow 2
compatibility work.

Until then:

- in Power Automate mode, Flow 2 must use `manifest_id`, never content SHA-256,
  as its idempotency key;
- a resolved file delivered again at the same relative path is not a new job;
- the same bytes delivered at a different path are a distinct physical copy
  and receive a distinct manifest.

## 2. No trusted Authenticode certificate

BackLog's Tauri updater signature and Windows Authenticode are separate trust
systems. A stable release requires the updater private key and installed copies
reject an update that does not match it.

The installer and bundled executables do not yet carry a trusted Authenticode
publisher signature. Manual installs can therefore show SmartScreen warnings,
and managed fleets may block the installer until IT approves it. SmartScreen
reputation remains external to the updater-signing workflow.

## 3. The optional 1.7B model has a size and memory cost

The one-download installer includes only Qwen3 0.6B Q8_0, which is the
configured primary on every RAM tier — so a fresh install names its first
document offline, on the model it shipped with. The Qwen3 1.7B Q8_0 escalation
model is an optional in-app download of roughly 1.8 GB, used for a third naming
attempt on machines above 9 GiB of RAM. It stays out of the installer because
carrying it would put the installer and the portable ZIP over GitHub's 2 GiB
per-release-asset limit.

Without it, BackLog remains usable and honestly reports that the primary model
will handle escalation attempts. With it, disk use and resident memory
increase. This is a deliberate package-size tradeoff, not a missing installer
component.

**A larger pair was measured and rejected.** Promoting the 1.7B to primary and
adding a Qwen3-4B-Q4_K_M escalation was tried on 2026-08-07 and, at matched
evidence coverage, cost **2.0x the wall clock** (40.11 against 20.03 s/file)
for a **two-document** faithfulness difference at n=26 — inside this project's
own documented run-to-run variance. It was rejected on quality-per-second, not
on memory: the 1.7B primary fits the 16 GB class comfortably. See
`docs/SIZING.md` for the full 2x2 and the caveats.

## 4. Deep dates can still be missed

The deterministic checker refuses an unverified date, but the model/evidence
lanes do not always find a trustworthy date deep in a long document. Such a
document goes to Needs Review rather than receiving an invented name.

A future improvement could preserve exact document offsets when salience-lane
dates are folded into the evidence bundle. That change needs a measured corpus
run because widening evidence can also increase model latency and ambiguity.

## 5. `SUBJECT_TRUNCATED` is noisy

The checker safely trims subjects over ten words, but the model often writes
past that limit, so the flag appears frequently and is less useful for
triage. The shipped filename remains valid; this is an observability problem.
Changing the word limit or suppressing the flag for complete
`<form> - <party>` subjects requires a measured naming-quality run.

## 6. Naming attempts and selected tier are not recorded per document

Attempts and whether the optional tier answered determine much of processing
time, but neither is stored as a queryable per-document field. Performance
analysis therefore still depends on controlled experiments instead of a ledger
query.

## 7. `sidecar/requirements.lock` is not hash-pinned

The sidecar lock contains exact versions but no artifact hashes. The Windows
workflow builds from that committed lock and smoke-tests the resulting
executable, but it cannot prove that a package artifact at a pinned version was
never replaced upstream.

The remaining supply-chain improvement is to generate a Windows/Python 3.11
lock with `pip-compile --generate-hashes` and keep the release build in
hash-checking mode.

## 8. A dismissal cannot be undone inside BackLog

**Can't fix this** has a confirmation and leaves the document untouched in
Quarantine, but once confirmed, the app has no Dismissed view or Undismiss
action. Recovery is manual filing.

Adding one requires a decision about reopening ledger state and what the
selected delivery mode does with the existing dismissed row. In Local folder
mode a dismissal deliberately leaves the file in Quarantine; its receipt
records that review outcome but has no delivered output path. In Power Automate
mode the handoff policy also needs a decision. Re-delivering the file under
another path is not a safe substitute because it represents a new physical
delivery.

## 9. Retention and plaintext handoff policy remain deployment decisions

The SQLCipher ledger is encrypted, but its value-free event trail has no
automatic retention period. In Power Automate mode, manifests must be plaintext
while waiting for Flow 2; they contain the proposed filename and one-sentence
description, not document text, and Flow 2 deletes them after commit. Local
folder mode has no manifest handoff, but its JSON receipts need a deployment
retention decision too.

Each deployment still needs to choose an event-retention policy and document
the accepted plaintext handoff window.

## 10. Ettin is configured but inactive in the slim package

The shipped sidecar does not include `transformers`, so the optional Ettin span
lane returns unavailable even if a path is entered in Advanced Settings.
BackLog's deterministic and Qwen lanes continue to work. A future release
should either ship a measured torch-inclusive profile or remove the inactive
field.

## 11. Dev-only Vite advisory — resolved

Kept as a numbered entry rather than deleted, because earlier changelog entries
reference this item by number.

This recorded an esbuild advisory reachable through the Vite 5 development
server, with the major upgrade deferred. **The upgrade happened.** The tree is
on Vite 8.2.0 and `npm audit` reports zero vulnerabilities as of 2026-08-06.
Nothing here is outstanding. For a genuine dev-tooling problem see item 14.

## 12. Power Automate tenant E2E is not certified by Local folder tests

The Local folder path can be acceptance-tested entirely on one Windows machine:
renamed output, receipt, no-overwrite collision handling, and recovery do not
need a cloud tenant. That evidence does not prove the Power Automate / SharePoint
path. The target tenant still needs its actual Flow 1 and Flow 2 connectors,
permissions, manifest pickup, index/archive writes, throttling, and checkpoint
recovery tested before a rollout claim is made.

## 13. A document with no content can be named from the prompt's own example

Observed in the 2026-08-06 validation batch — one later found to have run with
a stale sidecar (item 15), so treat the specific run as unreliable. The defect
class is attested independently of it: the same parroting was present in the
v0.9.0 run, and it is the same class as the 0.4.2 subject-example parroting.
`v090b_edge_garbage.pdf`, a deliberately contentless fixture, was delivered as:

```
2026-08-06 Shareholder's register - John Smith.pdf
description: Shareholder's register transferring 40,000 shares to John Smith.
```

That is the naming system prompt's own description-rule example, copied
verbatim. It shipped **`ok`, not flagged**: the date fell back to the file's
mtime, which is legitimate for an undated document, and the subject is
well-formed, so no tripwire had grounds to fire.

**This is pre-existing and was not introduced by the 1.7B/4B tier change.** It
is the same class as the 0.4.2 subject-example parroting fixed in 0.8.3 — a
well-formed, unfaithful name no gate can reject — and it was present in the
v0.9.0 run on the same corpus. It is confined to documents with no extractable
content, where every possible name is wrong anyway.

The shallow defect is the example in the prompt. The deeper one is that a
document with nothing to name it from is *named at all* rather than flagged for
having no evidence: `SUBJECT_UNGROUNDED` cannot fire against text that does not
exist, so the grounding checks that catch fabrication elsewhere have nothing to
test against here.

**Not fixed in this change, deliberately.** Removing the example is one line,
but that example is what stops descriptions opening "The document…", which was
a measured problem. Changing it needs its own A/B over the full sample, not a
reflex edit made in the middle of a model-tier change where its effect could
not be separated from the tier's. The stale claim in `slm.rs` that this example
"was never observed to parrot" has been corrected — measurement falsified it.

## 14. The screenshot harness picks its port unreliably

`scripts/ui-harness/shoot.mjs` asks Vite for an ephemeral port (`port: 0`), and
under Vite 8 that is not honoured: the server falls back to the default 5173,
shifts to 5174 when something already holds it, and can bind IPv6-only. The
harness then navigates to `http://localhost:<port>/`, which resolves to IPv4
first on Windows, and every scenario fails with a 30 s navigation timeout that
looks like a UI fault rather than a port fault.

Dev tooling only — it cannot affect the shipped app. It matters because the
failure is loud, misleading, and costs a debugging session to identify. Two
concurrent harness runs, or any other dev server on 5173, will trigger it.

## 15. The semantic evidence lane degrades silently

`filter.rs` collapses a `rank_paragraphs` failure into `available: false` with
`.unwrap_or_else(...)` and falls back to a deterministic paragraph slice. A
missing, stale or incapable sidecar therefore disables semantic ranking with
**no error, no flag, and entirely healthy-looking totals**. Nothing in the run
output distinguishes a semantically-ranked batch from a degraded one.

This is not hypothetical: it invalidated a whole measurement campaign on
2026-08-06. Two end-to-end batches ran against a PyInstaller **onefile**
`convertd.exe` from 23 July, reporting version 0.2.0, sitting beside the
current **onedir** build. It predates the semantic pipeline and answers
`unknown op` to both `rank_paragraphs` and `extract_entities`. Both batches
completed, reported 30/33 named, and produced naming figures that described the
fallback path rather than the product. The tell was only visible in the
evidence cache — `routing: semantic_unavailable`, `semantic_available: false`,
bundles pinned near 6,000 chars from a 27,366-char source, 12 of 95 paragraphs
selected — and only when someone thought to look.

**Partly mitigated.** `e2e_real_batch` now probes `rank_paragraphs` at startup
and panics with the onedir/onefile explanation, so the measurement harness can
no longer produce plausible wrong numbers. **The production path still degrades
silently**, which is the deliberate design — an optional capability going
missing must not stop a document being named — but it means an operator whose
sidecar is stale gets quietly worse names with no indication. A visible
readiness signal for semantic availability would close that, and does not
exist.

## 16. `SEMANTIC_TOP_K` was hardcoded and budget-independent — fixed

Kept as a numbered entry rather than deleted, because it is the most
instructive result on this branch: the ceiling it describes was the reason
raising `evidence_token_budget` had never done what its name implies.

**What it was.** `filter.rs` fixed the semantic lane at a constant 12 ranked
paragraphs regardless of `evidence_token_budget`, and gave that lane 35% of the
character budget. On a 95-paragraph document about 13% of the text could ever
be ranked in, whatever the budget was set to. Raising the budget only let more
of those same 12 paragraphs survive truncation — a verbosity lever, not a
coverage lever.

**What replaced it.** `semantic_top_k(char_budget)` derives the count from the
budget — `semantic_lane_budget(char_budget) / 350`, clamped to 12..40 — and the
lane's share went 35% → 60%. Both had to move together: 12 paragraphs rendered
at ~350 characters each want ~4,200 characters against a 3,500-character lane,
so raising either one alone changed nothing. That is why the ceiling survived
as long as it did.

**Measured effect**, same corpus and same models:

| | before | after |
|---|---|---|
| bundle size | 6,526–7,652 chars (65–76% of budget) | **9,277–10,000 chars (92–100%)** |
| ranked paragraphs, shipped budget | 12 | **17** |
| ranked paragraphs, rung-3 budget | 12 | **27** |
| wall clock, shipped pair | 24.69 s/file | **20.03 s/file (19% faster)** |

Faster *and* wider: the lane spends less effort re-truncating an
oversized selection. This, not the model tier, was the change worth making —
see `docs/SIZING.md`.

**What it did not fix.** Naming quality did not measurably improve on this
corpus either (`date_source` was 29/30 in all four measured configurations).
That is a saturated corpus rather than evidence of no effect, and it is why a
stratified hard corpus is the blocking next step.
