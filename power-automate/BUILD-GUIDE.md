# Build and test the BackLog Power Automate flows

BackLog uses **Power Automate cloud flows**. Build them in
<https://make.powerautomate.com/>; Power Automate for desktop is not required
for this integration.

Use this guide for build order and pilot gates. Keep the detailed action logic
in [`FLOW1-intake.md`](FLOW1-intake.md) and
[`FLOW2-commit.md`](FLOW2-commit.md) as the implementation source of truth.

## Recommended solution layout

Create a Power Automate Solution named `BackLog Automation`, then create these
flows directly inside it:

```text
BackLog - Flow 1 - Intake Mover        automated cloud flow
BackLog - Commit Manifest              instant child flow
BackLog - Flow 2 - Manifest Trigger    automated cloud flow
BackLog - Flow 2 - Recovery Sweep      scheduled cloud flow
```

The child flow holds the Flow 2 commit transaction once. The automated wrapper
passes a newly created manifest to it, while the scheduled wrapper retries
manifests that a connector event missed or a previous run left behind.

Solution-aware child flows require a Dataverse-enabled environment and maker
permission. If the tenant does not expose Solutions, build the two automated
flows under **My flows** and copy the Flow 2 commit scope into the recovery
sweep exactly. Do not maintain two intentionally different commit paths.

## Provision storage before opening the designer

Create these OneDrive for Business folders:

```text
/BackLog/Processing
/BackLog/Processed
/BackLog/Outbox/_manifests
```

Sync Processing and Outbox to the computer running BackLog. Quarantine is
local and must not be synced. Keep Processing, Outbox, and Quarantine distinct
and do not nest one inside another.

Create these SharePoint resources before adding connector actions so the
designer discovers the complete column set:

- document library `Intake`;
- document library `Archive`, including `/Archive/_backlog-staging` and the
  columns in `FLOW2-commit.md`;
- lists `DocumentIndex`, `NeedsReview`, and `_pa_errors`.

If a SharePoint column is added after its connector action, the designer may
require that action to be removed and added again before the new field appears.

### Canonical `_pa_errors` schema

Both flows write to the same list. Create the union below and make every custom
field optional so an error logger cannot fail merely because the other flow's
fields are absent:

| Column | Type | Used by |
| --- | --- | --- |
| `Title` | Single line of text; optional | Flow 1 |
| `ManifestId` | Single line of text; indexed | Flow 2 |
| `Sha256` | Single line of text; indexed | Flow 2 |
| `Stage` | Single line of text | Both |
| `Detail` | Multiple lines of plain text | Flow 1 |
| `Message` | Multiple lines of plain text | Flow 2 |
| `RunId` | Single line of text | Flow 2 |
| `Retryable` | Yes/No | Flow 2 |
| `When` | Date and time | Flow 1 |
| `OccurredAt` | Date and time | Flow 2 |

`ManifestId` is the replay/idempotency key. Do not substitute `Sha256` or an
invented `InstanceId`: equal bytes at different Processing paths are separate
physical deliveries and require separate index rows.

## Build Flow 1 first

Create an automated cloud flow using SharePoint **When a file is created
(properties only)** on the `Intake` library. Avoid triggers labelled
deprecated. In trigger settings, enable concurrency and explicitly set degree
of parallelism to `1`.

Build this action tree:

```text
SharePoint file-created trigger
|- Scope: Transfer_to_BackLog
|  |- SharePoint: Get file content
|  |- Compose: Delivery_folder_path
|  |- OneDrive for Business: Create file
|  `- SharePoint: Delete file
`- Scope: Log_transfer_failure (run after failure or timeout)
   |- SharePoint: Create item in _pa_errors
   `- Terminate: Failed
```

The stable delivery path is:

```text
concat(
  '/BackLog/Processing/',
  workflow()?['name'],
  '-',
  string(triggerBody()?['ID'])
)
```

Preserve the original filename. Never use `utcNow()` or `guid()` in the path;
those values change on retry and turn one delivery into multiple records.

Configure OneDrive **Create file** with exponential retry, count `4`, interval
`PT10S`. SharePoint **Delete file** keeps its default success-only run-after so
the source is retained when destination creation fails. Follow
`FLOW1-intake.md` for the exact fields and error expressions.

Before enabling the flow, upload one small distinctive file and verify:

1. exactly one OneDrive delivery subfolder and file are created;
2. the original filename and bytes are preserved;
3. SharePoint deletes the source only after OneDrive succeeds;
4. the synced nested file reaches BackLog's queue; and
5. `_pa_errors` receives no row.

The design expects OneDrive **Create file** to accept the deterministic missing
delivery folder. Microsoft's connector reference documents the Folder Path
parameter but does not guarantee missing-folder creation. Prove this in the
target tenant. If it returns `folder not found`, stop rollout and capture the
exact connector error; add a tenant-supported deterministic folder-provisioning
step instead of changing the identity token.

## Build Flow 2 as one reusable transaction

Create `BackLog - Commit Manifest` as an instant child flow with **Manually
trigger a flow** and two text inputs:

```text
ManifestPath
OneDriveFileId
```

Put Steps 1-13 from `FLOW2-commit.md` inside `Try_commit`. Add
`Record_failure` after it, configured to run after failure, timeout, or skip.
It records one `_pa_errors` item, never deletes the manifest, never overwrites
an Archive/Processed file, and terminates Failed.

The transaction order is deliberate:

1. get, parse, and validate the manifest;
2. check `DocumentIndex` by `ManifestId`;
3. find a matching `NeedsReview` row;
4. finish `flagged` or `dismissed` without archiving;
5. compose deterministic source, destination, and staging paths;
6. resolve the live Processing/Processed source;
7. probe the final Archive path and reject unrelated collisions;
8. create or resume the staged Archive file and attach identity metadata;
9. move the OneDrive source to Processed;
10. recheck the idempotency gate;
11. create `DocumentIndex`;
12. resolve an existing review row; and
13. delete the manifest last.

For **Parse JSON**, paste the complete
[`manifest.parse-json.schema.json`](manifest.parse-json.schema.json). Do not
generate a schema from one example and do not paste the stricter
`manifest.schema.json`. Then paste the full runtime contract condition from
`FLOW2-commit.md`; Parse JSON alone cannot enforce the status-dependent rules.

If the child uses SharePoint or OneDrive connections, configure its run-only
connection settings to **Use this connection**, not **Provided by run-only
user**.

### Manifest trigger wrapper

Create an automated flow using OneDrive for Business **When a file is created
(properties only)**:

```text
Folder: /BackLog/Outbox/_manifests
Include subfolders: No
Concurrency control: On
Degree of parallelism: 1
```

Add **Run a Child Flow**, select `BackLog - Commit Manifest`, and pass the
trigger's Path and Id.

### Recovery sweep wrapper

Create a scheduled flow that runs every 15 minutes:

1. list files in `/BackLog/Outbox/_manifests`;
2. select JSON files older than ten minutes; and
3. process them sequentially through the same commit child flow.

Keep the loop's concurrency at `1` for the pilot. If the designer rejects a
child-flow call in that loop/context, duplicate the exact commit scope into the
sweep rather than omitting recovery.

## Acceptance order

Keep BackLog `manifest_emit_per_min = 10`, trigger concurrency `1`, and loop
concurrency `1` until every gate below passes:

1. ordinary `ok` delivery creates one Archive file and one `DocumentIndex` row;
2. replaying the same manifest creates nothing additional;
3. `flagged` creates/updates one Pending review row and archives nothing;
4. `dismissed` creates/updates one Dismissed audit row and archives nothing;
5. three byte-identical files at different Processing paths produce three
   ManifestIds, one shared Sha256, three index rows, and collision-safe names;
6. interruptions after staging, Archive move, source move, and index creation
   each converge safely on retry;
7. unrelated Archive and Processed filename collisions fail without overwrite;
8. a manifest missed by the normal trigger is committed by the recovery sweep;
9. a 50-document supervised reconciliation accounts for every source across
   Processing, Processed, Archive, DocumentIndex, NeedsReview, and `_pa_errors`.

Only after the duplicate, replay, interruption, conflict, and sweep tests pass
may Flow 2 trigger concurrency increase to `4`.

## Current Microsoft references

- Cloud-flow designer: <https://learn.microsoft.com/en-us/power-automate/flows-designer>
- Solution-aware flows: <https://learn.microsoft.com/en-us/power-automate/create-flow-solution>
- Child flows: <https://learn.microsoft.com/en-us/power-automate/create-child-flows>
- Error handling and scopes: <https://learn.microsoft.com/en-us/power-automate/guidance/coding-guidelines/error-handling>
- Flow limits and concurrency: <https://learn.microsoft.com/en-us/power-automate/limits-and-config>
- SharePoint connector: <https://learn.microsoft.com/en-us/connectors/sharepointonline/>
- OneDrive for Business connector: <https://learn.microsoft.com/en-us/connectors/onedriveforbusinessconnector/>
