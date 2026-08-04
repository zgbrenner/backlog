# Flow 1: Intake Mover

> Build this as an **automated cloud flow** in the Power Automate web portal.
> It is not a Power Automate for desktop flow and does not require the desktop
> app.

Moves each new SharePoint intake file into the OneDrive-synced Processing
folder that BackLog watches. The flow performs transport only — it does not
rename anything and does not decide anything.

> **This document was rewritten.** A previous version prescribed a
> `__incoming_<flow-id>-<item-id>__<original filename>` filename envelope and
> described app behavior — deriving a delivery directory from the token, moving
> the file into `__bl_<id>/`, restoring the original filename — that does not
> exist anywhere in `src-tauri/src`. A pilot that built Flow 1 exactly as
> documented saw documents pile up in Processing with no queue rows, no
> manifests, no NeedsReview entries and no error. Build what is below instead.

## How BackLog identifies a delivery

You do not need to encode identity into the filename, because BackLog already
derives it from the path:

```
instance_id = SHA256( content_sha256 || 0x00 || normalized_relative_path )
```

(`src-tauri/src/identity.rs`.) The relative path is taken from the Processing
folder root, with separators unified and case folded. So:

- **Same bytes at the same relative path** → same id → a replay is an
  idempotent no-op.
- **Same bytes at different relative paths** → different ids → each is a
  separate physical delivery with its own manifest, its own index row and its
  own collision-safe filename (`base`, `(2)`, `(3)`).

That is exactly the property a delivery envelope was invented to provide. The
job of Flow 1 is therefore just to **give every delivery its own relative
path** — which a per-delivery subfolder does, while leaving the human filename
alone.

## The shape Flow 1 creates

```text
/BackLog/Processing/<flow-id>-<sharepoint-item-id>/<original filename>
```

For example:

```text
/BackLog/Processing/a1b2c3d4-e5f6-4789-abcd-ef0123456789-4217/Acme Agreement.pdf
```

The folder token is stable for the same flow and the same SharePoint library
item, so a connector retry lands on the same path and BackLog treats it as the
same delivery. The **filename is untouched**, which matters downstream: it is
what `original_name` carries into the SharePoint index and what a reviewer sees
in Needs Review.

The watcher is recursive, so a subfolder is watched exactly like the root.

### Two naming rules the watcher enforces

BackLog ignores any file or folder whose name begins with:

- `~$` — Office lock files
- `.` — dotfiles

Nothing else is ignored. A GUID-based folder token and an ordinary document
filename are both safe. Do not prefix either with a dot.

## Trigger

Use **When a file is created (properties only)** from the SharePoint connector.
Do not use either deprecated folder trigger.

- Site Address: the intake site
- Library Name: `Intake`
- Folder: `/` or the intended drop subfolder. Some tenants represent the
  library root as a blank picker value; use that if the designer rejects `/`.
- Concurrency Control: On
- Degree of Parallelism: `1`

The properties-only trigger does not include the file bytes. The next action
retrieves them with the trigger's file identifier.

Official connector references:

- <https://learn.microsoft.com/en-us/connectors/sharepoint/>
- <https://learn.microsoft.com/en-us/connectors/onedriveforbusiness/>
- <https://learn.microsoft.com/en-us/azure/logic-apps/expression-functions-reference#workflow>

## Build the flow

### Scope: `Transfer_to_BackLog`

Place the following actions inside one scope.

### 1. Get file content

Add **Get file content** from the SharePoint connector.

- Site Address: the intake site
- File Identifier: `Identifier` from the trigger

### 2. Compose the delivery folder path

Add a **Compose** action named `Delivery_folder_path`:

```text
concat(
  '/BackLog/Processing/',
  workflow()?['name'],
  '-',
  string(triggerBody()?['ID'])
)
```

In some tenants the designer renders `ID` under a friendly name. If the
expression editor does not recognise it, insert the trigger dynamic-content
token `ID` into the same `concat`.

**Do not use `utcNow()` or `guid()` in this token.** Those change between
retries and would turn one source item into several physical deliveries, each
with its own index row.

### 3. Create the OneDrive file

Add **Create file** from the OneDrive for Business connector.

- Folder Path: output of `Delivery_folder_path`
- File Name: `triggerBody()?['{FilenameWithExtension}']` — the original name,
  unmodified
- File Content: output of `Get_file_content`

Start the pilot without a separate folder-creation action. Microsoft's
connector reference documents `Folder Path` but does not guarantee that
**Create file** creates a missing delivery folder. Prove this behavior with one
delivery in the target tenant before rollout. If the action returns `folder
not found`, keep the stable delivery token and add a tenant-supported,
deterministic folder-provisioning step.

In the action settings:

- Retry Policy: Exponential
- Retry Count: `4`
- Retry Interval: `PT10S`

Because the folder is stable for the source item, a retry cannot silently
create a second differently-named delivery. If the connector reports a conflict
because the same path already exists, leave the SharePoint source intact and
record the failure for review rather than inventing another identity.

### 4. Delete the SharePoint source only after success

Add **Delete file** from the SharePoint connector.

- Site Address: the intake site
- File Identifier: `Identifier` from the trigger

Keep its default run-after setting so it runs only when **Create file**
succeeds. Do not test for a literal HTTP status such as `200`. Connector
success codes vary by operation and connector version, while Power Automate
already tracks action success directly.

If deletion fails after the OneDrive file was created, log the error. Do not
remove or overwrite the OneDrive copy — its stable delivery path makes a later
replay safe.

## Scope: `Log_transfer_failure`

Create a second scope after `Transfer_to_BackLog` and configure **Run after**
for:

- has failed
- has timed out

Do not select `is successful` or `is skipped`.

Inside the scope, add a SharePoint **Create item** action for the `_pa_errors`
list:

- Title: `File name with extension` from the trigger
- Stage: `flow1-transfer`
- Detail: `string(result('Transfer_to_BackLog'))`
- When: `utcNow()`

Then add **Terminate** with status `Failed` and a concise message such as:

```text
BackLog intake transfer failed. The SharePoint source was retained.
```

## `_pa_errors` list

Create one shared SharePoint list named `_pa_errors`. Both flows write to it,
so use the superset below and make every custom field optional:

| Column | Type | Used by |
|---|---|---|
| `Title` | Single line of text | Flow 1 original filename |
| `ManifestId` | Single line of text, indexed | Flow 2 delivery identity |
| `Sha256` | Single line of text, indexed | Flow 2 content identity |
| `Stage` | Single line of text | Both |
| `Detail` | Multiple lines of plain text | Flow 1 scope result |
| `Message` | Multiple lines of plain text | Flow 2 error message |
| `RunId` | Single line of text | Flow 2 run identity |
| `Retryable` | Yes/No | Flow 2 retry classification |
| `When` | Date and time | Flow 1 failure timestamp |
| `OccurredAt` | Date and time | Flow 2 failure timestamp |

See `BUILD-GUIDE.md` for the recommended Solution layout and acceptance order.

## How this reaches Flow 2

The subfolder becomes part of `original_relpath` in the manifest —
`a1b2…-4217/Acme Agreement.pdf`. `FLOW2-commit.md` step 5 already composes the
source path as `concat('/BackLog/Processing/', original_relpath)`, so nested
deliveries work with no change on that side. Do not build paths from
`original_name`; it is the leaf only.

## Backfill guidance

For the initial multi-thousand-file backfill, bypass Flow 1 and copy files into
`/BackLog/Processing` locally, in controlled batches.

Manual copies do **not** get a per-delivery subfolder — they get whatever
relative path you copy them to, which is the same identity mechanism. Two
same-named files in the same folder cannot both exist anyway; two same-named
files in different subfolders are two distinct deliveries and both are indexed.
Set `"manifest_emit_per_min": 10` in `backlog.config.json` before starting a
backfill (the shipped default of `0` means unlimited and is the documented
route to connector `429`s).

Use Flow 1 for steady-state intake after the desktop app, Flow 2, list columns,
and error handling have passed the pilot runbook.
