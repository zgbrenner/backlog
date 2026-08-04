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

The one-download installer includes only Qwen3 0.6B Q8_0. The Qwen3 1.7B Q8_0
escalation model is an optional in-app download of roughly 1.8 GB for difficult
documents.

Without it, BackLog remains usable and honestly reports that the primary model
will handle escalation attempts. With it, difficult-document quality can
improve, but disk use and resident memory increase. This is a deliberate
package-size tradeoff, not a missing installer component.

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

## 11. Dev-only Vite advisory

The current Vite 5 tree includes an esbuild advisory reachable through the
development server. The installed Tauri app serves a static production bundle
and does not expose that server. Moving to a newer Vite major remains deferred
until its build and harness behavior is revalidated.

## 12. Power Automate tenant E2E is not certified by Local folder tests

The Local folder path can be acceptance-tested entirely on one Windows machine:
renamed output, receipt, no-overwrite collision handling, and recovery do not
need a cloud tenant. That evidence does not prove the Power Automate / SharePoint
path. The target tenant still needs its actual Flow 1 and Flow 2 connectors,
permissions, manifest pickup, index/archive writes, throttling, and checkpoint
recovery tested before a rollout claim is made.
