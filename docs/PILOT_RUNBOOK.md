# BackLog staged pilot runbook

The pilot proves safety and measurable naming quality before BackLog touches a
large real collection. Every stage uses a frozen app commit, installer hash,
model lock, llama-server hash, configuration snapshot, and Power Automate
export.

## Roles

- **Pilot operator:** controls folders, starts and stops batches, and retains
  build and reconciliation evidence.
- **Document reviewer:** accepts or corrects every proposed name and description
  during the supervised stages.
- **Flow owner:** monitors commit checkpoints, `_pa_errors`, connector
  throttling, and recovery sweeps.
- **Security/legal reviewer:** approves runtime privacy, external binaries,
  dependency notices, and model redistribution.

## Freeze the candidate

Before Stage 0, record:

- PR commit SHA and unsigned installer SHA-256;
- `models.lock.json` SHA-256, and the model-bundle ZIP SHA-256 if one was
  produced for air-gapped staging;
- Qwen3 primary and escalation GGUF hashes (the slim, torch-free sidecar
  profile's whole model bundle -- no GLiClass or Granite snapshot; see
  `docs/DEPENDENCY_COMPATIBILITY.md`);
- llama-server release provenance, version output, and SHA-256;
- `convertd.exe` SHA-256 and `sidecar-build-lock.txt`;
- BackLog configuration, Flow exports, list schemas, and indexed columns; and
- the untouched source-document backup location.

Do not change any frozen input during a stage. A change creates a new candidate
and restarts that stage.

## Stage 0: synthetic local shadow run

1. Build and install the pilot candidate per `RELEASING.md`.
2. Install the models: Settings -> **Download models**, or copy the two locked
   `.gguf` files into `%APPDATA%\ai.sonomos.backlog\models` by hand. They are
   **not** in the installer (`bundle.resources` maps only `resources/*` and
   `binaries/*.dll`). Then confirm preflight reports both model files present.
3. Set Processing to a local test folder.
4. Set Outbox to a local, unsynced shadow folder so no manifest reaches Power
   Automate.
5. Set local quarantine and cache folders outside Processing and Outbox.
6. Run preflight with networking disabled (after the models are in place --
   the download is the one step that needs a network).
7. Copy the synthetic fixture set into Processing.
8. Exercise Pause before one file arrives, then Resume.
9. Kill `convertd.exe` during one request and verify bounded recovery.
10. Replay one delivered file and one emitted manifest.

Required results:

- zero source deletion, overwrite, or silent loss;
- every emitted JSON file validates against manifest schema v3;
- Unicode and ambiguous-date fixtures do not panic;
- Lingua returns a usable ISO language code for the Danish fixture;
- the zero-byte and unsupported fixtures are flagged;
- the scanned fixture follows 300-DPI, 400-DPI, and enhanced 600-DPI RapidOCR
  attempts when confidence remains low;
- the three duplicate fixtures produce one content hash, three stable instance
  and manifest IDs, and collision-safe names;
- Qwen output that violates a checker rule is rejected and retried; and
- replaying an existing delivery or manifest is idempotent.

## Bounded Local folder acceptance path

This path exercises the locally testable delivery mode. It is not evidence that
the Power Automate tenant flows work; keep Flow 1 and Flow 2 disabled for this
test.

1. In Settings choose **Local folder**. Choose four separate, non-nested test
   folders: a read-only source backup, Processing, Local Output, and
   Quarantine. Save and run **Check this computer**.
2. Put one ordinary supported document in Processing. Record its original path,
   expected renamed output path, and the receipt path
   `Local Output/.backlog/receipts/<manifest_id>.json`.
3. Put two byte-identical physical copies in different Processing paths. Record
   their distinct delivery IDs, output names, and receipts. Both copies must be
   accounted for even though their content hash matches.
4. Before delivery, create an unrelated file in Local Output with the proposed
   final name. BackLog must keep that unrelated file and use the deterministic
   collision suffix for its own output; it must not overwrite either file.
5. During another delivery, stop BackLog or force the documented fault/restart
   boundary. Restart it and reconcile the original source, renamed output, and
   receipt. There must be one durable outcome, not a missing source, duplicate
   output, or orphaned receipt.
6. Include one document that reaches Needs Review. Correct it and approve it:
   the renamed document must move directly from Quarantine to Local Output with
   its receipt. Include a second flagged document and choose **Can't fix
   this**: it must remain in Quarantine and must not produce Local Output or a
   renamed document; its receipt must record the dismissed delivery state.
7. Reconcile every test source against exactly one allowed outcome: still in
   Processing only while unfinished; a renamed Local Output file with its
   matching receipt; or Quarantine for flagged/dismissed work with its review
   receipt. Record every manifest/delivery ID, output path, receipt path, and quarantine path. Stop
   on any overwrite, source loss, unpaired completed output/receipt, or
   unaccounted file.

Pass criteria: the ordinary file, both physical duplicate copies, collision
case, restart/fault case, corrected flagged file, and dismissed flagged file
are all accounted for; no unrelated output changes; and Local Output contains
no Power Automate manifests or `_manifests` directory.

## Stage 1: 50-document supervised batch

Use non-sensitive or appropriately approved representative documents. Keep a
read-only backup outside every watched and synced folder.

1. Flow concurrency: `1`.
2. `manifest_emit_per_min`: `10`. The shipped default in `config.rs` is `0`
   (unlimited); it must be set explicitly in `backlog.config.json` or the
   Stage 2 no-unrecovered-429 gate cannot be met.
3. Review every file before relying on the index.
4. Reconcile Processing, Processed, Archive, DocumentIndex, NeedsReview,
   commit checkpoints, and `_pa_errors` at the end.
5. Stop immediately on any data-loss, unsupported-date, or identity-integrity
   failure.

Gate to proceed:

- 100% of source files accounted for;
- 100% of accepted dates supported by document evidence or recorded metadata;
- 100% of duplicate and replay cases correct;
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
- no 429 connector failure left unrecovered; and
- measured throughput is sufficient for an overnight run on the deployment
  computer.

## Stage 3: 500-document operational batch

Use the intended deployment computer and normal OneDrive synchronization.
Review a statistically useful sample plus every flagged or soft-flagged item.
Do not increase flow concurrency during the batch.

Gate for a wider pilot:

- zero unaccounted files;
- zero duplicate index rows;
- zero filename collision or overwrite;
- at least 95% filename acceptance on the reviewed sample;
- at least 92% description acceptance;
- all recovery tests pass; and
- security, legal, operational, and pilot owners sign off on the frozen evidence.

## Metrics to capture

For every stage record:

- total physical files, instances/deliveries, manifests where Power Automate
  mode is used, local receipts where Local folder mode is used, and unique
  content hashes;
- conversion success by detected MIME type and native or scanned route;
- OCR confidence and retry distribution;
- Qwen3 primary versus escalation usage;
- checker rejection and retry reasons;
- filename and description acceptance rates;
- human correction categories;
- flagged-reason accuracy;
- per-file and total processing time;
- Power Automate manifest-to-completed-flow latency, where that mode is used;
- Local Output and receipt commit/recovery latency, where Local folder mode is
  used;
- flow retries, conflicts, throttling responses, and failed checkpoints; and
- counts across every folder and SharePoint list before and after reconciliation.

## Stop conditions

Stop the batch and preserve all evidence when any of these occurs:

- a source cannot be located in the backup, Processing, Quarantine, Local
  Output plus receipt, Processed, or Archive as appropriate to its mode;
- one ManifestId maps to different bytes or a different physical instance;
- a destination file is overwritten;
- a date unsupported by evidence passes the checker;
- document content leaves the approved local and SharePoint boundary;
- an outbound connection occurs that is not one of the two documented ones
  (the one-time Hugging Face model download, and the startup updater check to
  `releases/latest/download/latest.json`);
- the same failure repeats after one controlled recovery attempt; or
- safety or accuracy materially regresses from the preceding stage.

## Rollback

1. Pause BackLog.
2. In Power Automate mode, disable Flow 1, Flow 2, and the recovery sweep. In
   Local folder mode there is no Flow consumer to disable.
3. Do not delete manifests or checkpoint rows.
4. Export commit checkpoints, DocumentIndex, NeedsReview, and `_pa_errors`.
5. Copy the local ledger, config, cache, Processing, Quarantine, and the
   selected output folder (Outbox or Local Output, including local receipts) to
   a dated incident directory. Include Processed/Archive where Power Automate
   is in scope.
6. Record running-process versions and all frozen hashes.
7. Reconcile every source against the untouched backup.
8. Fix and validate against synthetic fixtures before resuming at the previous
   completed stage, not the failed stage.
