# BackLog: what the messages mean

Every code BackLog can show you, and what to do about it. Find the exact text
you are looking at in the left column.

You do not need a terminal for anything on this page.

**Where these appear**

- **Needs Review** shows a *flag reason* — the reason BackLog stopped short of
  naming a file. Sections 1 and 2 below.
- A named file can carry a *note* (soft flag). It was still filed; the note
  just says something worth knowing. Section 3.
- **Settings → Readiness** shows a *readiness problem* — something about this
  computer, not about a document. Section 4.

BackLog already shows a plain sentence for each of these in the app itself
(`src/main.ts`'s `REASON_COPY`, `SOFT_FLAG_COPY`). This page is the longer
version, plus the codes that only ever show up in a log or a support call.

The buttons named below are the ones on a Needs Review card: **Approve and
file**, **Try again**, **Can't fix this**, **Show me the file**, **Document
text**, **What happened**.

---

## 1. Reasons a file stopped before it was named

The reason string is `CODE:detail`. Match on the part before the colon.

| What BackLog says | What it means, and what to do |
|---|---|
| `CORRUPT:zero-byte file` | The file has no contents at all — usually a failed copy or a sync that never finished. **Find the original and drop a fresh copy into the Processing folder.** |
| `CORRUPT:read error …` | The file exists but the computer could not read it: a broken OneDrive sync, a disconnected drive, or a file another program has locked. **Wait for OneDrive to finish syncing, then use Try again.** If it repeats, copy the file from its source again. |
| `UNSUPPORTED_TYPE:<type>` | BackLog knows what kind of file this is and cannot read it (a ZIP, a video, a database, an installer). **Save it as PDF, Word or an image, and drop that in instead.** |
| `UNSUPPORTED` | Same as above, for a file whose type BackLog could not even identify. **Save it as PDF and drop that in instead.** |
| `ENCRYPTED:password protected` | The document is locked with a password, so nothing can read the text — including BackLog. **Open it, remove the password, save it, and drop it back into Processing.** |
| `CONVERT_FAIL:empty extraction` | BackLog opened the file successfully but found essentially no text (under 30 characters). Usually a blank page, a cover sheet, or a photo of something that is not a document. **If it is a scan, re-scan it right way up at 300 dpi. Otherwise fill in the date and subject yourself in Needs Review.** |
| `CONVERT_FAIL:all conversion attempts exhausted` | BackLog tried several times to read the file and failed each time. **Use Try again once. If it fails again, open the file yourself and fill in the date and subject in Needs Review.** |
| `UNREADABLE:all conversion attempts exhausted` | This was a scan. BackLog read it as an image at 300 dpi, then 400 dpi, then with enhanced 600-dpi OCR, and still got nothing usable. **Re-scan it: flat, right way up, 300 dpi or better, in colour or greyscale rather than pure black-and-white.** Or fill in the date and subject yourself. |
| `SLM_FAIL:no valid output after escalation` | BackLog read the document but could not propose a date and subject it was able to *prove* against the text — so it refused to guess. This is the safety rule working, not a bug. **Press Document text on the card, read what BackLog found, and fill in the date and subject yourself.** |
| `TIMEOUT:exceeded <n>s at stage <stage>` | One file used up its whole time budget (90 seconds by default). Very large PDFs and slow scans do this. **Use Try again.** If it happens on many files, ask IT to raise `per_file_wall_clock_secs` in `backlog.config.json`. |
| `CRASH_LOOP:<n> attempts without leaving stage <stage>` | BackLog restarted five times and this file stopped at the same step every time, so it was set aside to protect the rest of the batch. **Restart BackLog, then Try again.** If it recurs, this file needs to be named by hand and removed from Processing. |
| `RUNTIME_FAIL:filter …` / `RUNTIME_FAIL:name reservation …` / `RUNTIME_FAIL:manifest …` / `RUNTIME_FAIL:routing task panicked …` | The failure was in BackLog or the computer, not in your document — a folder disappeared, a disk filled up, or a part of BackLog stopped answering. **Open Settings and press Check this computer, fix anything it reports, then Try again.** |
| `TRACE_WRITE_FAILED:evidence trace could not be saved` | BackLog created the reduced evidence input but could not save the exact selection trace needed to audit it. The file was sent to Needs Review rather than processed invisibly. **Check free disk space and folder permissions, open Settings and press Check this computer, then Try again.** |
| `DISMISSED:<your note>` | Not a failure. Someone pressed **Can't fix this** on this file and confirmed the **Set aside for good?** prompt, so BackLog recorded a *decision* rather than work completed: SharePoint indexes the row as dismissed and the throughput figures do not count it as filed. **This is final inside BackLog.** The card leaves Needs Review and there is no button anywhere in the app that brings it back — **Try again** only helps *before* you confirm, while the card is still on screen. The document itself is untouched in your Quarantine folder, so if you change your mind afterwards you file it by hand, or hand it to whoever delivers documents and ask for it to be re-delivered **under a different name**. (Dropping the same file back into Processing does nothing: BackLog recognises it and leaves it alone. Under a different name it is treated as a second copy and is indexed as one.) |

Whatever the reason, the file itself is safe: BackLog moves it to your
Quarantine folder and lists it under Needs Review — and it stays in Quarantine
even after you set it aside with `DISMISSED`. It is never deleted and never
overwritten. See `docs/PRIVACY.md`.

## 2. Reasons the deterministic checker rejected a proposed name

These come from the checker (`src-tauri/core/src/checker.rs`) and are the rules
that make a name trustworthy. You will normally see them only as the *reason a
retry happened*, in the app log or a diagnostics bundle — the pipeline retries
up to three times and only the final `SLM_FAIL` reaches Needs Review. They also
appear directly under the correction form if **your own** correction breaks one
of the same rules.

| What BackLog says | What it means, and what to do |
|---|---|
| `BAD_DATE` | The date is not a date that exists on the calendar (`2024-02-31`). **Type the date printed on the document.** |
| `DATE_OUT_OF_RANGE` | The date is before 1800 or more than about 13 months in the future — almost always a misread scan. **Type the date printed on the document.** |
| `DATE_NOT_IN_EVIDENCE` | BackLog will not put a date on a file unless it can point at that exact date in the document text or in the file's own properties. It could not. This is the anti-hallucination rule. **Open the document, find the date printed on it, and type that.** If the document genuinely has no date, keep the file-modified date BackLog prefilled. If the field is unexpectedly blank, open the original file's **Properties** in File Explorer and enter its **Modified** date before filing. |
| `BAD_DATE_SOURCE` | BackLog could not record where the date came from. Every date has to be traceable to the document or the file's properties. **Type the date printed on the document.** |
| `BAD_SUBJECT` | The subject was empty, too short, too long, generic (`Scanned Document`, `New Microsoft Word Document`), contained characters SharePoint forbids, or looked like an identifier rather than a description. **Write a short subject: what this document is, in two to ten words.** |
| `BAD_DESCRIPTION` | The description was too short, too long, more than one sentence, or just repeated the subject. **Write one sentence saying what this document is and who it is from.** |
| `TOO_LONG` | Date plus subject plus extension exceeded the filename length limit (120 characters by default). **Write a shorter subject.** |

## 3. Notes on a file that *was* named

These do not stop anything. They ride along on the manifest and land in the
`SoftFlags` column of the SharePoint index, so the batch can be audited later.

| What BackLog says | What it means |
|---|---|
| `DUPLICATE_CONTENT` | The exact same document has already been filed under another name. Both copies are indexed; the later one is named `… (2)` and carries a pointer to the first. This is intended: one row per physical copy. |
| `POSSIBLE_MULTIDOC` | Several letterheads or date blocks were found, so this file may be several documents scanned into one. The name describes the first one. **Worth splitting if it matters to you.** |
| `SPAN_MISMATCH:ettin=<date>` | The optional span model proposed a different date from the one shipped. Advisory only, and only ever present if that lane is enabled — it is disabled by default. |
| `SPAN_MISMATCH_PERSISTED` | The same mismatch survived a second attempt. Worth a spot-check. |
| `DATE_FROM_FILE_MTIME` | No date was printed on the document, so the file's own modified date was used. The index records `date_source: metadata`, so this is never presented as if it came from the page. |
| `DATE_PROPOSAL_DISCARDED:<value>` | The document had no date-shaped text anywhere in it, so the date BackLog proposed, `<value>`, could not be checked against anything and was thrown away in favour of the file's own modified date. Always appears together with `DATE_FROM_FILE_MTIME`. **Worth a glance if the date matters — it is a filesystem timestamp, not something read off the page.** |
| `DATE_FROM_BODY` | The date was found deep in the document rather than in the letterhead or date line. More likely than usual to be a reference to some *other* document's date. |
| `DATE_AMBIGUOUS_FORMAT` | The date was written numerically in a form that could be read as either day/month or month/day (`03/05/2024`). **Spot-check this one.** |
| `DATE_IN_FUTURE` | The date is more than 30 days in the future. Normal for leases, renewals and hearing notices; suspicious for an invoice. |
| `DATE_SOURCE_CORRECTED:<claimed>-><actual>` | The model said the date came from one place and the checker proved it came from the other. The corrected value is what shipped. |
| `DATE_PREFERRED_FROM_DOCUMENT:<value>` | The date BackLog proposed, `<value>`, was backed only by the document's own file properties (an embedded creation date, say) — but the document itself had an unambiguous date printed near the top, so that one was used instead. An automatic upgrade in provenance, not a problem. The displaced value is kept here so the swap can be audited. |
| `SUBJECT_UNGROUNDED` | The subject is not a phrase that appears in the document. Not wrong by itself — a good summary often is not a quote — but the one to check first when a name looks off. |
| `SUBJECT_DATE_STRIPPED` | A date was removed from the subject, because the filename already starts with one. |
| `SUBJECT_EXT_STRIPPED` | A file extension (`.pdf`) was removed from the subject. |
| `SUBJECT_TRUNCATED` | The suggested subject ran past the ten-word limit a filename can carry, so it was cut to the first ten words — the form number and the party, which is what a filename is for — and any trailing separator left dangling by the cut was removed too. A trim, never an addition: nothing was invented, and the full wording is still in the file's description. |
| `DESCRIPTION_TRIMMED_TO_ONE_SENTENCE` | The description ran past one sentence, or was cut off mid-sentence, so it was trimmed back to its first complete sentence. Also a trim, never an addition. |
| `HUMAN_CORRECTED` | You corrected this file's name by hand in Needs Review. Recorded so the index shows which names were human-chosen. |

## 4. Readiness problems (Settings → Readiness)

These are about this computer, not about a document. BackLog will not start
until every required problem is clear, so a half-set-up machine cannot start a
batch that would fail on the first file. A row explicitly marked as a warning
does not block Start. Each row in the app carries the plain sentence; the code
below is what a support call needs.

| What BackLog says (code) | What it means, and what to do |
|---|---|
| `preflight_required` | No check has run yet since something changed. **Press Check this computer.** |
| `config_invalid` | Two folders are the same, or one is inside another, or one is not set. BackLog refuses this because an Outbox inside the watched Processing folder would feed its own output back through the pipeline forever. **Pick three separate folders, none inside another.** |
| `processing_unset` | No Processing folder chosen. **Settings → Processing folder → Browse.** |
| `processing_missing` | The Processing folder is not there. Usually a renamed OneDrive folder or a drive that is not connected. **Press Create this folder for me if BackLog offers it, or Browse to the right folder.** |
| `processing_unreadable` | The folder exists but Windows would not let BackLog list it. **Ask IT to check the folder's permissions.** |
| `outbox_not_writable` / `quarantine_not_writable` / `cache_not_writable` | BackLog could not create a test file in that folder — no permission, a full disk, or a OneDrive folder that is still syncing. **Check the folder opens in Explorer and that the disk is not full.** |
| `models_missing` | The everyday 0.6B naming model is missing. A normal v0.6.0 installer includes it. **Press the download action BackLog shows, or reinstall v0.6.0 if the bundled model was removed.** If the optional backup model is also absent, the combined download is about 2.4 GB; otherwise BackLog downloads only the everyday model, about 0.6 GB. No account is needed. |
| `escalation_model_missing_using_primary` (warning) | The optional 1.7B backup model is not installed. **Nothing is broken: BackLog is ready and safely reuses the everyday model for difficult naming attempts.** If you want the larger backup model and have about 1.8 GB of disk space, press **Download optional backup model**. The transfer can be cancelled and resumed. |
| `install_dir_unknown` | BackLog cannot work out where it is installed, so it cannot start its own parts. **Reinstall BackLog.** |
| `sidecar_not_found` | The part of BackLog that reads documents is missing from the installation. **Reinstall BackLog.** |
| `llama_server_not_found` | The part of BackLog that suggests names is missing. **Reinstall BackLog.** |
| `grammar_not_found` | The naming rules file is missing. **Reinstall BackLog.** |
| `sidecar_ping_failed` | The document reader is installed but did not answer. **Restart BackLog. If it keeps happening, reinstall.** Most often antivirus quarantined it after install. |
| `sidecar_check_failed` | BackLog could not finish testing the document reader — the test itself failed. **Restart BackLog.** |
| `llama_server_probe_failed` | The naming engine is installed but would not start. Nearly always antivirus, or missing runtime DLLs next to it. **Ask IT to allow `llama-server.exe`, then reinstall BackLog.** |
| `llama_server_check_failed` | BackLog could not finish testing the naming engine. **Restart BackLog.** |
| `llama_port_busy` (warning) | Another program on this computer is already using the network port BackLog reserves for naming. BackLog will keep working only if that program stops. **Ask IT to change `llama_port` in `backlog.config.json`.** This is a warning, not a blocker, because the test can produce a false positive. |

## 5. Codes you will only see in a log or a support bundle

Recorded in the ledger's event trail (Diagnostics) rather than shown in the UI.
They deliberately carry no document text — see `docs/PRIVACY.md`.

| Code | Meaning |
|---|---|
| `PANIC` / `TIMEOUT` / `ENCRYPTED` / `ERROR` | Value-free classification of a failed conversion attempt. The full message goes to the app log, never to the ledger, because the raw error embeds the document's full path. |
| `QUARANTINE_FAILED` | BackLog wrote the flagged manifest but could not move the file into Quarantine. **The source file is still in Processing and is safe.** Check the Quarantine folder's permissions. |
| `RESTORE_FAILED` | You approved a corrected name, but the file could not be moved back out of Quarantine into Processing for re-emission. Check both folders' permissions and Try again. |

## Still stuck

1. In **Needs Review**, press **What happened** on the file. That is the
   step-by-step trail BackLog recorded for it, in order, and it is usually
   enough on its own.
2. Note the exact code from the tables above and the file's original name.
3. The app's log files are in `%APPDATA%\ai.sonomos.backlog\logs`. Folder paths
   inside them are redacted and no document text is ever written there — see
   `docs/PRIVACY.md`. **Settings** shows the version numbers a support call
   needs.
4. Nothing has been lost. Every source file is either in Processing, in
   Quarantine, or already filed — `docs/PRIVACY.md` says exactly where.
