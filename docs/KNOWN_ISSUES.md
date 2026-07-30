# Known issues

What is genuinely open, as of the current tree. This file replaces the "What
still needs attention" section of the retired `PRODUCTION_READINESS.md`, whose
list had drifted so far from the code that it claimed "No auto-updater" about an
app that ships a signed one.

**Rule for this file: an item leaves it only when the code changes, not when a
document says it did.** Every entry below was re-checked against the tree, not
carried forward.

Release-blocking gates that are simply *work to do on a Windows machine* live in
`docs/RELEASE_CHECKLIST.md`, not here.

---

## Open

### 0. ~~The undated-document fallback depends on the model volunteering "none"~~ — fixed in 0.4.1
`README.md`'s behavior guarantees say "**Undated documents fall back to the file
modified date** with `date_source: metadata` and a `DATE_FROM_FILE_MTIME` note".
That is implemented and tested — but the trigger is narrower than the sentence
suggests. `checker.rs` substitutes the mtime only when the model returns
`date: "none"` *and* `date_source: "none"`
(`checker::tests::undated_falls_back_to_metadata`). Nothing reaches the fallback
if the model proposes a date instead.

On genuinely undated tax paperwork it usually proposes one. The pages are dense
with years ("Tax Year 2022", "Year Acquired"), so a 0.6B/1.7B model offers a
plausible date rather than declining; `DateNotInEvidence` then correctly refuses
it, the retry ladder re-asks, and the document ends as
`SLM_FAIL:no valid output after escalation` in quarantine — never having reached
the fallback that exists for exactly this case.

Measured on a synthetic corpus whose undated fixtures contain **no date-shaped
text at all**: **0 of 3 undated documents were named**; all three were flagged.
By contrast the fallback does fire on documents that do carry a date the model
missed, which is how it has been seen working.

The refusal is not wrong — the checker is doing its job, and quarantine plus a
`NeedsReview` row is the safe outcome. What is wrong is the documented promise,
which reads as a property of undated documents when it is really a property of
models that decline to guess.

**Fixed in 0.4.1**, by the first of those two routes. `check_with` now converts
a would-be `DateNotInEvidence` into the mtime fallback when the document itself
carried no date — `harvest.dates` empty, which after `filter.rs` folds the
salience and Ettin lanes into it covers every date the model was shown. It is
placed *after* the per-date evidence check, not before: an earlier version
gated ahead of the tripwire and discarded dates that metadata genuinely
supported, which five existing tests caught.

The condition deliberately does **not** also require `file_metadata_dates` to be
empty. That sounds safer and is a no-op — `pipeline.rs` always extends that list
with the file's own mtime and ctime, so it is never empty for a real file, and
gating on it left the fallback as unreachable as before (measured: 6 of 18
undated documents named, and those six only because the model happened to guess
the mtime). It is circular besides: a filesystem timestamp cannot be the
evidence that forbids falling back to the filesystem timestamp.

The central promise is intact. This path does not ship the model's date — it
discards it and substitutes one with real provenance, recording both
`DATE_FROM_FILE_MTIME` and `DATE_PROPOSAL_DISCARDED:<what the model said>`, so
the two meanings of `date_source: "metadata"` stay distinguishable in the index
and model fabrication stays measurable. Where the document does contain dates, a
mismatched proposal is still a hard rejection
(`checker::tests::rejects_hallucinated_date`, unchanged and passing).

**The refinement this originally deferred landed in 0.4.2.** `pipeline.rs` no
longer merges filesystem timestamps into the evidence list, so `checker.rs` sees
only the document's embedded properties and the two are no longer conflated — and
it needed no signature change, only removing the merge. The paragraph that used
to sit here estimated a change across fifteen call sites; the actual fix was one
line. See item 0b.

### 0b. ~~The file's own mtime validates a model that proposes today's date~~ — fixed in 0.4.2
**Fixed, and the diagnosis below was only half right.** Filesystem timestamps no
longer reach the evidence list: `pipeline.rs` passes the document's *embedded*
metadata and leaves the mtime to the fallback, which already received it
separately. `README.md`'s guarantee now says "embedded metadata" and explains why
a timestamp cannot be the evidence for itself.

That alone changed almost nothing, which is the interesting part. The synthetic
corpus's fixtures were generated the same day, so their *embedded* `created`
property also said today — a model proposing today matched genuine embedded
evidence, exactly as the guarantee allows. The original attribution of "25 of 29
documents named with the run date" to filesystem timestamps was wrong; embedded
metadata was the larger cause.

The fix that actually moved the number was ordering, not evidence: **a date
printed on the page now outranks the document's embedded properties**
(`Checker::date_printed_on_the_page`). Handed both, the model usually took the
property, and several proposals claimed `date_source: "document"` while naming a
date that appears nowhere in the text. The substituted date is regex evidence read
from the document, so its provenance is strictly better than what it replaces, and
the displaced proposal is recorded as `DATE_PREFERRED_FROM_DOCUMENT:<value>`. Only
unambiguous head-region dates qualify — a date deep in the body is more likely a
reference to another document, which is what `DATE_FROM_BODY` has always warned
about.

Measured on the same 40 documents: documents named with the run date fell from
**37 of 40 to 16 of 40**, and documents with a date on page one went from **2 of
16 to 16 of 16**. The remaining sixteen are ten fixtures that carry no date at
all, where the mtime is the correct answer, plus six whose date sits past the
harvest window — see item 0c. Those six are the only genuine misses left on this
sample.

### 0c. The harvest cannot see a date in the middle of a long document
`harvest::harvest` scans the first 6,000 and last 2,500 characters. A date on page
three of a nine-page document falls in neither, so nothing downstream can prefer
it: measured, only 2 of 8 documents whose date sits deep in the body were named
from it, against 16 of 16 when the date is on page one.

The window, not the page number, is what decides it — which is why this is worth
fixing rather than accepting. Against the corpus generator's ground truth, both
`dated_deep` documents that succeeded have their date on page 3 or earlier, and
all six that failed have it on pages 4 to 7 of 6- to 12-page files. The single
apparent exception (a failure whose date is on page 3) is in a 9-page file whose
earlier pages are long enough to push it past 6,000 characters. These six are the
only genuine date misses in the whole 40-document sample.

The salience lane does read the whole document and `filter.rs` folds any dates it
finds back into the harvest, but those arrive with no usable position — an offset
from `extract_dates` is relative to the sentence it was given, not the document —
so they are recorded as position-unknown and deliberately excluded from the
head-region preference. They still count as evidence; they just cannot win the
letterhead tie-break.

**Fix:** give the folded dates a real document offset, by locating the salient
sentence in the markdown before extracting from it. That makes a mid-document date
orderable and lets the same preference reach it. Not done here because it is a
change to how evidence positions are computed, and it wants its own measured run
rather than being bundled into a release already carrying three date-handling
changes.

### 0d. Three in ten filenames do not name the party at all
Measured on the 40-document sample, scoring each new filename against the
document's own `Taxpayer / Entity:` line: 18 name the party exactly, 8 name a
correct prefix cut short, 2 garble it, and **12 do not contain it at all**.

This is a budget problem, not a reading problem. `SUBJECT_MAX_WORDS` is ten, and
`Form 8829 - Expenses for Business Use of Your Home` is exactly ten words, so a
model that leads with the form's full legal title has nothing left for the party.
The date is right and the document type is right; the filename simply does not
say whose it is, which is half of why anyone renames a file.

**Fix:** state in the naming prompt that the party outranks the form's full
title — `Form 8829 - Marcus Alvarez` over
`Form 8829 - Expenses for Business Use of Your Home`. Cheap to try, but it moves
the one instruction the model follows most literally, so it needs its own
measured 40-document run rather than being folded into a release whose changes
were all in the checker. Not attempted here.

### 0e. A party cut short by filename length is recorded nowhere
`SUBJECT_TRUNCATED` reports the ten-word subject cap. It is **not** the flag for
a name cut by `compose`'s `max_filename_len` trim, which is a separate mechanism
and currently records nothing at all — measured, it fired on only 2 of the 8
documents whose party was actually cut in the filename, and on 2 of the 18 whose
party was complete.

Neither is wrong about what it claims, but the effect is that a user looking at
`... - Ironwood & Vance.pdf` has no indication that `Roofing` was dropped, and no
way to tell that filename from one where the party genuinely is `Ironwood &
Vance`. A `SUBJECT_TRIMMED_TO_FILENAME_LENGTH` note at the point of the trim
would close it. Small and self-contained; not done here only because it belongs
with 0d.

### 0f. Character-level garbling of a party name is undetectable
Two of the 40 documents had their party altered rather than shortened:
`Cross & Daughters Bakery` was named `Cross & Daubs`, and `Whitmore &
Associates` became `Whitmore &Associes`. A third is borderline — `Derrick Pena`
became `Derrick Pén`, acquiring an acute accent that appears nowhere in the
source.

Neither carries a soft flag, and none of the checker's rules can catch this:
`SUBJECT_UNGROUNDED` tests whether the subject is a phrase from the document, but
these subjects mostly *are*, and the check is not per-token. A 0.6B model
mis-transcribing a proper noun is a known limit of the model tier rather than a
logic error, so the honest options are a substring check of the party against the
document text, or accepting it and documenting the rate. Recorded, not fixed.

### 1. The shipped sidecars are not built by anything reproducible
`src-tauri/binaries/` is gitignored and empty on a fresh clone. A release
depends on a human running `scripts/build-sidecar.ps1` and staging
`llama-server` by hand on one specific Windows machine.

`scripts/verify-binaries.ps1` now refuses to package dev stubs, truncated files
and non-PE images, and `scripts/dev-stubs.*` mark their output — so the
"shipped a placeholder" failure is caught at the packaging gate (the *runtime*
readiness gate still is not; see item 9). Reproducibility is not solved: two
builds of `convertd` from the same lock are not byte-identical.

### 2. `sidecar/requirements.lock` is version-pinned but not hash-pinned
57 exact versions, no `--generate-hashes`. A compromised or yanked-and-replaced
PyPI artifact at those versions would be installed without complaint.
`docs/RELEASE_CHECKLIST.md` calls a hash-pinned lock mandatory for a *signed*
release; today no release can honestly tick it.

**Fix:** `pip-compile --generate-hashes` in a clean 64-bit Python 3.11 venv on
Windows, commit the result.

### 3. Dev-only npm advisory (esbuild via vite 5)
`npm audit` reports an esbuild advisory reachable only through the Vite dev
server. It does not affect the shipped Tauri bundle, which serves a static
build. The fix is a Vite major bump (`vite@8`), deferred to avoid destabilising
a verified build shortly before a pilot.

### 4. No code-signing certificate
The installer and the external executables are unsigned. Every user therefore
sees a SmartScreen warning on install (documented in `docs/USER_GUIDE.md`) and
some managed fleets will block it outright. The updater's *own* minisign
signature chain is separate and does work — an update that fails signature
verification is rejected client-side.

### 5. Retention policy for the ledger `events` table
The ledger is encrypted at rest and the events it stores are value-free codes,
not document text. But nothing prunes them, so a multi-thousand-file backfill
leaves a permanent per-file trail. That is an asset for an audit and a liability
for a retention policy, and no one has chosen which.

### 6. Manifests are unencrypted on disk between emission and pickup
By necessity — Power Automate has to read them. They carry the proposed
filename and the one-sentence description, never document text, and Flow 2
deletes each one after committing. Worth stating explicitly in a security
review rather than discovering.

### 7. The Ettin span lane is inert in every shipped build
`training/` can produce a fine-tuned span model, and Settings has a field to
point at one, but the shipped slim sidecar has no `transformers`, so
`op_ettin_spans` always returns `{"spans": []}` with `available: false`. Setting
the field does nothing and reports no error. `training/README.md` says so up
front now; the honest fix is either a torch-inclusive sidecar profile or
removing the Settings field.

### 8. Screenshots are not committed, so a doc cannot embed one
`npm run harness:shots` renders every screen from the real frontend, in both
themes, and exits non-zero on any console error — but it writes into
`dist-harness/`, which is gitignored. `docs/USER_GUIDE.md` therefore describes
the screens in words and contains no images. It also contains no instruction to
run the harness: its second sentence promises the reader will never need a
terminal, and a guide that breaks that promise on the one reader least able to
notice is worse than a guide with no pictures. Anyone who *can* run it does so
from here or from the Tests block in `README.md`:

```
npm run harness:shots     # → dist-harness/shots/<scenario>.<theme>[.full].png
```

Committing a screenshot set would make the guide better and would need a policy
for keeping it current; nobody has chosen one.

### 9. The readiness panel still accepts a marked dev stub as an installed sidecar
`scripts/dev-stubs.{sh,ps1}` used to write zero bytes; they now write the 29-byte
`BACKLOG-DEV-STUB-DO-NOT-SHIP` marker, which is what makes a stub *provably* a
stub for `scripts/verify-binaries.ps1`. But `binary_exists` in
`src-tauri/src/preflight.rs:593` is still `m.is_file() && m.len() > 0`, so a
marked stub is 29 bytes of ASCII and passes. Settings → Readiness will report
"Document reader (convertd) is installed — Ready" for it; the failure then shows
up one row down as "Document reader answers — Blocked", which points at the
wrong thing.

This only reaches a user through the same accident `verify-binaries.ps1` exists
to stop (a stubbed checkout packaged as a release), and that gate is the one
that actually blocks shipping. It is recorded here because the *readiness* half
of the fix has not landed: `binary_exists` should read the first 28 bytes and
reject the marker, and under `#[cfg(windows)]` require the `MZ` magic, which
subsumes it. `preflight.rs`'s
`binary_exists_rejects_zero_length_stubs_and_bare_names` needs the marker case
added when it does.

### 10. A dismissal cannot be undone from inside the app
`Can't fix this` is behind a `Set aside for good?` confirmation, and that
wording is honest — but only because nothing can reverse it, not because a
reversal was considered and rejected. `transition_allowed` in
`src-tauri/src/ledger.rs:136` freezes `(Dismissed, _) => false`, and the one
statement that can reopen a job, `Ledger::reset_for_reprocess`, is reachable
only through the `reprocess` command, which is wired only to the Needs Review
card's **Try again** button. `src/main.ts`'s `applyJobUpdate` removes that card
the moment the state leaves `flagged`, `list_flagged` never returns dismissed
jobs, and `fillQueueRow` gives queue rows no buttons at all — so once the
confirmation lands, the affordance is gone.

Re-delivery is not a workaround either. `Pipeline::ingest_one` returns silently
for a resolved job at the same normalized relpath, and at a *different* relpath
emits a duplicate manifest — a stray `… (2)` row in DocumentIndex rather than a
corrected one.

The document itself is never at risk: it stays in Quarantine untouched, so the
recovery is manual filing. `docs/TROUBLESHOOTING.md` and `docs/USER_GUIDE.md`
now say exactly this rather than promising reversibility.

**Fix:** either a `Dismissed` filter in the queue whose rows carry a **Try
again** that calls `reset_for_reprocess`, or an `undismiss` command with the
matching `(Dismissed, Ingested)` transition allowed. Both need a decision about
what Flow 2 does with the already-indexed dismissed row.

### 11. GitHub Actions has never executed; `scripts/ci-local.sh` is the enforcing copy
`.github/workflows/ci.yml` is committed, is syntactically valid and is
triggered on every push — and has never run a single step. Every run in the
workflow's history is `conclusion: failure` within 2–17 seconds of creation,
every job in every run reports `runner_id: 0` with an empty `runner_name`, and
the log download for any of them is an HTTP 404 because no log was ever
produced. That is the signature of a run that is created and then never
assigned a runner. `zgbrenner/backlog` is a **private** repository, so it draws
on the account's free Actions allowance rather than the unmetered public pool,
and the allowance is spent.

Consequence, and the reason this is an item rather than a footnote: **"CI is
green" is not a fact anyone can obtain about this repository today.** The gates
in `ci.yml` are enforced only by `scripts/ci-local.sh`, which runs the same five
jobs on a developer's machine. `.github/scripts/check-ci-parity.mjs` exists to
keep the two in lockstep, so that the day Actions is switched on the workflow
does not fail on drift accumulated while nobody could see it.

`docs/RELEASE_CHECKLIST.md` therefore gates on `./scripts/ci-local.sh` and marks
its CI-green box explicitly unsatisfiable, in the same way as the hash-pinned
lock in item 2.

**Fix:** enable billing for Actions on the account, or make the repository
public. Either resolves it without a code change; until one of them happens,
the honest statement is the one above.

## Closed since `PRODUCTION_READINESS.md` was written

Recorded because that document listed them as open and they are not, which is
how it lost the reader's trust.

| Was listed as open | Actually |
|---|---|
| "No auto-updater" | Ships one. `tauri.conf.json` declares `plugins.updater` with a minisign pubkey, `lib.rs` initialises the plugin, `src/main.ts` calls `check()` at startup, and `RELEASING.md` is an entire signed-release procedure. |
| "Encryption at rest" | Done. SQLCipher whole-database encryption, key DPAPI-protected at `<data_dir>/ledger.key`, proven by `ledger::tests::ledger_db_is_encrypted_at_rest_and_key_persists`. |
| "Async hygiene — blocking calls from async code" | Done. Convert/OCR/hash round-trips run under `tokio::task::spawn_blocking`; a `JoinError` folds into the same retry-then-flag path as any other failure. |
| "In-flight claim" | Done. `Pipeline::inflight` is an in-memory `HashSet<PathBuf>` on top of the durable ledger claim, so a path enqueued by both the startup sweep and a filesystem event is driven once. |
| "`binary()` PATH fallback" | Done. `resolve_binary` takes `cfg!(debug_assertions)`, so a release build never resolves a sidecar from `%PATH%`. |
| "Real app icon" | Done. A 1024×1024 source and the generated platform set are committed. |
| "No committed lockfiles" | Done. `Cargo.lock`, `package-lock.json` and `sidecar/requirements.lock` are all committed (see item 2 for the remaining gap). |
