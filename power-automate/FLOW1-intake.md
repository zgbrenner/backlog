# Flow 1: Intake Mover

Moves each new SharePoint intake file into the OneDrive-synced Processing
folder that BackLog watches. The flow performs transport only. BackLog assigns
its durable physical-delivery identity locally before hashing or processing.

## Why the temporary filename has an envelope

Cloud-flow triggers and connector retries are not a safe physical-file identity.
The same source item can be delivered more than once, and two unrelated source
files can have the same human filename.

Flow 1 therefore creates the OneDrive file with this temporary shape:

```text
__incoming_<flow-id>-<sharepoint-item-id>__<original filename>
```

The token is stable for the same flow and SharePoint library item. When the
synced file becomes stable, BackLog:

1. derives a safe delivery directory from the token;
2. moves the file into `__bl_<delivery-id>/`;
3. restores the original human filename inside that directory; and
4. hashes and processes that durable path.

A retry of the same delivery becomes an idempotent no-op when its bytes match.
Different bytes under the same delivery token fail closed and remain visible in
Processing for investigation.

## Trigger

Use **When a file is created (properties only)** from the SharePoint connector.
Do not use either deprecated folder trigger.

- Site Address: the intake site
- Library Name: `Intake`
- Folder: `/` or the intended drop subfolder
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

### 2. Compose the stable delivery filename

Add a **Compose** action named `Delivery_file_name`.

Use this expression:

```text
concat(
  '__incoming_',
  workflow()?['name'],
  '-',
  string(triggerBody()?['ID']),
  '__',
  triggerBody()?['{FilenameWithExtension}']
)
```

In some tenants, the designer renders the last two properties under friendly
names rather than their internal names. If the expression editor does not
recognize either property, insert these trigger dynamic-content tokens into the
same `concat` expression:

- `ID`
- `File name with extension`

Do not use `utcNow()` or `guid()` for this token. Those values change between
retries and would turn one source item into multiple physical deliveries.

### 3. Create the OneDrive file

Add **Create file** from the OneDrive for Business connector.

- Folder Path: `/BackLog/Processing`
- File Name: output of `Delivery_file_name`
- File Content: output of `Get_file_content`

In the action settings:

- Retry Policy: Exponential
- Retry Count: `4`
- Retry Interval: `PT10S`

Because the filename is stable for the source item, a retry cannot silently
create a second differently named delivery. If the connector reports a conflict
because the same envelope already exists, leave the SharePoint source intact
and record the failure for review rather than inventing another identity.

### 4. Delete the SharePoint source only after success

Add **Delete file** from the SharePoint connector.

- File Identifier: `Identifier` from the trigger

Keep its default run-after setting so it runs only when **Create file**
succeeds. Do not test for a literal HTTP status such as `200`. Connector success
codes can vary by operation and connector version, while Power Automate already
tracks action success directly.

If deletion fails after the OneDrive file was created, log the error. Do not
remove or overwrite the OneDrive copy. Its stable delivery envelope makes any
later replay safe.

## Scope: `Log_transfer_failure`

Create a second scope after `Transfer_to_BackLog` and configure **Run after** for:

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

Create a SharePoint list named `_pa_errors` with these columns:

| Column | Type | Purpose |
|---|---|---|
| Title | Single line of text | Original source filename |
| Stage | Single line of text | `flow1-transfer`, `flow2-*`, or sweep stage |
| Detail | Multiple lines of text | Serialized action or scope result |
| When | Date and time | Failure timestamp |

## Backfill guidance

For the initial multi-thousand-file backfill, bypass Flow 1 and copy files into
`/BackLog/Processing` locally in controlled batches. BackLog gives every manual
arrival its own durable `__bl_<delivery-id>` directory before processing, so
same-name and same-content files remain separate physical instances.

Use Flow 1 for steady-state intake after the desktop app, Flow 2, list columns,
and error handling have passed the pilot runbook.
