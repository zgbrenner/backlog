# What BackLog does with your documents

Plain language, and specific. If a sentence here is ever wrong, that is a bug —
please report it.

`docs/SECURITY.md` is the same subject written for an IT or security reviewer.

---

## The short version

BackLog reads your documents **on this computer**. The text never leaves it.

The only things that ever leave this computer are:

1. **The finished file and its new name**, handed to your own SharePoint by
   your own Power Automate flows — the same place the document was always
   going.
2. **A one-time download of two model files** from Hugging Face, when you press
   **Download models (~2.4 GB)** in Settings. No account, no login, and nothing
   about your documents is sent — it is a plain file download.
3. **A check for a new version of BackLog** when the app starts, to a GitHub
   releases URL. It sends nothing but the request itself.

That is the complete list.

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

Everything lives under `%APPDATA%\ai.sonomos.backlog` — your own Windows user
profile, which other users of the machine cannot read.

| What | Where | Protection |
|---|---|---|
| The record of every file processed: original name, proposed date, subject, description, state | `ledger.db` | **Encrypted.** The whole database file is encrypted with SQLCipher. |
| The key to that database | `ledger.key` | Protected by Windows DPAPI, so it can only be decrypted by **you, on this machine**. It is never written in plain form. |
| Converted document text | `cache\` | Deleted on filing (see above). |
| Model files | `models\` | The two Qwen model files, about 2.4 GB. |
| Log files | `logs\` | See below. |
| Your settings | `backlog.config.json` | Plain text — folder paths and tuning numbers only. |

Two things are deliberately **not** encrypted, because something else has to
read them:

- **Manifests** in your Outbox. These are the instructions Power Automate
  collects: the new filename, the one-sentence description, the date and the
  identifiers. They contain **no document text**. They exist for seconds to
  minutes and Flow 2 deletes each one after it commits.
- **Quarantined files.** A file that could not be named is moved, unchanged, to
  the Quarantine folder you chose. Protect that folder the way you protect the
  originals — pick a local folder, not a synced one.

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

## No file is ever deleted or overwritten

- A file that cannot be named is **moved** to Quarantine and listed in Needs
  Review. Nothing is thrown away.
- Two documents that would get the same name become `… (2)` and `… (3)`. An
  existing file is never overwritten.
- If BackLog is closed or the computer restarts mid-batch, files still sitting
  in the Processing folder are picked up again from where they were.

## What Power Automate sees

Your two flows run in **your** Microsoft tenant, under your own account.
BackLog does not talk to Microsoft; it writes a manifest to a folder that
OneDrive happens to sync, and your flows read it. The flows put the finished
file in your Archive library and a row in your DocumentIndex list.

If a file needed review, a row goes to your NeedsReview list with the reason
code — never with document text.

## Removing BackLog

Uninstalling BackLog removes the program. It deliberately leaves your data
behind, because deleting it silently would be worse: the model files are a
2.4 GB re-download, and the ledger is the record of what was filed.

To remove everything after uninstalling:

1. Delete `%APPDATA%\ai.sonomos.backlog` — this removes the encrypted ledger,
   its DPAPI key, the converted-text cache, the logs, your settings, and the
   2.4 GB of model files.
2. Delete the **Quarantine folder** you chose in Settings. Uninstall does not
   touch it, because it holds your own documents and BackLog will not delete a
   document under any circumstances.
3. Your Processing and Outbox folders are OneDrive folders BackLog only ever
   read from and wrote manifests to. Nothing there is BackLog's to remove.

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
