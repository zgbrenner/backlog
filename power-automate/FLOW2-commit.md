# Flow 2: Commit (manifest-driven rename + archive + index)

Triggers on each JSON manifest BackLog writes to `<Outbox>/_manifests/`,
renames the original, copies it to the SharePoint archive, and inserts one row
into the index list. Idempotent: keyed on the manifest's `sha256`, so a re-run
or a duplicate trigger never double-inserts.

## Prerequisites

- SharePoint document library `Archive`
- SharePoint list `DocumentIndex` with columns:
  - Title (text)             <- new filename
  - Sha256 (text, indexed)   <- idempotency key
  - Description (multiline)
  - DocDate (date)
  - DateSource (choice: document / metadata / human)
  - DocType (text)
  - Language (text)
  - OriginalName (text)
  - DuplicateOf (text)
  - SoftFlags (text)
- SharePoint list `NeedsReview` with columns:
  Title (text), Sha256 (text), FlagReason (text), OriginalName (text)
- The OneDrive folders `/BackLog/Processing`, `/BackLog/Outbox/_manifests`
  synced on the machine running BackLog.

## Trigger

**When a file is created** - OneDrive for Business
- Folder: `/BackLog/Outbox/_manifests`
- Include file content: Yes
- Concurrency: On, Parallelism 1 for the first backfill; raise to 4 later.

## Steps

1. **Parse JSON** - Content: trigger file content. Schema: generate from a
   sample manifest (see `manifest.rs`; note `new_filename`, `flag_reason`,
   `duplicate_of` are nullable).

2. **Get items** - SharePoint, list `DocumentIndex`
   - Filter Query: `Sha256 eq '@{body('Parse_JSON')?['sha256']}'`
   - Top Count: 1

3. **Condition: already indexed**
   - `length(outputs('Get_items')?['body/value'])` is greater than 0
   - If yes -> **Delete file** (the manifest) -> Terminate (Succeeded).
     This is the idempotency gate.

4. **Condition: status**
   - `body('Parse_JSON')?['status']` is equal to `flagged`
   - If yes:
     a. **Create item** in `NeedsReview` (Title = original_name, Sha256,
        FlagReason = flag_reason, OriginalName = original_name)
     b. **Delete file** (the manifest). Done. (The original file already sits
        in BackLog's local quarantine; nothing to move here.)
   - If no: continue.

5. **Condition: file exists in Processing** (sync-race guard)
   - **Get file metadata using path** - OneDrive:
     `concat('/BackLog/Processing/', body('Parse_JSON')?['original_relpath'])`
   - Configure run after to catch failure. If it fails:
     - **Delay** 2 minutes (OneDrive still syncing the file up), then retry
       once. Second failure -> append `_pa_errors` row, stage `flow2-missing`,
       leave the manifest in place (the flow retriggers on next run of a
       scheduled sweep, below), Terminate (Failed).

6. **Copy file using path** - OneDrive -> SharePoint `Archive`
   - Destination file path: `concat('/Archive/', body('Parse_JSON')?['new_filename'])`
   - If a 409 conflict occurs (should be impossible; BackLog dedupes names
     against its ledger): append `_pa_errors`, stage `flow2-conflict`, stop.

7. **Rename**: OneDrive has no rename action; use **Move or rename a file**
   (OneDrive) on the Processing file with the new name, moving it into
   `/BackLog/Processed/`. This doubles as the "done" marker on the local side.

8. **Create item** - SharePoint `DocumentIndex`
   - Title: `new_filename`
   - Sha256: `sha256`
   - Description, DocDate (`date`), DateSource, DocType, Language,
     OriginalName (`original_name`), DuplicateOf (`duplicate_of`),
     SoftFlags: `join(body('Parse_JSON')?['soft_flags'], ',')`

9. **Delete file** (the manifest). Deleting last means any crash before this
   point leaves the manifest in place and the idempotency gate (step 3)
   makes the retry safe.

## Scheduled sweep (companion flow, every 15 min)

Trigger: Recurrence. **List files in folder** `_manifests`; for each file older
than 10 minutes, resubmit it through the same steps (a child flow, or just
re-copy it into the folder to refire the trigger). This catches manifests whose
trigger run died mid-flow and files that lost the sync race in step 5.

## Throttling

SharePoint connector throttling on huge backfills is real. If runs start
failing with 429s: set BackLog's `manifest_emit_per_min` to ~30, keep Flow 2
parallelism at 1, and let it grind overnight. The pipeline is asynchronous
end-to-end precisely so nobody has to babysit this.
