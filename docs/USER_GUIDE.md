# BackLog: a guide for the person who runs it

BackLog watches a folder, works out what each document is, gives it a name like
`2024-05-31 Acme Services Agreement.pdf`, and hands it to your SharePoint.

You will not need a terminal, a command, or a developer, at any point.

If something goes wrong, `TROUBLESHOOTING.md` lists every message BackLog can
show you and what to do about each one. `PRIVACY.md` explains exactly what
happens to your documents.

---

## 1. Install it

1. Double-click `BackLog_<version>_x64-setup.exe`.
2. **Windows will probably warn you.** BackLog is not yet code-signed, so
   SmartScreen shows a blue box saying *"Windows protected your PC"*. Click
   **More info**, then **Run anyway**. Windows Defender may also ask; choose
   **Allow**.
3. It installs for **your user account only** and does not ask for an
   administrator password. If it does ask for one, stop — that is not the right
   installer.
4. BackLog opens. There is nothing to configure during install.

If your IT department manages this computer, give them `docs/SECURITY.md` and
`NOTICE.md` before you run the installer.

## 2. Set it up (once)

Open the **Settings** tab. There are three folders to choose and one download
to start.

### The three folders

| Folder | What to pick |
|---|---|
| **Processing folder** | The OneDrive-synced folder your documents arrive in. This is the folder BackLog watches. |
| **Outbox folder** | Also OneDrive-synced. BackLog writes small instruction files here for Power Automate to collect. |
| **Quarantine folder** | A **local** folder, not synced. Anything BackLog cannot confidently name is moved here so you can look at it. |

Use **Browse** next to each. Three rules, which BackLog enforces:

- All three must be different folders.
- None may be inside another.
- BackLog must be able to write to the Outbox and Quarantine folders.

Once you set the Processing folder, BackLog tells you how many files it can see
in it. That number is the quickest way to confirm you picked the folder you
meant.

### The models

Under **Readiness** you will see a button: **Download models (~2.4 GB)**.

Press it. It fetches the two files BackLog uses to read and name documents,
once, from a public source. No account and no login. It shows a progress bar
and you can carry on filling in your folders while it runs. If it is
interrupted, press it again — it picks up where it stopped.

After this, BackLog never needs the internet to process a document.

### Check

Press **Check this computer**. BackLog tests everything and shows a list:

```
Processing folder is readable              Ready
Outbox folder is writable                  Ready
Quarantine folder is writable              Ready
Working folder is writable                 Ready
Document reader (convertd) is installed    Ready
Document reader answers                    Ready
Naming engine (llama-server) is installed  Ready
Naming engine starts                       Ready
Naming rules file is installed             Ready
Everyday model file is present             Ready
Backup model file is present               Ready
```

Before you press it they all say **Not checked**, which is honest — nothing has
been examined yet. Afterwards each is **Ready** or **Blocked**. Anything
Blocked comes with a sentence saying what it means and, where BackLog can fix
it itself, a button that does. Section 4 of `TROUBLESHOOTING.md` covers every
one of them.

**Start stays greyed out until all eleven are Ready.** That is on purpose: a
half-configured machine would fail on the first document and you would find out
an hour later.

## 3. Run it

Press **Start**.

BackLog first sweeps everything already in the Processing folder, then keeps
watching for new arrivals. Files appear in the **Queue** tab as it works
through them.

The state on each row, in the order they happen:

| It says | It means |
|---|---|
| **Queued** | Seen, not started. |
| **Reading** | Getting the text out (this is the slow step for scans). |
| **Understanding** | Picking out the dates and the subject. |
| **Naming** | The model is proposing a name. |
| **Checking** | Verifying the proposal against the document — no date ships unless it is actually in there. |
| **Done** | Named and handed over to Power Automate. |
| **Needs review** | BackLog would not name it and wants you. |
| **Dismissed** | You decided this one does not need filing. |

**Closing the window does not stop BackLog.** It hides to the system tray
(bottom-right, by the clock) and keeps working. To actually stop it, right-click
the tray icon and choose **Quit**. That is deliberate: an accidental click on
the X in the middle of a two-thousand-file batch used to kill the batch.

**Pause** stops new work without losing anything. Files that arrive while
paused are still picked up when you resume.

## 4. Needs Review

This is the part of BackLog that is a job rather than a machine. Everything
BackLog would not name confidently lands here, and it is expected to happen —
scans that came out badly, documents with no date on them, packets of several
documents in one PDF.

Each card shows:

- **The original filename** and a plain sentence saying why it stopped.
- **Date, Subject, Description** — pre-filled with whatever BackLog got as far
  as, ready for you to correct.
- **Document text** — what BackLog actually read. Open this first; it is usually
  obvious immediately whether the scan came out or not.
- **What happened** — the step-by-step trail for this file.
- **Show me the file** — opens the original in Explorer.

Then one of three buttons:

- **Approve and file** — your corrections go through the same safety checks the
  model's proposals do, then the file is named and handed on. The index records
  that a person chose this name (`date_source: human`, and a `HUMAN_CORRECTED`
  note).
- **Try again** — put it back through the pipeline. Worth one try for a timeout
  or a temporary failure; pointless for a bad scan.
- **Can't fix this** — set it aside. Use this for junk, sync artefacts and
  duplicates you do not want indexed. It is recorded as a decision you made,
  not as work completed, so it never inflates the "done" count.

### Writing a good name

BackLog will reject your correction too, if it breaks a rule. The rules:

- **The date must be one that is actually on the document**, or in the file's
  properties. If there genuinely is no date, leave it blank — BackLog will use
  the file's own date and label it honestly rather than pretending.
- **The subject is two to ten words**, describing what the document *is*.
  "Acme Services Agreement", not "Scanned Document" and not "Document1".
- **The description is one sentence.** What it is and who it is from.

If something is rejected, the message says which rule and why. Section 2 of
`TROUBLESHOOTING.md` lists them all.

## 5. The big backfill

For an initial run of several thousand files:

1. Copy files into the Processing folder **in batches**, not all at once. A few
   hundred at a time keeps the review queue a size a person can actually work
   through.
2. Ask whoever set up your flows to set `manifest_emit_per_min` to **10**.
   Without it BackLog hands SharePoint work as fast as it can produce it, and
   SharePoint starts refusing (`429`), which is slower than pacing would have
   been. `docs/PILOT_RUNBOOK.md` covers the staged approach.
3. Leave it running overnight. Closing the window is fine; it keeps going.
4. Do the review queue in sittings. Nothing expires.

## 6. When BackLog updates itself

BackLog checks for a new version when it starts. If there is one, a bar appears
at the top offering it. Updates are cryptographically signed and BackLog will
refuse one that is not — so if the bar appears, the update is genuine.

Choosing it downloads and installs the new version and restarts BackLog. You
can dismiss the bar and carry on; it will offer again next time.

## Where things are

| | |
|---|---|
| Every message BackLog can show you | `TROUBLESHOOTING.md` |
| What happens to your documents | `PRIVACY.md` |
| For your IT department | `SECURITY.md` |
| Setting up the two Power Automate flows | `../power-automate/FLOW1-intake.md`, `FLOW2-commit.md` |
| Rolling this out carefully | `PILOT_RUNBOOK.md` |
| What is not finished yet | `KNOWN_ISSUES.md` |
