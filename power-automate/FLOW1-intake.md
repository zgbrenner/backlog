# Flow 1: Intake Mover

Moves new files from the SharePoint intake library into the OneDrive-synced
Processing folder that BackLog watches. Deliberately dumb: no logic beyond the
move, so nothing here can corrupt a naming decision.

## Trigger

**When a file is created (properties only)** - SharePoint
- Site Address: your intake site
- Library Name: `Intake`
- Folder: `/` (or the drop subfolder)

Settings on the trigger:
- Concurrency Control: On, Degree of Parallelism: 1
  (serializes bursts; BackLog dedupes by content hash anyway, but 1 keeps
  OneDrive sync churn sane during a 3,000-file dump)

## Steps

1. **Get file content** - SharePoint
   - Site Address: intake site
   - File Identifier: `Identifier` from the trigger

2. **Create file** - OneDrive for Business
   - Folder Path: `/BackLog/Processing`
   - File Name: `triggerOutputs()?['body/{FilenameWithExtension}']`
   - File Content: output of step 1

3. **Condition: create succeeded**
   - Left: `outputs('Create_file')?['statusCode']`  |  is equal to  |  `200`
   - If yes -> **Delete file** - SharePoint (Identifier from trigger)
   - If no  -> **Append to _pa_errors list** (see below), do NOT delete

## Name collisions in Processing

Two different intake files can share a name. In step 2 set:
- If another file is already there: the OneDrive connector fails with 409.
  Configure a **Configure run after** on a duplicate **Create file 2** action:
  - File Name: `concat(formatDateTime(utcNow(),'yyyyMMddHHmmss'), '_', triggerOutputs()?['body/{FilenameWithExtension}'])`
  BackLog names from content, so a mangled temp name costs nothing.

## Error list

Create a SharePoint list `_pa_errors` with columns: Title (text), Stage (text),
Detail (multiline), When (date/time). Every failure branch appends a row:
- Title: filename
- Stage: `flow1-create` or `flow1-delete`
- Detail: `outputs(...)` of the failed action
- When: `utcNow()`

## Throughput note

For an initial multi-thousand-file backfill, skip Flow 1 entirely: bulk-download
the intake library and drop the files into the Processing folder locally.
Flow 1 exists for steady-state trickle, not for the opening dump.
