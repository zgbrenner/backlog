# What BackLog does with your documents

Plain language, and specific. If a sentence here is ever wrong, that is a bug —
please report it.

`docs/SECURITY.md` is the same subject written for an IT or security reviewer.

---

## The short version

BackLog reads your documents **on this computer**. It never sends converted
document text or evidence to a cloud service for reading, summarising, or
naming. In Power Automate mode, the finished document itself can still be
handed to your own SharePoint by your own flows, as described below.

The only outbound data and network activity are:

1. **In Power Automate / SharePoint mode only:** the finished file and a
   limited index record, handed to your own SharePoint by your own Power
   Automate flows — the same place the document was always going. The record
   includes the new filename, description, date, date source, document type,
   language, original name/path, content and delivery identifiers, model
   versions and soft flags. It never includes document text or evidence.
2. **A model-bundle download** from Hugging Face only when you press
   **Download models (~2.4 GB)** in Settings. A fresh release already includes
   the primary model; this action fills in missing or optional model assets. No
   account, no login, and nothing about your documents is sent — it is a plain
   file download.
3. **A check for a new version of BackLog** when the app starts, to a GitHub
   releases URL. It sends nothing but the request itself.

That is the complete list. **Local folder mode sends neither documents nor
receipts to Power Automate or SharePoint:** it writes them to the Local Output
folder you chose. The update request does not upload document data.

## Reading your documents

To suggest a name, BackLog has to read the document. That happens in two
programs that run **on this computer** and are not connected to the internet at
all:

- `convertd` turns Word, PowerPoint, PDFs and scans into plain text. Scans go
  through optical character recognition here on the machine.
- `llama-server` runs the small language model that proposes the date and
  subject. It listens only on `127.0.0.1` — the computer's own internal
  address, which nothing outside the machine can reach.

No part of a document is sent to a cloud service to be read, summarised or
named. There is no API key in this product because there is nothing to call.

## The converted text, and when it is deleted

The plain-text version is written to a working folder so the review pane can
show you what BackLog read.

- **When a file is filed successfully, its text is deleted immediately.**
- A file that lands in **Needs Review** keeps its text until you resolve it —
  otherwise "Document text" would have nothing to show you.
- Anything left orphaned is swept after 7 days.

If someone has deliberately turned on `retain_cache` in `backlog.config.json`
(off by default, and only useful for training a model on your own corpus), the
text is kept instead. You would have to be told; nothing turns it on by itself.

## What is stored on this computer, and where

BackLog's app data lives under `%APPDATA%\ai.sonomos.backlog` — your own
Windows user profile, which other users of the machine cannot read. The
Processing, Quarantine, Outbox, and Local Output folders are separate locations
chosen by the operator in Settings; they are not moved into AppData.

| What | Where | Protection |
|---|---|---|
| The record of every file processed: original name, proposed date, subject, description, state | `ledger.db` | **Encrypted.** The whole database file is encrypted with SQLCipher. |
| The key to that database | `ledger.key` | Protected by Windows DPAPI, so it can only be decrypted by **you, on this machine**. It is never written in plain form. |
| Converted document text | `cache\` | Deleted on filing (see above). |
| Model files | `models\` | The bundled primary Qwen model plus the optional escalation model, about 2.4 GB when both are installed. |
| Log files | `logs\` | See below. |
| Your settings | `backlog.config.json` | Plain text — folder paths and tuning numbers only. |
| Final Local Output documents | The **Local Output folder** chosen in Settings | **Not encrypted by BackLog.** In Local folder mode, completed renamed documents are written directly here. Protect and retain them as you would the originals. |
| Local Output receipts | `<Local Output>\.backlog\receipts\<manifest_id>.json` | **Plaintext metadata.** Each durable receipt records delivery and manifest metadata, including filenames, descriptions, identifiers, and source paths; it does not contain converted document text. |
| Local Output recovery state | `<Local Output>\.backlog\intents\` and `<Local Output>\.backlog\staging\` | Private transaction artifacts. Intent JSON is plaintext metadata; a staging `.part` file can be a temporary full copy of the document. They normally disappear after a completed delivery, but can remain after an interruption so BackLog can recover safely. |

Two things are deliberately **not** encrypted, because something else has to
read them:

- **Manifests** in your Outbox. These are the instructions Power Automate
  collects: the new filename, the one-sentence description, the date and the
  identifiers. They contain **no document text**. They exist for seconds to
  minutes and Flow 2 deletes each one after it commits.
- **Quarantined files.** A file that could not be named is moved, unchanged, to
  the Quarantine folder you chose. Protect that folder the way you protect the
  originals — pick a local folder, not a synced one.
- **The Local Output tree, in Local folder mode.** Finished renamed documents,
  their `.backlog/receipts` JSON, and any unfinished `.backlog` transaction
  artifacts stay in the operator-selected folder. BackLog does not encrypt
  them. Local mode writes no Outbox manifest, SharePoint index, or cloud
  archive.

## The logs

Logs record what BackLog did, not what your documents say.

- Folder paths are reduced to the drive and how deep they go —
  `C:\Users\jane\OneDrive\2024 Terminations` is logged as `C: (+5 levels)`
  (the drive counts as one of them).
- Anything the model produced is replaced with `[model output withheld]`.
- The permanent event trail inside the ledger stores stable codes
  (`CONVERT_FAIL`, `TIMEOUT`) rather than error text, because a raw error
  message embeds the document's full path.

Logs are safe to attach to a support email. Read one first if you would rather
be sure.

## No completed output is overwritten, and Local source removal is deliberate

- A file that cannot be named is **moved** to Quarantine and listed in Needs
  Review. Nothing is thrown away at that point. An unresolved or dismissed file
  stays there; an approved Local correction may later consume its pinned
  Quarantine copy only after the corrected output and receipt are durable.
- Two documents that would get the same name become `… (2)` and `… (3)`. An
  existing file is never overwritten.
- In Local folder mode, BackLog removes a source only after the finished
  renamed document and its receipt are durable. An interrupted transaction can
  leave private staging or intent artifacts for safe recovery.
- If BackLog is closed or the computer restarts mid-batch, files still sitting
  in the Processing folder are picked up again from where they were.

## What Power Automate sees — and what Local Output keeps local

Your two flows run in **your** Microsoft tenant, under your own account.
BackLog does not talk to Microsoft; it writes a manifest to a folder that
OneDrive happens to sync, and your flows read it. The flows put the finished
file in your Archive library and a row in your DocumentIndex list.

If a file needed review, a row goes to your NeedsReview list with the reason
code — never with document text.

In **Local folder** mode, BackLog does not send a manifest or a document to
Power Automate or SharePoint. It writes the finished renamed document directly
to the Local Output folder selected in Settings and retains a plaintext receipt
under `.backlog/receipts`. A flagged or dismissed Local file remains in the
operator-selected Quarantine folder instead of creating a cloud handoff.

## Removing BackLog

Uninstalling BackLog removes the program. It deliberately leaves your data
behind, because deleting it silently would be worse: the model files are a
2.4 GB re-download, and the ledger is the record of what was filed.

To remove everything after uninstalling:

1. Delete `%APPDATA%\ai.sonomos.backlog` — this removes the encrypted ledger,
   its DPAPI key, the converted-text cache, the logs, your settings, and the
   2.4 GB of model files.
2. Delete the **Quarantine folder** you chose in Settings. The uninstaller does
   not touch it: unresolved and dismissed documents remain there for manual
   handling. During normal Local folder operation — not uninstall — an approved
   correction may consume its pinned Quarantine copy only after the corrected
   output and receipt are durable.
3. If you used **Local folder** mode, review and remove or retain the entire
   **Local Output folder** according to your organisation's policy. It survives
   uninstall and can contain final renamed documents, plaintext
   `.backlog/receipts`, and unfinished private transaction artifacts under
   `.backlog`. Protect it until it is deliberately removed.
4. The uninstaller removes nothing from your Processing folder or from the
   Outbox used only in Power Automate mode; they are operator-owned folders.
   During normal **Local folder** delivery, before any uninstall, BackLog does
   remove the selected Processing source only after its finished renamed output
   and receipt are durable. Retain or remove the remaining contents under your
   own document and retention policy.

Deleting the DPAPI key makes the ledger permanently unreadable. That is the
intended outcome of step 1, and it cannot be undone.

## Two things worth knowing

- **The date on a file is never invented.** BackLog refuses to put a date on a
  document unless it can point at that exact date in the document's own text or
  in the file's properties. When neither exists, it uses the file's modified
  date and records that it did (`date_source: metadata`), so the index tells you
  which dates came from the page and which did not.
- **The update check runs at startup even though everything else is offline.**
  It is one HTTPS request to GitHub asking whether a newer BackLog exists. If
  your organisation would rather it did not, that is a change to
  `src/main.ts`'s `checkForUpdates` — ask IT.
