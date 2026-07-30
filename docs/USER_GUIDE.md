# BackLog: a guide for the person who runs it

BackLog watches a folder, works out what each document is, gives it a useful
name, and hands it to the Power Automate flow that files it in SharePoint.

You do not need a terminal, Python, a separate model download, or a developer
to install it.

## 1. Install one file

Download **`BackLog_0.5.0_x64-setup.exe`** from the BackLog v0.5.0 release page
and double-click it. This is the only required download.

The installer includes:

- the BackLog app;
- the document-conversion runtime;
- the llama naming server and its Windows runtime libraries;
- the verified Qwen3 0.6B everyday model; and
- the offline WebView2 runtime.

It installs for your Windows user account and does not need the internet during
installation. It must not ask for an administrator password.

Windows will probably show **Windows protected your PC** because the installer
does not yet have a trusted Authenticode certificate. Click **More info**, then
**Run anyway** only if you obtained the file from the BackLog release page. If
your organization blocks unsigned applications, ask IT to review
`docs/SECURITY.md` and `NOTICE.md`.

## 2. Set it up once

Open **Settings**. BackLog leads you through three steps:

1. Choose the **Processing**, **Outbox**, and **Quarantine** folders.
2. Press **Save and check this computer**.
3. Read any Blocked message and use the action beside it.

The Processing folder is the watched intake folder. New documents arrive
there, either because you put them there or because the intake flow did.

Outbox is where BackLog writes manifests for Power Automate. Quarantine is the
local folder where files needing a person wait. Keep all three folders
separate; do not put one inside another.

The check confirms that the folders are usable and that the bundled document
reader, naming server, rules, and everyday model can start. **Start** becomes
available when every required check passes.

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
| **Done** | BackLog handed a manifest to Power Automate. |

**Done does not mean SharePoint has finished.** The downstream Power Automate
flow owns the later rename, archive, SharePoint copy, and list update. Its
status and retry policy are separate from BackLog.

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
  them. A short Undo window follows you if you navigate to another tab.
- **Try again** sends the document through the bounded retry path.
- **Can't fix this** sets it aside after confirmation. The document stays in
  Quarantine, but the dismissal cannot currently be reversed inside BackLog.

A good subject is two to ten words and says what the document is. A good
description is one sentence. If a correction breaks a rule, BackLog explains
which rule.

## 5. Large backfills

For several thousand files:

1. Add a few hundred at a time so Needs Review stays manageable.
2. Ask the flow owner to set `manifest_emit_per_min` to 10.
3. Leave BackLog running overnight; hiding the window is safe.
4. Work through Needs Review in sittings. Nothing expires.

## 6. Updates and trust

BackLog checks for an update at startup. Stable updates are signed with the
Tauri updater key and are rejected if the signature does not match the public
key already inside the app.

That updater signature is different from Windows Authenticode signing. The
first protects the in-app update channel; the second establishes publisher
identity and SmartScreen reputation for the installer. BackLog v0.5.0 can have
a valid updater signature while still showing a SmartScreen warning because a
trusted Authenticode certificate has not yet been configured.

An unsigned v0.5.0 build may appear as a prerelease for manual testing. It is
not offered through the stable updater; v0.4.4 remains the stable updater until
a correctly signed v0.5.0 release exists.

## Help

| Need | Read |
|---|---|
| A message or blocked check | `docs/TROUBLESHOOTING.md` |
| What happens to documents | `docs/PRIVACY.md` |
| IT/security review | `docs/SECURITY.md` |
| Pilot rollout | `docs/PILOT_RUNBOOK.md` |
| Known limitations | `docs/KNOWN_ISSUES.md` |
