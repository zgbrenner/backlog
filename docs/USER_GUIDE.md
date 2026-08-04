# BackLog: a guide for the person who runs it

BackLog watches a folder, works out what each document is, and gives it a
useful name. You can use the existing **Power Automate / SharePoint** handoff
or have BackLog file finished renamed documents directly into a **Local
folder**.

You do not need a terminal, Python, a separate model download, or a developer
to install it.

## 1. Install one file - or use the no-installer package

For the normal per-user install, download **`BackLog_0.8.0_x64-setup.exe`**
from the [BackLog v0.8.0 release page](https://github.com/zgbrenner/backlog/releases)
and double-click it. This is the only required download.

If you are moving BackLog to another Windows x64 laptop and do not want an
installer, download **`BackLog_0.8.0_x64-portable.zip`**, extract it completely,
and double-click **`BackLog-Portable.cmd`**. The portable ZIP includes its own
fixed WebView2 runtime and does not require a separate runtime, Python, VC++
Redistributable, administrator password, or model download to launch. Keep the
extracted folder together. See [`PORTABLE.md`](PORTABLE.md) for the short
version.

The installer includes:

- the BackLog app;
- the document-conversion runtime;
- the llama naming server and its Windows runtime libraries;
- the verified Qwen3 0.6B everyday model;
- the pinned MiniLM semantic evidence model; and
- the offline WebView2 runtime.

It installs for your Windows user account and does not need the internet during
installation. It must not ask for an administrator password. The portable
package has the same app and model contents, but is updated by downloading a
new ZIP rather than through the installer/updater path.

Windows will probably show **Windows protected your PC** because the installer
does not yet have a trusted Authenticode certificate. Click **More info**, then
**Run anyway** only if you obtained the file from the BackLog release page. If
your organization blocks unsigned applications, ask IT to review
`docs/SECURITY.md` and `NOTICE.md`.

## 2. Set it up once

Open **Settings**. First choose an **Output mode**:

- **Power Automate / SharePoint** (the default) writes a handoff manifest to
  Outbox for Flow 2.
- **Local folder** writes a finished renamed document to Local Output and a
  JSON receipt for that delivery under `.backlog/receipts` inside Local Output.

Then BackLog leads you through three steps:

1. Choose **Processing**, **Quarantine**, and **Outbox** for Power Automate,
   or **Processing**, **Quarantine**, and **Local Output** for Local folder.
2. Press **Save and check this computer**.
3. Read any Blocked message and use the action beside it.

The Processing folder is the watched intake folder. New documents arrive
there, either because you put them there or because the intake flow did.

Outbox is only for Power Automate manifests. Local Output is only for finished
renamed documents and their receipts; Local mode does not write an Outbox
manifest, a SharePoint index, or a cloud archive. Quarantine is the local
folder where files needing a person wait. Keep the three folders required by
your selected mode separate; do not put one inside another.

The check confirms that the folders are usable and that the bundled document
reader, naming server, rules, and everyday model can start. **Start** becomes
available when every required check passes.

### Local folder quick start

1. Choose **Local folder** under **Output mode**.
2. Browse to separate **Processing**, **Local Output**, and **Quarantine**
   folders, then press **Save and check this computer**.
3. Press **Start** and drop documents into Processing.
4. Find each finished renamed document in Local Output. Its matching receipt
   is at `.backlog/receipts/<manifest_id>.json` below that same folder.

If an output name already exists, BackLog keeps it and chooses a deterministic
suffix such as `(2)` for the new file. It removes the Processing source only
after the renamed output and its receipt are durably written. If Windows or
BackLog stops, restart it: recovery reconciles unfinished deliveries rather
than overwriting an output.

### Optional model

The larger Qwen3 1.7B model is optional. It can help with difficult documents,
but it uses about 1.8 GB more disk space and more memory while running. BackLog
works without it by safely reusing the everyday 0.6B model for backup naming
attempts.

To add it, use **Download models** in Settings. BackLog verifies the bundled
everyday model and downloads only anything missing. You can choose **Cancel
download** and later **Resume download**; partial data is retained safely. The
download result remains visible if you leave Settings and come back.

Document processing itself remains local and works offline. The only routine
network operations are this optional model download and the startup update
check.

## 3. Daily use

Press **Start**. BackLog first sweeps files already in Processing, then watches
for new arrivals. Closing the window hides it to the system tray; use the tray
menu to quit it.

The main states mean:

| State | Meaning |
|---|---|
| **Processing** | BackLog is watching or working through the intake folder. |
| **Needs Review** | BackLog needs a person to decide or correct something. |
| **Done** | Power Automate mode: BackLog handed off a manifest. Local folder mode: BackLog wrote the renamed file and its receipt. |

In **Power Automate / SharePoint** mode, **Done does not mean SharePoint has
finished.** The downstream flow owns the later rename, archive, SharePoint
copy, and list update; see [`power-automate/BUILD-GUIDE.md`](../power-automate/BUILD-GUIDE.md).
In **Local folder** mode, Done is a local delivery, not a Power Automate or
SharePoint result.

When no file is processing:

- **All caught up** means no processing or review work remains.
- **Processing is caught up** means the intake work is clear but one or more
  documents still need a person in Needs Review.

**Pause** stops new work without losing files. Anything arriving while paused
is picked up after Resume.

## 4. Needs Review

Needs Review is expected, especially for poor scans, packets containing several
documents, and documents without a trustworthy date.

BackLog never invents a date just to clear the queue. If it cannot verify a
date against the page or the file's embedded properties, a person must review
the document. If the document genuinely has no date, keep the file-modified
date that BackLog prefilled. If the date field is unexpectedly blank, find the
original file in File Explorer, open **Properties**, and enter its **Modified**
date before filing. BackLog records that human correction rather than
pretending the date appeared on the page.

Each card shows the original filename, the reason it stopped, the proposed
date/subject/description, the extracted text, and the event trail. You can
filter by reason and sort oldest or newest first.

- **Approve and file** uses your corrections and records that a person chose
  them. In Local folder mode it files directly from Quarantine into Local
  Output; in Power Automate mode it updates the handoff for Flow 2. A short
  Undo window follows you if you navigate to another tab.
- **Try again** sends the document through the bounded retry path.
- **Can't fix this** sets it aside after confirmation. The document stays in
  Quarantine for manual handling, but the dismissal cannot currently be
  reversed inside BackLog.

A good subject is two to ten words and says what the document is. A good
description is one sentence. If a correction breaks a rule, BackLog explains
which rule.

## 5. Large backfills

For several thousand files:

1. Add a few hundred at a time so Needs Review stays manageable.
2. In Power Automate mode, ask the flow owner to set
   `manifest_emit_per_min` to 10. It does not apply to Local folder delivery.
3. Leave BackLog running overnight; hiding the window is safe.
4. Work through Needs Review in sittings. Nothing expires.

## 6. Updates and trust

BackLog checks for an update at startup. Stable updates are signed with the
Tauri updater key and are rejected if the signature does not match the public
key already inside the app.

That updater signature is different from Windows Authenticode signing. The
first protects the in-app update channel; the second establishes publisher
identity and SmartScreen reputation for the installer. A correctly signed
BackLog v0.8.0 build can still show a SmartScreen warning because a trusted
Authenticode certificate has not yet been configured.

An unsigned v0.8.0 build may appear as a prerelease for manual testing. It is
not offered through the stable updater; v0.4.4 remains the stable updater until
a correctly signed v0.8.0 release exists.

## Help

| Need | Read |
|---|---|
| A message or blocked check | `docs/TROUBLESHOOTING.md` |
| What happens to documents | `docs/PRIVACY.md` |
| IT/security review | `docs/SECURITY.md` |
| Pilot rollout | `docs/PILOT_RUNBOOK.md` |
| Known limitations | `docs/KNOWN_ISSUES.md` |
