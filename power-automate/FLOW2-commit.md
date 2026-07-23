# Flow 2: Commit manifest v2 to SharePoint

Flow 2 consumes each JSON file from `<Outbox>/_manifests`, commits one physical
file instance to SharePoint, records the result, and deletes the manifest only
after every required side effect is durable.

The flow is idempotent by `manifest_id`, not by `sha256`:

- `manifest_id` identifies one physical file instance and is the replay key.
- `sha256` identifies the file bytes and is used for duplicate reporting.
- Two files with identical bytes have the same `sha256`, different
  `manifest_id` values, and different reserved filenames.
- Replaying the same physical instance reuses its `manifest_id` and cannot
  create a second index row.

## 1. Prerequisites

### OneDrive for Business folders

- `/BackLog/Processing`
- `/BackLog/Processed`
- `/BackLog/Outbox/_manifests`

The Processing and Outbox folders are synced to the machine running BackLog.
The local quarantine folder is intentionally not synced.

### SharePoint document library: `Archive`

Create the folder `/Archive/_backlog-staging`. Add these library columns:

| Column | Type | Settings | Purpose |
| --- | --- | --- | --- |
| `BackLogManifestId` | Single line of text | Indexed, enforce unique values | Links the archived file to one manifest delivery |
| `Sha256` | Single line of text | Indexed | True content identity and duplicate analysis |
| `OriginalRelpath` | Single line of text | Optional | Source path inside Processing |
| `DuplicateOf` | Single line of text | Optional | Original content SHA-256 for duplicate content |
| `ProcessedAt` | Date and time | Optional | BackLog processing timestamp |

The staging folder gives retries a deterministic place to resume. Each manifest
uses `/Archive/_backlog-staging/<ManifestId>/` and keeps `new_filename` as the
file name. The SharePoint **Move file** action then moves that file into the
Archive root without changing its name. Custom metadata is preserved by the
move.

### SharePoint list: `DocumentIndex`

| Column | Type | Settings |
| --- | --- | --- |
| `Title` | Single line of text | New filename |
| `ManifestId` | Single line of text | Indexed, enforce unique values |
| `Sha256` | Single line of text | Indexed |
| `Description` | Multiple lines of text | Plain text |
| `DocDate` | Date only |  |
| `DateSource` | Choice | `document`, `metadata`, `human` |
| `DocType` | Single line of text |  |
| `Language` | Single line of text |  |
| `OriginalName` | Single line of text |  |
| `OriginalRelpath` | Single line of text |  |
| `DuplicateOf` | Single line of text | Optional |
| `SoftFlags` | Multiple lines of text | Plain text |
| `ModelVersions` | Multiple lines of text | Plain text JSON |
| `ProcessedAt` | Date and time |  |
| `ArchiveItemId` | Number | Optional but recommended |
| `ArchiveUrl` | Hyperlink | Optional but recommended |

### SharePoint list: `NeedsReview`

| Column | Type | Settings |
| --- | --- | --- |
| `Title` | Single line of text | Original filename |
| `ManifestId` | Single line of text | Indexed, enforce unique values |
| `Sha256` | Single line of text | Indexed |
| `FlagReason` | Multiple lines of text | Plain text |
| `OriginalName` | Single line of text |  |
| `OriginalRelpath` | Single line of text |  |
| `DuplicateOf` | Single line of text | Optional |
| `SoftFlags` | Multiple lines of text | Plain text |
| `ModelVersions` | Multiple lines of text | Plain text JSON |
| `ProcessedAt` | Date and time |  |
| `ReviewState` | Choice | `Pending`, `Committed`, `Dismissed` |
| `ResolvedAt` | Date and time | Optional |

Keep committed rows for audit. Create a default view filtered to
`ReviewState = Pending`.

### SharePoint list: `_pa_errors`

Create `ManifestId` and `Sha256` as indexed single-line text columns, plus
`Stage`, `Message`, `RunId`, `Retryable`, and `OccurredAt`. Every failure branch
writes one actionable row and leaves the manifest in place.

## 2. Manifest contract

BackLog ships two schemas for different jobs:

- `power-automate/manifest.schema.json` is the strict Draft 2020-12 source
  contract used by CI and fixture validation.
- `power-automate/manifest.parse-json.schema.json` is the conservative schema
  to paste into the Power Automate **Parse JSON** action.

Do not paste the strict schema into Parse JSON and do not generate a new schema
from one sample. The action uses a limited JSON Schema feature set, so the flow
must perform the status-specific and identifier checks described below.

Reference fixtures:

- `examples/manifest-ok.json`
- `examples/manifest-duplicate.json`
- `examples/manifest-flagged.json`

The Parse JSON schema establishes the payload shape. The explicit contract
condition enforces stable identifiers and status-specific requirements before
any file or SharePoint side effect is permitted.

## 3. Trigger and concurrency

Use **When a file is created (properties only)** from OneDrive for Business.

- Folder: `/BackLog/Outbox/_manifests`
- Include subfolders: No
- Trigger concurrency: On
- Degree of parallelism: `1` for the first pilot, then `4` after duplicate and
  retry tests pass

Add **Get file content using path** as the first action. Keeping content out of
the trigger makes the trigger payload small and makes the scheduled sweep use
the same processing actions.

BackLog may replace a still-pending flagged JSON path after human review. Do
not rely on the created trigger to observe that transition. The scheduled sweep
below is required and remains the source of truth for missed connector events
and corrected manifests already present at the same path.

## 4. Recommended flow structure

Put steps 1 through 13 in a `Try commit` scope. Add a `Record failure` scope that
runs after timeout, failure, or skip. Do not delete the manifest in the failure
scope.

### Step 1: Get, parse, and validate the manifest

1. **Get file content using path** for the trigger path.
2. **Parse JSON** using `manifest.parse-json.schema.json`.
3. Add Compose action `ManifestIdNonHex` with this expression:

```text
replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(replace(toLower(body('Parse_JSON')?['manifest_id']),'0',''),'1',''),'2',''),'3',''),'4',''),'5',''),'6',''),'7',''),'8',''),'9',''),'a',''),'b',''),'c',''),'d',''),'e',''),'f','')
```

4. Add Compose action `Sha256NonHex` with the same expression, replacing
   `manifest_id` with `sha256`.
5. Add a **Condition** using this expression:

```text
and(
  equals(body('Parse_JSON')?['schema'], 2),
  equals(length(body('Parse_JSON')?['manifest_id']), 64),
  equals(body('Parse_JSON')?['manifest_id'], toLower(body('Parse_JSON')?['manifest_id'])),
  empty(outputs('ManifestIdNonHex')),
  equals(length(body('Parse_JSON')?['sha256']), 64),
  equals(body('Parse_JSON')?['sha256'], toLower(body('Parse_JSON')?['sha256'])),
  empty(outputs('Sha256NonHex')),
  not(empty(trim(body('Parse_JSON')?['original_name']))),
  not(empty(trim(body('Parse_JSON')?['original_relpath']))),
  not(empty(trim(body('Parse_JSON')?['processed_at']))),
  or(
    and(
      equals(body('Parse_JSON')?['status'], 'ok'),
      not(empty(trim(coalesce(body('Parse_JSON')?['new_filename'], '')))),
      not(empty(trim(coalesce(body('Parse_JSON')?['description'], '')))),
      not(empty(trim(coalesce(body('Parse_JSON')?['date'], '')))),
      not(empty(trim(coalesce(body('Parse_JSON')?['date_source'], '')))),
      empty(coalesce(body('Parse_JSON')?['flag_reason'], ''))
    ),
    and(
      equals(body('Parse_JSON')?['status'], 'flagged'),
      not(empty(trim(coalesce(body('Parse_JSON')?['flag_reason'], '')))),
      empty(coalesce(body('Parse_JSON')?['new_filename'], ''))
    )
  )
)
```

On a false result, create `_pa_errors` stage `flow2-contract`, leave the
manifest in place, and terminate as failed. This condition is the runtime
equivalent of the stricter CI schema.

Useful expressions:

```text
body('Parse_JSON')?['manifest_id']
body('Parse_JSON')?['sha256']
body('Parse_JSON')?['status']
```

### Step 2: First idempotency gate

Use **Get items** on `DocumentIndex`:

```text
ManifestId eq '@{body('Parse_JSON')?['manifest_id']}'
```

Set **Top Count** to `1`. If one row exists:

1. Delete the manifest.
2. Terminate as succeeded.

Never query by `Sha256` for this gate. A duplicate-content file is a separate
physical instance and must receive its own index row.

### Step 3: Find the review record

Use **Get items** on `NeedsReview` with the same ManifestId filter and Top Count
`1`. Save the result for both branches.

### Step 4: Handle `status = flagged`

If `status` equals `flagged`:

1. If the NeedsReview row exists, update it with the newest reason, flags,
   versions, timestamp, and `ReviewState = Pending`.
2. Otherwise create it.
3. Delete the manifest.
4. Terminate as succeeded.

The original file is already in BackLog's local quarantine. Flow 2 must not
copy or rename it.

Use this expression for optional flags:

```text
join(coalesce(body('Parse_JSON')?['soft_flags'], json('[]')), ',')
```

### Step 5: Compose deterministic paths for `status = ok`

Create Compose actions or variables:

```text
SourceProcessingPath = concat('/BackLog/Processing/', body('Parse_JSON')?['original_relpath'])
SourceProcessedPath  = concat('/BackLog/Processed/', body('Parse_JSON')?['new_filename'])
ArchiveFinalPath     = concat('/Archive/', body('Parse_JSON')?['new_filename'])
ArchiveStageFolder   = concat('/Archive/_backlog-staging/', body('Parse_JSON')?['manifest_id'])
ArchiveStagePath     = concat(outputs('ArchiveStageFolder'), '/', body('Parse_JSON')?['new_filename'])
```

Do not build paths from `original_name`; nested source folders are represented
by `original_relpath`.

### Step 6: Resolve the live source path

1. Try **Get file metadata using path** on `SourceProcessingPath`.
2. If it is missing, try `SourceProcessedPath`.
3. If Processing exists, set `LiveSourcePath = SourceProcessingPath` and
   `SourceAlreadyProcessed = false`.
4. If only Processed exists, set `LiveSourcePath = SourceProcessedPath` and
   `SourceAlreadyProcessed = true`.
5. If neither exists, delay two minutes and repeat once. A second miss records
   `flow2-missing-source`, marks the error retryable, leaves the manifest, and
   terminates as failed.

This handles both OneDrive sync lag and a prior run that moved the source before
creating the index row.

### Step 7: Probe the final archive path

Try **Get file metadata using path** in SharePoint for `ArchiveFinalPath`.

If the file exists:

1. Use **Get file properties** with its item ID.
2. Compare `BackLogManifestId` to the parsed `manifest_id`.
3. If they match, set `ArchiveCommitted = true` and skip staging.
4. If they differ or the property is blank, record `flow2-name-conflict`, leave
   the manifest, and fail. Never overwrite or auto-rename an unrelated file.

If the final file is absent, continue to staging.

### Step 8: Create or resume the staged archive file

When `ArchiveCommitted` is false:

1. Ensure `ArchiveStageFolder` exists. Treat "already exists" as success.
2. Probe `ArchiveStagePath`.
3. If it is absent, use OneDrive **Get file content using path** on
   `LiveSourcePath`, then SharePoint **Create file**:
   - Folder Path: `ArchiveStageFolder`
   - File Name: `new_filename`
   - File Content: the OneDrive file content
4. Use **Get file properties** for the staged file.
5. Use **Update file properties** and set:
   - `BackLogManifestId = manifest_id`
   - `Sha256 = sha256`
   - `OriginalRelpath = original_relpath`
   - `DuplicateOf = duplicate_of` or blank
   - `ProcessedAt = processed_at`
6. Use SharePoint **Move file** to move the staged file to the Archive root.
   Set the name-conflict behavior to fail.

If a run dies after staging, the next run finds the deterministic stage path and
continues. If it dies after the move, the next run recognizes the final file by
its `BackLogManifestId` property.

### Step 9: Move and rename the OneDrive source

If `SourceAlreadyProcessed` is false, use OneDrive for Business
**Move or rename a file using path**:

- Source: `SourceProcessingPath`
- Destination: `SourceProcessedPath`

If the destination already exists, verify that the Processing source is gone
and continue only when the archive file carries the matching ManifestId.
Otherwise record `flow2-processed-conflict` and fail.

### Step 10: Recheck the idempotency gate

Run the ManifestId query against `DocumentIndex` again immediately before
creating the row. This closes the race if trigger concurrency is raised later.
If a row now exists, skip creation and continue to cleanup.

### Step 11: Create `DocumentIndex`

Map the manifest fields as follows:

| DocumentIndex | Manifest or action output |
| --- | --- |
| `Title` | `new_filename` |
| `ManifestId` | `manifest_id` |
| `Sha256` | `sha256` |
| `Description` | `description` |
| `DocDate` | `date` |
| `DateSource` | `date_source` |
| `DocType` | `doc_type` |
| `Language` | `language` |
| `OriginalName` | `original_name` |
| `OriginalRelpath` | `original_relpath` |
| `DuplicateOf` | `duplicate_of` or blank |
| `SoftFlags` | joined `soft_flags` |
| `ModelVersions` | `string(model_versions)` |
| `ProcessedAt` | `processed_at` |
| `ArchiveItemId` | final SharePoint file item ID |
| `ArchiveUrl` | final SharePoint file link |

If Create item reports a unique-value conflict, query by ManifestId once more.
Treat it as success only when the existing row has the same ManifestId and
Sha256. Otherwise record `flow2-index-conflict` and fail.

### Step 12: Resolve an existing review row

When the Step 3 query found a NeedsReview row, update it only after the archive
and DocumentIndex commit succeed:

- `ReviewState = Committed`
- `ResolvedAt = utcNow()`

This preserves the human-review audit trail for a corrected manifest that uses
the same stable ManifestId.

### Step 13: Delete the manifest last

Delete the JSON manifest only after the DocumentIndex row exists and all file
operations are complete. A crash before this action leaves the durable retry
record in place.

## 5. Failure scope

Configure `Record failure` to run after failure, timeout, or skip from
`Try commit`.

1. Create one `_pa_errors` item with ManifestId, Sha256, the failing stage,
   `workflow()?['run']?['name']`, a concise message, and retryability.
2. Do not delete the manifest.
3. Do not overwrite archive or Processed files.
4. Terminate as failed.

Use connector retry policies for 429 and transient 5xx responses. Do not retry
schema errors, source-identity conflicts, or filename conflicts automatically.

## 6. Scheduled sweep

Create a companion recurrence flow every 15 minutes:

1. List files in `/BackLog/Outbox/_manifests`.
2. Select files older than 10 minutes.
3. Run the same commit logic for each file, with concurrency `1` initially.

A child flow inside a Power Automate solution is the cleanest way to reuse the
commit scope. If that is not available, duplicate the scope exactly and keep
both schema files as the shared source of truth.

The sweep covers missed OneDrive trigger events, sync races, transient
throttling, and human-review manifest replacements.

## 7. Duplicate and replay acceptance test

Before enabling the pilot, place three byte-identical files at three different
Processing paths. The expected result is:

1. Three distinct ManifestId values.
2. One shared true Sha256 value.
3. Three DocumentIndex rows.
4. Three distinct filenames: base, `(2)`, and `(3)`.
5. `duplicate_of` on the later two rows equals the shared Sha256.
6. Reprocessing any one manifest produces no additional row and no additional
   archive file.

## 8. Throttling

Start with Flow 2 concurrency `1` and BackLog `manifest_emit_per_min = 30`.
Raise concurrency only after the duplicate, replay, interruption, and conflict
tests pass. The SharePoint connector is throttled per connection, so increasing
parallelism can reduce throughput when it creates more 429 retries.
