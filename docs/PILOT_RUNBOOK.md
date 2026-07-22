# BackLog staged pilot runbook

The pilot proves safety and measurable naming quality before BackLog touches a
large real collection. Each stage uses a frozen app commit, model lock, config,
and Power Automate export.

## Roles

- **Pilot operator:** controls folders, starts/stops batches, and retains build
  evidence.
- **Document reviewer:** accepts or corrects every proposed name and description.
- **Flow owner:** monitors `BackLogCommits`, `_pa_errors`, connector throttling,
  and recovery sweeps.
- **Security/legal reviewer:** approves runtime privacy and model redistribution.

## Stage 0: synthetic local shadow run

1. Build and install the pilot candidate.
2. Set Processing to a local test folder.
3. Set Outbox to a local, unsynced shadow folder so no manifest reaches Power
   Automate.
4. Select the verified model directory and GGUF files.
5. Run preflight.
6. Copy the contents described in `fixtures/README.md` into Processing.
7. Confirm the acceptance checks below before connecting SharePoint.

Required results:

- zero source deletion or silent loss;
- every emitted JSON file validates against schema v2;
- Unicode and ambiguous-date fixtures do not panic;
- the zero-byte fixture is flagged;
- the three duplicate fixtures produce collision-safe names; and
- replaying an existing file or manifest is idempotent.

## Stage 1: 50-document supervised batch

Use non-sensitive or appropriately approved representative documents. Keep a
backup outside all watched and synced folders.

1. Flow concurrency: 1.
2. `manifest_emit_per_min`: 10.
3. Review every file before relying on the index.
4. Reconcile Processing, Processed, Archive, DocumentIndex, NeedsReview,
   BackLogCommits, and `_pa_errors` at the end.
5. Stop immediately on any data-loss or identity-integrity failure.

Gate to proceed:

- 100% of source files accounted for;
- 100% of accepted dates are supported by document evidence or recorded
  metadata;
- 100% of duplicate and replay cases are correct;
- no duplicate ManifestId rows;
- at least 90% of filenames accepted without editing;
- at least 85% of descriptions accepted without editing; and
- every rejected file has an accurate, actionable flag reason.

## Stage 2: 200-document representative batch

Include the actual mix of native documents, scans, languages, file sizes, and
legacy formats expected in the project.

Gate to proceed:

- all Stage 1 safety gates remain at 100%;
- at least 93% filename acceptance;
- at least 90% description acceptance;
- no unresolved workflow checkpoint older than 30 minutes;
- no 429 connector failures after automatic recovery; and
- measured throughput is sufficient for an overnight run on the deployment
  machine.

## Stage 3: 500-document operational batch

Use the intended deployment computer and normal OneDrive synchronization.
Review a statistically useful sample plus every flagged or soft-flagged item.
Do not increase flow concurrency during the batch.

Gate for a wider pilot:

- zero unaccounted files;
- zero duplicate index rows;
- zero filename collisions or overwrites;
- at least 95% filename acceptance on the reviewed sample;
- at least 92% description acceptance;
- all recovery tests pass; and
- security, legal, and operational owners sign off on the recorded evidence.

## Metrics to capture

For every stage record:

- total files and total unique content hashes;
- conversion success by MIME type and native/scanned route;
- OCR confidence and retry distribution;
- primary versus escalation SLM usage;
- checker rejection reasons;
- filename and description acceptance rates;
- human correction categories;
- flagged-reason accuracy;
- per-file and total processing time;
- manifest-to-completed-flow latency;
- flow retries, 409s, 429s, and failed checkpoints; and
- counts across every folder and SharePoint list before and after reconciliation.

## Stop conditions

Stop the batch and preserve all evidence when any of these occurs:

- a source cannot be located in the backup, Processing, quarantine, Processed,
  or Archive;
- a manifest ID maps to different bytes or a different instance;
- a destination file is overwritten;
- a date unsupported by evidence passes the checker;
- document content leaves the approved local/SharePoint boundary;
- the same failure repeats after one controlled recovery attempt; or
- error rates materially exceed the preceding stage.

## Rollback

1. Pause BackLog.
2. Disable Flow 1, Flow 2, and the recovery sweep.
3. Do not delete manifests or checkpoint rows.
4. Export `BackLogCommits`, `DocumentIndex`, `NeedsReview`, and `_pa_errors`.
5. Copy the local ledger, config, cache, Processing, Processed, quarantine, and
   Outbox folders to a dated incident directory.
6. Reconcile every source against the untouched backup.
7. Fix and validate against synthetic fixtures before resuming at the previous
   stage, not the failed stage.
