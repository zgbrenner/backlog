# Changelog

Notable changes to BackLog. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow the
single number that `package.json`, `src-tauri/tauri.conf.json` and
`src-tauri/Cargo.toml` must agree on (CI enforces this — see
`.github/scripts/check-versions.mjs`).

`RELEASING.md`'s "Cutting a release" procedure references this file: step 1 adds
the section for the version being cut, and the release notes and the `notes`
field of `latest.json` should quote it.

> **Provenance.** The 0.2.0 section was reconstructed from the working tree and
> the superseded `PRODUCTION_READINESS.md` rather than from a per-commit log,
> because the pre-0.2.0 history was squashed. Treat it as an accurate summary
> of *what the code does now*, not as a commit-by-commit record.

## [0.8.2] — the appliance behaves like one

### Changed

- The naming servers now respect the rest of the machine. Both llama-server
  tiers get an explicit thread budget (`cores − convert_workers`, floor 2)
  instead of each claiming every core beside six conversion workers doing
  the same — the measured order-of-magnitude batch slowdown. The primary
  server is recycled after `slm_recycle_after_requests` served requests
  (default 64; llama.cpp's Windows memory growth is unfixed upstream), the
  1.7B escalation server is retired after `slm_escalation_idle_secs`
  (default 600) without a completed request — never mid-request — and
  escalation runs with its own small `slm_escalation_parallel` (RAM-tiered
  1–2) instead of inheriting the primary's, returning roughly a gigabyte on
  16 GB machines.
- Idle conversion workers are now reaped. A worker that has served an OCR
  or language call holds 450–530 MB for the life of the process; after
  `convert_idle_reap_secs` (default 300) idle, the pool shrinks to
  `convert_min_idle_workers` (default 1) and respawns on demand in about a
  second. The per-worker memory budget was corrected to the measured
  figure, which drops the 8 GB tier from two conversion workers to one —
  two consumed nearly the whole tier's margin before any document work.
- A file re-processed after a crash or restart no longer redoes conversion
  and OCR when its cached conversion is still valid: a new sidecar
  metadata artifact carries what the resume needs, any mismatch falls
  through silently to a full conversion, and the cache is now written
  atomically and before the ledger admits the stage — closing a window
  where a crash could leave a converted state with no cache behind it.
- Plain-text files are read through a 2 MiB cap instead of feeding up to
  64 MB into charset detection for output that was truncated to 200k
  characters anyway; OCR's ONNX sessions are capped to two threads each.

## [0.8.1] — no file left behind

### Security

- The updater signing key was rotated (new minisign ID `199849F4EB9588F7`);
  the private half of the previous key (`F6BB6D5A6C3954C6`) was lost, so no
  release after v0.4.4 could be updater-signed. Existing v0.4.x and v0.8.0
  installations cannot verify this release through the in-app updater and
  need one manual download of v0.8.1; updates verify automatically from then
  on.

### Fixed

- A same-content copy arriving while its original was still queued was parked
  for a fixed window of wall-clock time and then silently abandoned — no
  ledger row, no flag, no UI count, the file just sat in Processing. The copy
  now waits for the original's terminal transition (which the original's own
  wall clock guarantees) and then gets its own duplicate delivery; the old
  window only paces a diagnostic log line.
- A copy of a file already waiting in Needs Review vanished without a trace —
  and because every zero-byte file shares one content hash, the second empty
  file always hit this path and restarts never recovered it. The copy is now
  moved into Quarantine beside its original, with a ledger event and a
  `DUPLICATE_QUARANTINE_FAILED` troubleshooting entry for the failure case.

### Changed

- Descriptions now open in register style — "Shareholder's register
  transferring 40,000 shares to John Smith." — never "The document is a…".
  The checker strips the preamble deterministically from model proposals
  (human-typed descriptions pass through untouched, and stripping never takes
  a description under the length floor), and the naming prompt tells the
  model not to write it in the first place.

## [0.8.0] — native Local Output delivery

### Added

- An explicit **Local folder** output mode alongside the existing default
  **Power Automate / SharePoint** handoff. Local mode writes a finished renamed
  document directly to the selected Local Output folder and records one JSON
  receipt per delivery under `.backlog/receipts`.
- Mode-aware setup, readiness, folder selection, and Needs Review guidance.
  Power Automate mode continues to write manifests to Outbox for Flow 2; Local
  mode does not write a Power Automate manifest, SharePoint index, or cloud
  archive.
- Receipt-backed no-overwrite delivery and recovery. Local Output retains an
  unrelated existing file, selects a deterministic collision suffix, and only
  removes a source after its output and receipt are durable.

### Changed

- A job's delivery mode and root are pinned at ingest. Later Settings changes
  cannot redirect a flagged correction or recovery between Local Output and
  Power Automate.

### Release notes

- Local testing covers the application contracts and bounded Local Output
  acceptance path. Target-tenant Power Automate Flow 1/Flow 2 testing,
  Authenticode, and updater signing remain separate release gates.

## [0.7.0] — portable offline release and trust-boundary hardening

### Added

- Installer-free Windows x64 portable ZIP packaging with a self-contained
  launcher and verified release manifest.
- Clearer daily-use guidance for portable launches and low-memory machines.

### Fixed

- Bound oversized model fields before deterministic validation can allocate or
  echo attacker-controlled content.
- Preserve a later unambiguous date reading when an earlier reading is
  ambiguous.

## [0.6.0] — exact semantic evidence and guarded release packaging

### Added

- Local, torch-free ONNX paragraph ranking and cached-label entity extraction.
  Both lanes preserve exact source text, paragraph indices, and character
  offsets, and degrade to deterministic evidence when the optional model is
  unavailable.
- Hash-pinned MiniLM model assets are staged, copied into the per-user runtime
  model directory on first launch, and exercised by the frozen-sidecar smoke
  contract.

### Fixed

- The release workflow now derives the version, tag, installer name, and
  updater paths from validated package/Tauri/Cargo metadata on the exact CI
  commit. A stale hand-edited release version can no longer publish a mismatched
  installer.
- Rust formatting drift and Windows development-stub parity are covered before
  release verification.

## [0.5.0] — reliable recovery and one-download setup

### Fixed

- Controlled shutdown now prevents model-server respawn, releases every active
  ledger claim, and makes abandoned work immediately recoverable on restart.
  Persisted terminal manifests are reconciled before model work is repeated,
  and a failed quarantine or manifest write leaves the job visible and
  retryable.
- A missing optional 1.7B model no longer overwrites the saved model choice.
  BackLog honestly reports the missing file and safely reuses the installed
  0.6B primary model for escalation attempts.
- Model downloads always use distinct canonical destinations, retain partial
  transfers for resume, and expose completed, failed, and cancelled outcomes
  after navigating away from Settings.
- The public CI workspace job now stages a deterministic marker model for
  Tauri resource validation. Release staging hash-verifies the real model and
  cannot publish that marker.

### Changed

- First run is a three-step, save-then-check flow. Active downloads can be
  cancelled and later resumed, review approvals remain undoable across
  navigation, and empty queues distinguish caught-up processing from review
  work still waiting for a person.
- The Windows installer is now the only required download. It contains the
  app, conversion sidecar and Python runtime, llama.cpp server and runtime
  libraries, verified Qwen3 0.6B primary model, and offline WebView2 runtime.
  The larger Qwen3 1.7B escalation model remains an optional in-app download
  for difficult documents.
- Resource defaults are clamped for an 8 GB Windows laptop.

### Release

- A push to `main` starts a clean `windows-2022` workflow when `v0.5.0` is
  absent. Later pushes skip cleanly after the tag exists. The build uses the
  repository lockfiles and pinned model/llama.cpp inputs.
- When the Tauri updater key is available, the workflow publishes the stable
  installer, detached signature, and `latest.json`. Without that key it
  publishes an installer-only prerelease and leaves v0.4.4 as the stable
  updater. It never invents a signature or publishes unsigned updater
  metadata.

## [0.4.4] — the installer was missing three DLLs it could never have noticed

### Fixed

**Every `ggml*.dll` and `llama*.dll` in the bundle imports `vcruntime140.dll` and
`msvcp140.dll`, and two also import `vcruntime140_1.dll`. None of the three was
shipped, and Windows does not have them** — they arrive with the Visual C++
2015–2022 Redistributable. On a machine that had never installed it, every
release up to and including 0.4.3 would install cleanly, launch, pass its own
readiness check, and then fail to start the naming engine, because the exe and
all 29 of its DLLs were present and the missing piece was one level below them.
All three are now staged app-locally beside the executables that import them
(`NOTICE.md` records the redistribution).

**Why no amount of testing here would have found it:** every machine that can
build a Tauri app has the redistributable installed, so the dependency is always
satisfied on the machine that packages the release and never mentioned by
anything that runs there. It is only visible by reading the import tables, which
is now a gate — `scripts/verify-binaries.ps1` parses the import directory of
every shipped binary and fails the release if any imported DLL is neither in the
bundle nor part of a stock Windows. Verified by removing `vcruntime140.dll` and
confirming it exits 1 naming the file, rather than trusting that a new check
works.

### Changed

- `.githooks/pre-push` reads the ref list git feeds it and skips when every ref
  is a commit `origin` already has. Pushing a tag straight after its branch was
  re-running the whole three-minute suite on an identical tree, and two cargo
  builds seconds apart contend over `target/` — the second run failed a gate the
  first had just passed and then passed on retry with nothing changed.

### Documentation

- **`docs/KNOWN_ISSUES.md` item 11 was wrong, and `ci.yml`'s header comment was
  the reason nobody noticed.** Both stated that Actions had never executed a
  step because the private repository's allowance was spent. That was true when
  written; the repository is public now, runs execute normally, and the timing
  API reports `billable_ms: 0` for every one of them because public repositories
  are not billed. The accurate statement is narrower: four of the five jobs
  pass, and `Workspace (app crate)` cannot, because `tauri-build` resolves
  `bundle.resources` and `resources/models/*.gguf` matches nothing on a runner
  that has no 2.4 GB of weights. A red X on that job is the expected state
  rather than a broken build, and the other four jobs are the signal.

## [0.4.3] — the filename says whose document it is

### A character cap cannot enforce a word count

**Fixed — the naming schema's `maxLength: 64` on `subject` was severing the
answer, not shortening it.** llama.cpp enforces `maxLength` by refusing to emit
another character, so **18 of 40 subjects came back at exactly 64 characters**,
mid-word, with no flag on any of them — the word count was still under ten, so the
checker's trimmer never engaged:

```
'Form 4562 - Depreciation and Amortization Return for Yolanda Bea'   (Beaumont)
'Tax Return - Supplemental Income and Loss (Rental Real Estate) -'   (party was next)
'S Corporation Tax Return for Whitmore & Associates - Internal 11'
```

This is the same defect 0.4.1 fixed for `description` and introduced for `subject`
in the same change, reasoning that "64 characters is about ten words of ordinary
English". The schema counts characters; the rule is about words; the two do not
substitute for one another.

The cap is now **95**, which is not a judgement call but the whole filename budget:
`compose` builds `"YYYY-MM-DD " + subject` and needs `FILENAME_TAIL_RESERVE` on
top, so at `max_filename_len: 120` a 95-character subject never trips `TooLong`. A
test derives that from the constants, so moving either side fails loudly instead of
producing documents that quarantine on length.

**Added — the naming prompt now prescribes the subject's shape**, `<form> -
<party>`, with the short form identifier rather than the form's full legal title.
`Form 8829 - Expenses for Business Use of Your Home` is exactly ten words on its
own, so a model leading with the legal title had nothing left for the party even
when the cap was not cutting it off.

**Fixed — a subject that arrives already ending in a separator is tidied.** The
word trim always cleaned its own cut, but a subject severed by the schema could
ship as `"... (Rental Real Estate) -"`, pointing at a party that was never written.
`sanitize_subject_inner` now strips a dangling tail from every model subject.
Unflagged deliberately: it drops punctuation, never a word.

### Measured, same 40 documents, same settings

Scored against each document's own `Taxpayer / Entity:` line, because the corpus
draws the filename's party and the body's party independently:

| | 0.4.2 | 0.4.3 |
|---|---|---|
| party named **exactly** | 18 | **38** |
| party cut short | 8 | **0** |
| party garbled | 2 | **0** |
| party absent entirely | 12 | **2** |
| subjects at exactly the old 64-char cap | 18 | **0** (longest 87) |

**The date lane improved without being touched**, which was not predicted:

| | 0.4.2 | 0.4.3 |
|---|---|---|
| `dated_deep` named from its own date | 2 of 8 | **4 of 8** |
| named with the run date | 16 of 40 | **14 of 40** |
| `date_source: document` | 24 | **26** |
| `DATE_PREFERRED_FROM_DOCUMENT` fired | 21 | **9** |

No date logic changed between those runs. The likely reading is that the evidence
always held the date and the model was the bottleneck: one not spending its output
on a severed form title also picks the date better. Ten of the fourteen remaining
run-dated files have no date to find, so the genuine misses are **four**, down from
six — and the harvest-window ceiling of `KNOWN_ISSUES.md` item 0c is a smaller
problem than 2-of-8 made it look.

### What it cost, and the attempt that cost twice as much

**Throughput fell 19% — 9.58 to 11.61 s/file, so 1,000 files goes from 2.7 to 3.2
hours.** Naming stays at 40 of 40.

The first version of this prompt stated each subject prohibition as its own rule,
four bullets instead of two, and cost **22.95 s/file — 6.4 hours per 1,000** for 37
of 40 on the party and 39 of 40 named. Twice the wall clock for slightly worse
results: a system prompt is re-sent on every naming attempt and every escalation, so
prompt length is a throughput decision here and not only a quality one.

**How that was nearly got wrong is worth recording.** Read on its first ten
documents, the four-bullet prompt looked clearly better and the short one looked
broken — a repeated party, a leaked EIN, `"... - 2021 - 2021 - 2021"`. Scoring all
40 reversed it: the degenerate cases were real but rare, and the party was still
correct in them because the word trim removes the tail. The comment above the prompt
now says to score the whole sample, because ten documents mislead on this corpus.

Two things this leaves open, both recorded rather than rushed:
`SUBJECT_TRUNCATED` now fires on 35 of 40 and has stopped distinguishing anything
(item 0g), and the pipeline still records neither how many naming attempts a document
took nor which tier answered — which is why settling the throughput question took
three full runs instead of one (item 0h).

### The gates now enforce themselves, locally

GitHub Actions has never assigned a runner for this repository and never will on
this account, so `scripts/ci-local.sh` was the only thing standing between a bad
change and `origin` — and nothing invoked it. `.git/hooks/` held only the stock
samples, despite a comment in that script suggesting a symlink since 0.3.0.

- **`.githooks/pre-commit`** — `cargo fmt --check` plus the five file-reading node
  gates. **2.2 seconds.** A commit hook that takes minutes gets bypassed and then
  guards nothing, so what lives here is decided by the clock: everything that
  compiles, bundles or launches a browser stays in pre-push.
- **`.githooks/pre-push`** — the whole of `scripts/ci-local.sh`, ~190 s.
- **`scripts/install-hooks.ps1` / `.sh`** — point `core.hooksPath` at `.githooks`
  and then verify it: config value, hook presence, CRLF detection, `bash -n`, and a
  warning when a hook is tracked as mode `100644`, which Linux and macOS clones
  silently skip.
- **Tracked, not symlinked into `.git/hooks`.** That directory is not version
  controlled, so a link dies with the clone that made it and the next clone is
  silently unenforced — the same failure as a CI file nobody can run.
- **`.gitattributes` pins `.githooks/** text eol=lf`.** With `core.autocrlf=true`
  set globally on a dev machine, CRLF hooks fail with `bad interpreter` and every
  gate stops running.
- Both hooks honour `BACKLOG_SKIP_HOOKS=1`, print how to bypass when they fail,
  and hand off to a global `core.hooksPath` hook of the same name afterwards, so
  installing these does not silently disable a machine-wide hook.

## [0.4.2] — a date on the page beats a date in the file's properties

### Filesystem timestamps are no longer evidence

**Fixed — `pipeline.rs` merged the file's own mtime and ctime into the list of
dates the checker validates a proposal against.** A model that proposed today's
date was therefore *correct*: the file had been copied into Processing today, so
today was in evidence. The tripwire that exists to catch fabricated dates could
not fire on the most likely fabrication.

The mtime never needed to be there. `check_with` already receives it separately
as the fallback for a genuinely undated document, so removing it from the
evidence list costs nothing and closes the loop where a filesystem timestamp was
the evidence for itself. `README.md`'s guarantee now says "**embedded**
metadata" and says why.

**This alone changed almost nothing, which is worth recording.** The synthetic
corpus was generated the same day it was measured, so the fixtures' *embedded*
`created` property also said today — a model proposing today was matching real
embedded evidence, exactly as the guarantee allows. The earlier attribution of
"25 of 29 documents named with the run date" to filesystem timestamps was wrong.

### A date printed on the page now outranks the document's properties

**Added — `Checker::date_printed_on_the_page`.** Handed both a date in the text
and a date in the PDF/DOCX properties, the model usually took the property, and
several proposals claimed `date_source: "document"` while naming a date that
appears nowhere in the text. When a proposal is supported only by embedded
metadata and the document itself carries an unambiguous date in its head region,
that printed date is substituted and the displaced proposal is recorded as
`DATE_PREFERRED_FROM_DOCUMENT:<value>`.

The substitution is strictly a provenance upgrade — regex evidence read out of
the document replacing a property the reader cannot see. Only unambiguous
head-region dates qualify: a date deep in the body is more likely a reference to
*another* document, which is what `DATE_FROM_BODY` has always warned about.

**Measured on a 40-document stratified sample, 0.6B + 1.7B, `slm_parallel: 1`:**

| | 0.4.1 | 0.4.2 |
|---|---|---|
| named `ok` | 40 of 40 | 40 of 40 |
| named with the run date instead of the document's own | 37 of 40 | **16 of 40** |
| documents with a date on page one, named from it | 2 of 16 | **16 of 16** |

The naming *rate* is unchanged — 0.4.1 already got every document named. What
changed is whether the date in the name belongs to the document.

The remaining sixteen break down as ten fixtures that carry no date at all,
where the mtime is the correct answer, and **six** whose only date sits past the
harvest window — now recorded as `docs/KNOWN_ISSUES.md` item 0c rather than
folded into this release. Those six are the only genuine misses on the sample.

**Fixed — folded dates carried a fabricated position that could win that
preference.** `filter.rs` folds dates found by the salience and Ettin lanes back
into the harvest so they count as evidence. An offset from `extract_dates` is
relative to the sentence it was given, not to the document, so a date from page
five arrived with a small offset that read as "near the top" — and an Ettin span
arrived with a hardcoded `0`. Both now record `POSITION_UNKNOWN`. They still
count as evidence; they cannot win the letterhead tie-break, because their real
position genuinely is not known at that point.

### Measured for the first time, and not fixed

The date half of a filename is now measured to death. The **party** half never
had been, so it was scored against each document's own `Taxpayer / Entity:` line
across the same 40: **18 exact, 8 a correct prefix cut short, 2 garbled, and 12
with no party in the filename at all.**

Nothing in this release addresses it, and the results are recorded as
`docs/KNOWN_ISSUES.md` items 0d, 0e and 0f rather than being described as
anything other than open:

- **0d — three in ten filenames name no party.** A budget problem, not a reading
  problem: the subject is capped at ten words and
  `Form 8829 - Expenses for Business Use of Your Home` is exactly ten. The fix is
  to tell the prompt the party outranks the form's full legal title, which moves
  the instruction the model follows most literally and wants its own measured run.
- **0e — a party cut to fit the filename length records nothing.**
  `SUBJECT_TRUNCATED` reports the ten-word subject cap, a different mechanism; it
  fired on only 2 of the 8 documents whose party was actually cut.
- **0f — character-level garbling is undetectable.**
  `Cross & Daughters Bakery` was named `Cross & Daubs`. No existing rule catches
  a mis-transcribed proper noun.

### Smaller things

- **`MAX_NAME_COLLISIONS` raised from 500 to 2,000**, and moved next to the
  filename length budget that must reserve room for its ` (2000)` suffix, since
  the two are one decision. A new test derives the reserve from the constant, so
  raising the cap again without widening the reserve fails rather than silently
  truncating.
- **A flagged document now logs why.** `flag()` wrote a manifest and a ledger row
  but no log line, so the app log — the first thing anyone reads — showed a batch
  with unexplained gaps.
- **`Pipeline::new` retries `sidecar.versions()` three times.** A cold convertd
  unpacking itself to `%TEMP%` could lose the very first handshake, which took
  the model-version stamp out of every manifest in the run.
- **The app log now records the two silent re-tries**: an OCR-confidence
  escalation and a span-mismatch re-prompt were both invisible in a run that
  looked merely slow.
- **`route` is persisted through an explicit `match`** rather than a `Debug`
  lowercasing that would have followed a renamed variant into the database.
- **A file that never stabilizes now logs its extension and last observed size.**
  It gets no ledger row, no manifest and no quarantine copy, so an operator
  reconciling 1,000 files against fewer manifests previously had a scrubbed path
  and nothing else.

## [0.4.1] — the things 0.4.0 measured and left broken

### Documents stopped being quarantined for obeying their own schema

**Fixed — the JSON schema capped `subject` and `description` at exactly the
checker's own limits, so llama.cpp's grammar stopped generation mid-word.**
`maxLength` is enforced by refusing to emit another character, so every proposal
came back at exactly the cap: subjects ending `"Cobalt Ridge Analyt,"` and
`"Taxpayer / "`, descriptions ending `"...supporting worksheets. The return was "`.
That trailing fragment then failed the "exactly one sentence" rule the model had
been told to satisfy. The document was quarantined for a limit the schema
imposed, and the manifest blamed the model.

`description` now has real headroom (200 to 320) so the model finishes its
sentence. `subject` went the other way, to 64: its constraint is a word count the
schema cannot express, and the model uses whatever room it is given — raising it
to 140 produced an over-long subject on **39 of 40** documents, every one then
trimmed. The cap had stopped being a constraint and become a target. 64
characters is about the checker's ten words.

The prompt now also states the two rules it never stated: that the description
must end in a single full stop and must not run to a second sentence, and that
the subject should name one document type and one party rather than a
comma-separated list.

**Added — deterministic, recorded repair for a mechanically-fixable answer.**
Rejecting a whole proposal over punctuation spends a person's attention on
nothing.

- A description that runs past one sentence, or that was cut off after one, is
  trimmed to its first complete sentence and flagged
  `DESCRIPTION_TRIMMED_TO_ONE_SENTENCE`. The trimmed result is re-validated like
  any other input, so a first sentence too short to stand alone is still a
  rejection, and no complete sentence at all still is too.
- A subject over ten words keeps its first ten — the form number and the party,
  which is what a filename is for — and is flagged `SUBJECT_TRUNCATED`, with any
  dangling separator removed. Too *few* words cannot be repaired by trimming and
  remains a rejection.

Both are trims, never additions: they can drop a tail, not invent content. Three
existing tests encoded the old reject-only contract and now assert the repair
instead; the guarantee they protect is unchanged — what ships is still at most
ten words and exactly one sentence.

**Fixed — a validation rejection was invisible outside the encrypted ledger.**
The code went only to `Ledger::log_event`, and the manifest carried the generic
`SLM_FAIL:no valid output after escalation`, so an operator looking at a third of
a backfill in Needs Review could not tell a subject problem from a date problem
without decrypting a database. The code now also reaches the app log, and the
flag reason names the rule that refused — `SLM_FAIL:no valid output after
escalation (BAD_SUBJECT)` — keeping the documented prefix that
`docs/TROUBLESHOOTING.md` lists and Flow 2 matches on. Only the code, never the
offending text: that is what `CheckError::code` exists for.

### Measured effect

Same eight genuinely-undated documents, across the three fixes:

| | named |
|---|---|
| before any of this | 0 of 3 |
| after the date fallback | 5 of 8 |
| after the schema and repair fixes | **8 of 8** |

And on a 40-document stratified sample spanning all five fixture shapes:
**40 of 40 named, 10.6 s/file** — against 34.3 s/file before. Throughput
improved because a rejection is expensive: each one burns three naming attempts,
so removing false rejections did far more for wall clock than pooling conversion
did. **1,000 files goes from roughly 9.5 hours to under 3.**

### Still open, and recorded rather than papered over

`docs/KNOWN_ISSUES.md` item 0b: a model that proposes today's date is validated
against the file's own mtime, because `pipeline.rs` puts filesystem timestamps in
the evidence list and `README.md` advertises file metadata as valid evidence. On
the 40-document run, 25 of 29 completed documents carried
`DATE_SOURCE_CORRECTED` and most were named with the day of the run. The rig
amplifies it — fixtures were copied in immediately before, so every mtime was
that day — but it is real. The fix is close to one line, and it narrows a
guarantee the README states in those words, so it is a product decision rather
than something to slip into a patch release.

### Pooling and the undated fallback (same release)

### Undated documents are named again

**Fixed — the mtime fallback fired only when the model volunteered `"none"`.**
`README.md` promises that undated documents fall back to the file modified date.
The branch existed and was tested, but reaching it required the model to decline;
on tax pages dense with years a 0.6B/1.7B model proposes a plausible date
instead, `DateNotInEvidence` correctly refuses it, the ladder re-asks, and the
document quarantines as `SLM_FAIL` — having never reached the fallback that
exists for precisely that case. Measured before: **0 of 3** genuinely undated
fixtures named.

`check_with` now converts a would-be `DateNotInEvidence` into the fallback when
the document itself carried no date. Three things made that safe rather than a
loophole:

- **It sits after the per-date evidence check, not before it.** An earlier
  version gated ahead of the tripwire and discarded dates that metadata
  genuinely supported; five existing tests caught it. That is the difference
  between "this date is unsupported" and "this document has no date."
- **It does not require `file_metadata_dates` to be empty.** That reads safer and
  is a no-op: `pipeline.rs` always extends that list with the file's own mtime
  and ctime, so it is never empty for a real file. Gating on it left the fallback
  as unreachable as before — measured at 6 of 18, and those six only because the
  model happened to guess the mtime. It is also circular: a filesystem timestamp
  cannot be the evidence that forbids falling back to the filesystem timestamp.
- **The central promise is untouched.** This path does not ship the model's date.
  It discards it and substitutes one with real provenance, recording both
  `DATE_FROM_FILE_MTIME` and `DATE_PROPOSAL_DISCARDED:<what was proposed>` — so
  the two meanings of `date_source: "metadata"` stay distinguishable in the index,
  and how often the model fabricates dates stays measurable. Where the document
  does contain dates, a mismatched proposal is still a hard rejection;
  `rejects_hallucinated_date` is unchanged and passing.

**Fixed — `harvest.dates` did not include every date the model was shown.**
`harvest::harvest` scans the first 6,000 and last 2,500 characters, but
`filter.rs` shows the model salient sentences drawn from the *whole* document and
Ettin spans from the first 8,000. A date outside the harvest window could
therefore appear in the bundle while the checker had no record of it. That was
already a latent way to reject a correct, evidenced answer; it became
load-bearing the moment an empty harvest started licensing the fallback, because
a document whose only date sat at character 7,000 would have looked dateless.
`build_evidence` now folds those lanes back into the harvest.

### Conversion is no longer serialized app-wide

**Fixed — `Sidecar` held one convertd process behind one mutex for an entire
request/response round trip**, so every conversion, OCR, probe and langid call in
the app queued behind every other one, and `Config::convert_workers` sized a
semaphore that bought queue depth and no parallelism at all. convertd's main loop
is `while True: readline()`, strictly one request per process, so the fix is more
processes rather than more requests down one pipe.

`Sidecar` now runs a pool. A condvar free-list rather than one mutex per slot: an
intermediate version scanned slots and then blocked on a rotating one, which let
a caller sit behind a long OCR while a different worker went idle — the fairness
loss it was meant to remove. Workers are handed out through an RAII `Checkout`,
so the several ways `call` can leave — success, four failures, or a panic in
serde or a caller — all return or retire the worker; without it any missed path
would have cost the pool a worker permanently and a long backfill would grind
down with nothing in the log to explain it.

Measured, warm pool, conversion stage alone: 13 documents in 3.2 s on one worker
and 2.4 s on four (1.33x). Not 4x because these fixtures are small text-layer
PDFs where the JSON round trip dominates; the gain grows with per-document work,
which is what scanned pages running escalating OCR passes are.

**`Config::convert_workers` is now capped by installed RAM** as well as by cores.
Before the pool the value cost nothing in memory however large it was, since one
process served everything. Now each worker is its own ~195 MB Python process, so
six of them is ~1.2 GB and does not fit on 8 GB beside Windows, the two model
servers and the app: <=9 GiB caps at 2, <=17 GiB at 4, above that 6.

### Honest about what this did not fix

Pooling conversion did not shorten the end-to-end batch, and `docs/SIZING.md`
says so with the numbers. With `slm_parallel: 1` the naming lane sets the wall
clock, so converted documents queue behind a single naming slot; a 12-file batch
measured 34.3 s/file before and 40.25 s/file after with four workers, the
difference being CPU the extra workers took from llama-server. Conversion
parallelism pays off only where `slm_parallel` can also rise, which is a function
of RAM. The benchmark is `#[ignore]`d and measures the conversion stage in
isolation for exactly that reason.

The remaining failures on undated fixtures are subject and description
rejections, not date ones — a naming-quality limit of a small model on sparse
"draft working notes" pages rather than an unreachable rule.

## [0.4.0] — sized for the machine it actually runs on

0.3.0 proved the release procedure worked. This release is what happened when
the product was pointed at its real workload — a thousand tax PDFs and Word
documents on an 8 GB laptop with no GPU — and measured instead of assumed. New
in `docs/SIZING.md`: every number below, how it was obtained, and how to
reproduce it.

### The defaults could not run on 8 GB

**Fixed — `slm_parallel` defaulted to 4 on every machine, which needs ~6 GB for
the language models alone.** `slm.rs` derives `--ctx-size` as
`4096 * slm_parallel` and llama.cpp preallocates the entire KV cache at startup.
Qwen3's attention shape (28 layers, 8 KV heads, head_dim 128, F16) costs
112 KiB/token, so **each parallel slot is 448 MiB** — and the weights are the
cheap half. Measured, both model servers resident:

| `slm_parallel` | Working set | Private commit |
|---|---|---|
| 4 (old default) | 6,078 MB | 3,904 MB |
| 1 (new 8 GB default) | 3,385 MB | 1,207 MB |

Both tiers *are* resident on any real batch: `SlmLane` keeps `primary` and
`escalation` in separate slots, and the 1.7B server stays up for the rest of the
run once any document reaches a third naming attempt. So 6 GB was the
steady state, before Windows, the app and convertd. On 8 GB that is not a slow
run, it is a thrashing one.

`default_slm_parallel()` now reads installed RAM (`GlobalMemoryStatusEx`,
declared inline rather than adding a system-info dependency): <=9 GiB gives 1,
<=17 GiB gives 2, above that 4, and unknown gives 2 rather than gambling on the
smaller machine. Lowering it costs no naming quality — per-slot context is 4,096
tokens either way, since the total is `4096 * n` shared across `n` slots.

**Fixed — the persisted config kept the unsafe value across upgrades.**
`backlog.config.json` outlives the installer, so an 8 GB machine that had ever
run an earlier build would keep `slm_parallel: 4` forever, having never chosen
it. `Config::load` now clamps to what RAM supports, one-directionally: a value
at or below the ceiling is left exactly as configured, because someone lowering
it knows something this does not. Overcommitment is corrected and logged, never
silently.

**Fixed — the naming HTTP timeout and the wall-clock budget disagreed, and the
tighter one silently won.** `slm.rs` hardcoded a 60-second client timeout while
`pipeline.rs`'s `wall_clock_cap` budgets `per_file_wall_clock_secs` (90) for the
same request. On a workstation naming takes seconds and this never surfaced; on
the CPU-only laptops this ships to it turns a slow-but-succeeding document into
`SLM_FAIL:no valid output after escalation` — blaming the model for a deadline
the HTTP client imposed. Now 120s, with the coupling to the config value stated
at the constant.

### One download, good to go

**Added — the installer carries the primary model.** A fresh machine can name
its first document without the 2.4 GB in-app fetch. On first launch the bundled
GGUF is *relocated* into the app-data models folder rather than pointed at in
place: per-user installs share a volume with app-data, so the move is instant
and free, and keeping one canonical models dir is what stops a later "Download
models" from writing back into the install tree and being orphaned by the next
upgrade.

Both Q8_0 weights together are 2.4 GB and GitHub caps a release asset at 2 GiB,
so the 1.7B ships as a separate optional asset. That is a real quality
trade-off, not a packaging detail — see the table below. The official Qwen
repositories publish only Q8_0, so a smaller quantisation would mean
third-party weights and would break the provenance `NOTICE.md` documents.

**Added — the escalation tier degrades instead of failing when the 1.7B is
absent.** `Config::normalize` points both tiers at the primary when the
escalation GGUF is missing, and `SlmLane::ensure_up` then reuses the running
server instead of standing a second one up over identical weights. Without this,
a missing optional model was not a degraded mode but a cliff: `spawn_server`
refuses a GGUF that is not a file, so every third naming attempt failed outright.

### Measured behaviour

`pipeline.rs`'s `e2e_real_batch` is a new `#[ignore]`d load harness that drives
the real sidecars and real weights against real folders, parameterised by
environment variables. It is not one of the five gates and never runs in
`cargo test`. Everything else in the suite exercises the orchestrator against
stubs, so nothing could previously answer what a batch costs.

Measured on a mixed synthetic tax corpus, `slm_parallel: 1`:

| Tiers | Per file | Named `ok` |
|---|---|---|
| 0.6B only | 23.9 s | 2/12 (17%) |
| 0.6B + 1.7B | 34.3 s | 7/12 (58%) |

Extrapolated to 1,000 files: **6.6–9.5 hours**. The bottleneck is not the naming
lane — `Sidecar::call` holds one mutex for a whole request/response round trip,
so conversion is one-at-a-time app-wide and `convert_workers` buys queue depth
rather than parallelism. That is recorded in `docs/SIZING.md` as a known
characteristic, not fixed here.

Roughly a third to a half of a real tax batch lands in Needs Review. Much of
that is correct — an ambiguous `04/05/2023` must go to a human, and `checker.rs`
refuses any date it cannot prove against the document text. `docs/SIZING.md`
separates the genuine misses from the designed refusals rather than reporting a
single success rate.

## [0.3.0] — the release procedure, actually executed end to end

Everything below the "Build and release" heading was found by running
`RELEASING.md` from a bare clone on a clean Windows 11 box rather than by
reading it. Four of the five defects were in the build and packaging path, not
in the app: the shipped Rust, TypeScript and Python code needed no correction,
and every gate that could be run passed on the first honest attempt once the
scripts themselves would run.

### Build and release

**Fixed — `sidecar/requirements.lock` could not be installed on Windows at
all.** It pinned `magika==0.6.3` alongside `onnxruntime==1.28.0`, but magika
0.6.3's own metadata caps `onnxruntime<=1.20.1` on `win32`. The lock had been
resolved on Linux, where that marker does not apply, and committed as "the
reproducible lock" for a product that only ships on Windows. Any resolving
installer rejects it outright. Repinned to `magika==0.6.2`, resolved on
Windows/Python 3.11, which is the only difference between the committed lock
and a fresh Windows resolution.

**Fixed — `scripts/build-sidecar.ps1` shipped a sidecar with no document
parsers in it when the dependency install failed.** `$ErrorActionPreference =
"Stop"` does not apply to native commands, so the `uv pip install` failure
above was ignored: the run continued with only PyInstaller installed, logged
twelve `--collect-all ... is not a package` warnings, and produced a
`convertd.exe` containing none of MarkItDown, RapidOCR, ONNX Runtime or
pdfminer. Every install step now checks `$LASTEXITCODE`, matching what the
script already did for PyInstaller itself. The fixture smoke test did catch
this build — but a build must not rely on a later gate to notice its
dependencies were never installed.

**Fixed — the sidecar smoke test blamed the binary for its own encoding.**
PowerShell encodes a native command's stdin with `[Console]::InputEncoding`.
Where that is UTF-8 *with* a preamble, a 3-byte BOM was glued to the first
request, convertd correctly answered `JSONDecodeError: Unexpected UTF-8 BOM`
with `"id": null`, and the gate reported `no response for request 1` — pointing
at the sidecar rather than at the harness. The encoding is now pinned for the
duration of the test and restored afterwards. The shipped Rust client was never
affected: `sidecar.rs` writes `serde_json::to_string` bytes, which carry no BOM.

**Fixed — `scripts/verify-binaries.ps1` could not run on Windows PowerShell.**
`$BinDir` defaulted to `Join-Path $PSScriptRoot ...` inside the `param()`
block; 5.1 binds parameters before `$PSScriptRoot` is populated, so the gate
died on its own first line with `Cannot bind argument to parameter 'Path'`.
Resolved in the body instead, `-BinDir` still overridable. Both this and the
BOM defect were invisible because every script is documented as `pwsh`, and
pwsh 7 happens to paper over both.

**Documented — `npm run tauri build` hangs after bundling, waiting for a
password this key does not have.** The CLI prints `Decrypting updater signing
key, expect a prompt for password` and blocks on stdin even for an
empty-password key, so in any shell without a console the build stops with the
installer written and no `.sig` next to it. `$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""`
is not a workaround — PowerShell deletes a variable assigned the empty string.
`RELEASING.md` now says so and gives the separate `signer sign` step, which is
how 0.3.0 was signed.

**Fixed — two `RELEASING.md` commands that cannot work as written.** The
signing-key command uses `-p ""`, and PowerShell drops an empty-string argument
before it reaches the executable, so the CLI exits 2 demanding the value that
was just supplied; it is now marked as the one command in that document to run
from a POSIX shell. Build step 4 also told you to pin `llama-server.exe` to the
SHA-256 recorded in Build step 2, which is the hash of the *zip* and can never
match the extracted binary.

### Gates that had never run on Windows

`scripts/ci-local.sh` runs the five jobs on Linux, and
`.github/workflows/ci.yml` has never been assigned a runner
(`docs/KNOWN_ISSUES.md` item 11). Two of those jobs turn out to have been
failing the whole time on the only platform this product ships on. Both are now
fixed and both gates pass on Windows.

**Fixed — `cargo test --workspace` could not start on Windows at all.** The test
harness binary died with `STATUS_ENTRYPOINT_NOT_FOUND` (0xC0000139) before
reaching `main`, which cargo surfaces as `test exited abnormally` with no test
having run. Cause: the harness links tao/wry and so inherits their static
imports of `TaskDialogIndirect`, `RemoveWindowSubclass` and `DefSubclassProc`
— ComCtl32 **v6** exports. `System32\comctl32.dll` is v5.82 and exports only
`SetWindowSubclass`; v6 is reachable only via the side-by-side
`Microsoft.Windows.Common-Controls` assembly, which needs an application
manifest. `tauri_build::build()` embeds one through `rustc-link-arg-bins` —
bins only — so the harness had no `.rsrc` section whatsoever. `build.rs` now
declares that dependency with `cargo:rustc-link-arg`. The scoped
`rustc-link-arg-tests` would have been the narrower tool but covers only
integration tests under `tests/`, which this crate has none of; cargo rejects
it with "does not have a test target", and the harness at issue is built from
the *lib* target, which has no scoped instruction of its own. This never
affected the shipped `BackLog.exe`, which is a bin and always had its manifest;
and `cargo test -p backlog-core` passed throughout because the trust core links
none of this, which is precisely the property it was separated out for.

**Fixed — the log scrubber could be defeated by a path arriving in fragments,
which on Windows was every path it was meant to redact.** `SharedSink::write`
scrubbed each `write` call as it came, justified by a comment asserting that
"env_logger formats a whole record and hands it over in one call, so scrubbing
here … cannot be defeated by a sensitive path straddling two writes". That
holds for env_logger and for nothing else. `fmt::write` calls back once per
format fragment, and `std`'s `Debug for OsStr` escapes its input with
`f.write_char(c)` — one character per call — so a `{path:?}` argument reaches
the sink as about a hundred single-byte writes, and no root can be matched
inside a one-byte haystack. `SharedSink` now reassembles complete lines before
scrubbing, so the guarantee holds whatever the caller's write granularity is
instead of resting on an undocumented detail of one dependency's
`Target::Pipe`. The same buffering fixes a second defect in the same line:
`String::from_utf8_lossy` over one-byte slices turned every byte of a
multi-byte character into U+FFFD, so non-ASCII filenames were being mangled on
the way into the log.

The two `logging` tests that assert this — which is to say, the tests that
assert the claim `docs/PRIVACY.md` makes about the log file — were correct and
failing; nothing was wrong with them. A regression test now pins the
fragmentation itself, since it originates in `std` and would not survive being
rediscovered by accident.

Reachability, stated plainly: in a shipped build every line goes through
env_logger, which does hand over whole records, so no released version is known
to have written a document path to disk. The defect was that the product's
central privacy promise held by coincidence rather than by construction, and
any second writer into that sink would have silently turned the log into the
plaintext index of HR filenames the encrypted ledger exists to prevent.

**Fixed — `cargo clippy --workspace --all-targets -- -D warnings` failed on
Windows.** `pipeline.rs`'s `tiny_cap` and `Harness::dir` are reachable only
from `#[cfg(unix)]` tests (there are five of those and no `cfg(windows)`
counterpart), so on Windows both are dead code and `-D warnings` rejects the
build. `tiny_cap` is now `#[cfg(unix)]` to match its only callers. `Harness::dir`
is `#[allow(dead_code)]` instead, because it is not really unused: it is a
`TempDir` whose drop deletes the very tree the pipeline is pointed at, so the
field is load-bearing on every platform even where nothing reads it.

**Changed — the updater signing key was rotated.** The keypair that signed
0.1.0 and 0.2.0 was not on the build machine and is not recoverable, so 0.3.0
is signed by a new one and `plugins.updater.pubkey` was replaced to match.
Installed 0.1.0 and 0.2.0 copies verify against the old pubkey baked into them
and will therefore reject 0.3.0 silently, by design; they need one manual
install. From 0.3.0 forward the chain is intact. See `RELEASING.md`.

### Added
- `.github/workflows/ci.yml`: five Linux jobs (trust core, workspace, frontend
  + UI harness, Python 3.11, release version agreement). The project previously
  had no CI at all, and `cargo fmt --all -- --check` — an item
  `docs/RELEASE_CHECKLIST.md` calls mandatory — had never been executed.
- `rust-toolchain.toml` pinning the channel with `rustfmt` and `clippy`.
- `LICENSE` (proprietary, source-available) and `NOTICE.md` enumerating every
  redistributed component, so the redistribution gate in
  `docs/DEPENDENCY_COMPATIBILITY.md` can be closed.
- `docs/USER_GUIDE.md`, `docs/TROUBLESHOOTING.md` and `docs/PRIVACY.md` — the
  first documents in the repo written for the office worker who runs this
  appliance rather than for a builder or auditor.
- `docs/KNOWN_ISSUES.md` and `docs/DECISIONS.md`, replacing
  `PRODUCTION_READINESS.md`.
- `scripts/verify-binaries.ps1`: refuses to package a release whose sidecars
  are dev stubs, truncated builds, or not valid PE images.
- `training/README.md` and `training/requirements.txt`.
- `bundle.windows.webviewInstallMode: offlineInstaller` and
  `bundle.windows.nsis.installMode: currentUser` in `tauri.conf.json`.

### Changed
- `scripts/dev-stubs.{sh,ps1}` write a `BACKLOG-DEV-STUB-DO-NOT-SHIP` marker
  instead of zero bytes, so a stub is provably a stub to
  `scripts/verify-binaries.ps1`, which is the gate between a dev checkout and a
  bundle. The *runtime* readiness check has not caught up: `preflight.rs`'s
  `binary_exists` still only requires a non-empty file, so it certifies a marked
  29-byte stub as installed — see `docs/KNOWN_ISSUES.md` item 9.
- `power-automate/manifest.schema.json`,
  `power-automate/manifest.parse-json.schema.json` and the example fixtures
  move to manifest **v3** (`dismissed` status; `model_versions` required
  non-empty on `ok`), matching `src-tauri/src/manifest.rs`.
- `power-automate/FLOW1-intake.md` rewritten: Flow 1 now delivers each file
  under its plain original name inside a per-delivery subfolder it composes
  itself. The previously documented `__incoming_<flow-id>-<item-id>__` envelope
  described app behavior that does not exist.

### Fixed
- README behavior guarantees, prerequisites and setup steps that did not match
  the code (`--vl`, "Python 3.11+", the llama-server version rationale,
  "indexed once", "never deleted").

## [0.2.0] — backend hardening + licensing-clean model swap

### Added
- **Encryption at rest for the ledger.** `ledger.db` is whole-file encrypted
  with SQLCipher (`rusqlite`'s `bundled-sqlcipher-vendored-openssl`). The
  256-bit key is generated on first open, DPAPI-protected, and stored at
  `<data_dir>/ledger.key` — never written in plaintext (`src-tauri/src/dbkey.rs`).
- **In-app model downloader** (`src-tauri/src/model_download.rs` +
  `download_models` command): resumable, cancellable, SHA-256-verified against
  `models.lock.json`, so a non-technical user never opens a terminal to install
  the two Qwen3 GGUFs.
- **Runtime preflight** (`src-tauri/src/preflight.rs`): every check carries a
  plain-language `message`, a technical `detail`, and — where the app can fix
  it — an `action` the UI renders as a button.
- **Signed auto-updater**: `tauri-plugin-updater` with a minisign pubkey in
  `tauri.conf.json`, checked once at startup by `src/main.ts`.
- **System tray + close-to-hide**: closing the window no longer quits the
  process and kills the sidecars mid-batch.
- `JobState::Dismissed` and the `dismiss` command: a terminal human decision,
  distinct from `Emitted` so throughput does not count it as work delivered.
- Diagnostics (`get_diagnostics`), structured logging with path redaction
  (`src-tauri/src/logging.rs`), and ledger read APIs behind the review loop.
- `backlog-core` crate: `harvest` + `checker` extracted with no Tauri, sidecar
  or icon dependency, so the trust core tests on a bare checkout — which is
  what makes Linux CI possible at all.

### Changed
- **Licensing-clean model swap**: Qwen3-0.6B/1.7B, Lingua and RapidOCR 3
  replace LFM2.5, fastText `lid.176` and `rapidocr-onnxruntime`.
- **Slim, torch-free sidecar**: `torch`, `transformers`,
  `sentence-transformers` and `gliclass` removed (~3x smaller Python footprint).
  `classify`, `salience` and `ettin_spans` degrade to deterministic `ok=true`
  fallbacks with `available: false`, so no document is ever flagged over a
  missing naming enhancement.
- Manifest schema **v2 → v3**: adds the `dismissed` status and requires a
  non-empty `model_versions` on an `ok` manifest.
- Model paths are rehomed at startup to `%APPDATA%\ai.sonomos.backlog\models`;
  a path set through Settings' Browse dialog passes through untouched.
- Retry ladder rungs now vary the *evidence bundle*, not just the model tier —
  rung 3 used to be byte-identical to rung 1.

### Fixed
- **Duplicate path.** The duplicate manifest id was `{sha}:{uuid}`: `:` is
  invalid on NTFS so the write silently failed on Windows, and the fresh UUID
  made replay non-idempotent. Now a deterministic filesystem-safe per-copy key,
  with a durable ledger row per physical copy so `(2)`/`(3)` names increment.
- **Sidecar could wedge the pipeline.** stdout is drained on a reader thread
  with an enforced per-request deadline (kill + respawn on timeout), and `Drop`
  guarantees no orphaned sidecar processes.
- **Crash-loop guard**: a durable attempt counter quarantines a poison-pill
  document as `CRASH_LOOP` after 5 restarts.
- **Watcher no longer skips leading-underscore filenames.** `_DRAFT
  Agreement.docx` was silently dropped — no ledger row, no manifest, no log.
- Unicode crash in `harvest()`; `tail_pages=0` trim no-op; pdfium bitmap leak;
  `get_evidence` path traversal; PII in the persisted `events` table; unbounded
  header read during type detection.
- Removed `tauri-plugin-shell` and `tauri-plugin-opener` and their capability
  grants — all dead code, and pure IPC attack surface.
