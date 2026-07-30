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

### 0c. The harvest cannot see a date in the middle of a long document — much relieved in 0.4.3, not fixed
`harvest::harvest` scans the first 6,000 and last 2,500 characters. A date on page
three of a nine-page document falls in neither, so nothing downstream can prefer
it: measured on 0.4.2, only 2 of 8 documents whose date sits deep in the body were
named from it, against 16 of 16 when the date is on page one.

**0.4.3 took that to 4 of 8 without touching the harvest at all**, which is worth
recording because it was not predicted. The subject fix in item 0d changed only the
schema cap and the naming prompt, yet `dated_deep` doubled and
`DATE_PREFERRED_FROM_DOCUMENT` fell from 21 to 9. The most likely reading is that
the evidence bundle already contained the date and the model was the bottleneck, not
the window — one no longer spending its output budget on a severed form title picks
the right date out of what it was already shown.

**How much of this ceiling is really the window is now an open question.** The
intermediate four-bullet prompt described in 0d reached **6 of 8** on the same
fixtures at twice the wall clock. If the window were the binding constraint, no
prompt could have got there, because the date is not in the bundle to be found. So
some of these documents do have their date reachable and the model is simply missing
it, and the split between "outside the window" and "inside but overlooked" is not
established. Anyone attempting the fix below should measure that split first: it
decides whether widening the harvest is worth anything at all.

The paragraph that follows was written against the 0.4.2 run, when all six failures
were outside the window. It is kept because the page positions are still facts about
the fixtures, but the six is now four.

Against the corpus generator's ground truth, both
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

### 0d. ~~Three in ten filenames do not name the party at all~~ — fixed in 0.4.3
**Fixed, and the diagnosis recorded here first was wrong in an instructive way.**
The original entry called this "a budget problem, not a reading problem" — the
subject capped at ten words, and the model spending them on the form's full legal
title. That was half of it. The other half, which the entry attributed to
`compose` trimming to `max_filename_len` (see the old 0e), was really the **JSON
schema's `maxLength: 64` on `subject`**.

llama.cpp enforces `maxLength` by refusing to emit another character, so the cap
did not shorten the answer, it *severed* it. Measured on the 0.4.2 run, **18 of
40 subjects came back at exactly 64 characters**, mid-word:
`"... for Yolanda Bea"` (Beaumont), `"... - Internal 11"`, and
`"Tax Return - Supplemental Income and Loss (Rental Real Estate) -"` where the
party was the next thing to be written and never arrived. None of them carried a
flag, because the word count was still under ten so the checker's trimmer never
engaged. This is the same defect 0.4.1 fixed for `description` and introduced for
`subject` in the same change, on the reasoning that "64 characters is about ten
words of ordinary English" — a character cap cannot stand in for a word count.

Two changes, and the measurement that separates them. `maxLength` is now **95**,
which is not a guess but the whole filename budget: `compose` builds
`"YYYY-MM-DD " + subject` and needs `FILENAME_TAIL_RESERVE` on top, so at
`max_filename_len: 120` the subject can be `120 - 11 - 14 = 95` characters and
never trip `TooLong`. A test derives that arithmetic from the constants, so
moving either side fails rather than producing documents that quarantine on
length. And the prompt now prescribes the shape `<form> - <party>`, with the
short form identifier rather than the legal title.

Measured on the same 40 documents, scoring each filename against the document's
own `Taxpayer / Entity:` line:

| | 0.4.2 | 0.4.3 |
|---|---|---|
| party named exactly | 18 | **38** |
| party cut short | 8 | **0** |
| party garbled | 2 | **0** |
| party absent entirely | 12 | **2** |
| subjects at exactly the 64-char cap | 18 | **0** (longest now 87) |

The date lane improved without being touched, which was not predicted:
`dated_deep` documents named from their own date went from 2 of 8 to **4 of 8**,
run-dated fell from 16 to 14, and `DATE_PREFERRED_FROM_DOCUMENT` from 21 to 9 — the
printed date is chosen outright more often when the model is not also fighting a
severed subject. See item 0c, which this relieved.

**It cost 19% throughput: 9.58 to 11.61 s/file**, so 1,000 files went from 2.7 to
3.2 hours.

That number is the second attempt. The first wrote each subject prohibition as its
own rule — four bullets instead of two — and cost **22.95 s/file, 6.4 hours per
1,000**, for 37 of 40 on the party and 39 of 40 named. Twice the wall clock for
slightly worse results, because a system prompt is re-sent on every naming attempt
and every escalation, so prompt words are not free. The shorter prompt wins on party
accuracy, naming rate and speed together.

**Worth recording as method, not just result:** judging the four-bullet version on
its first ten documents said it was clearly the better prompt — the ten-document
sample showed degenerate output from the short one, including a repeated party and a
leaked EIN. Scoring all 40 reversed it. Ten documents mislead on this corpus; score
the whole sample.

### 0e. ~~A party cut short by filename length is recorded nowhere~~ — withdrawn, the premise was false
`compose` **rejects** an over-long name with `TooLong`; it has never truncated
one. The eight filenames this item was written about were cut by the schema
`maxLength` described in 0d, not by the filename budget, and
`SUBJECT_TRUNCATED`'s weak correlation with them (2 of 8) was the clue that the
two mechanisms were different — read at the time as the flag being unreliable
rather than as the diagnosis being wrong.

One real defect did come out of it and is fixed: a subject could arrive already
ending in a separator whose right-hand side was never emitted, and ship that way.
`sanitize_subject_inner` now strips a dangling tail from every model subject
rather than only from one it trimmed itself
(`a_subject_that_arrives_ending_in_a_separator_is_tidied`). It is deliberately
unflagged — it drops punctuation, never a word.

### 0f. ~~Character-level garbling of a party name is undetectable~~ — no longer observed
Zero garbled parties in the 0.4.3 run, against two in 0.4.2
(`Cross & Daughters Bakery` named `Cross & Daubs`, `Whitmore & Associates` named
`Whitmore &Associes`). Both were adjacent to the severed-subject bug of 0d, and
neither survived fixing it.

**This is "not observed", not "cannot happen".** Nothing was added that would
detect a mis-transcribed proper noun, and a 0.6B model is capable of one; the
sample is 40 documents. The third borderline case originally recorded here —
`Derrick Pena` becoming `Derrick Pén` — turned out to be the 64-character cut
landing mid-name, not an invented diacritic, so it was never garbling either. If
this reappears at scale, the options remain a per-token substring check of the
party against the document text, or documenting the rate.

### 0g. `SUBJECT_TRUNCATED` now fires on 35 of 40 and no longer distinguishes anything
The checker trims a subject over ten words at a word boundary and flags it. That is
the intended replacement for the schema severing subjects mid-word (item 0d), and it
is working. But the model now writes past eight words on **35 of 40** documents, up
from 10, so the flag fires on seven documents in eight.

Nothing that ships is wrong: the trim keeps the leading `<form> - <party>`, which is
what a filename is for, and the full wording stays in the description. The problem is
that the flag has stopped carrying information. A reviewer filtering on
`SUBJECT_TRUNCATED` to spot-check the interesting cases now gets almost the whole
batch, which is the same as having no flag.

Two options, neither obviously right. Stop flagging a trim that only removed words
after a complete `<form> - <party>` — cheap, but it needs a way to recognise that
shape, and the checker deliberately knows nothing about tax forms. Or raise
`SUBJECT_MAX_WORDS` from ten so eight-to-twelve-word subjects stop being trimmed at
all — which spends filename length on wording that is usually filler.

Not attempted here: both change what ships, so both want their own measured run
rather than being folded into a release that already moved the schema cap and the
prompt.

### 0h. The pipeline does not record how many naming attempts a document took
A rejection costs three naming attempts instead of one, and an escalation runs the
1.7B tier at roughly 3x the cost of the 0.6B — so attempts and tier are most of what
determines wall clock. Neither is recorded anywhere: not in the manifest, not in the
ledger, not in the app log except as an unattributed `warn` line.

That is why the 0.4.3 throughput question above took three full 40-document runs to
answer, when one run plus two numbers per document would have shown it directly. One
integer (attempts) and one string (which tier answered) per document would make the
next such question a query instead of an experiment, and would let
`docs/SIZING.md`'s throughput table say *why* a configuration is slow rather than
only that it is.

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

Because "run it before you push" is a request and not a gate, the tracked
`.githooks/` directory makes it one: `pre-push` runs the whole of
`scripts/ci-local.sh` and fails the push on any gate, and `pre-commit` runs the
seconds-long subset (`cargo fmt --check` plus the five file-reading gates).
`scripts/install-hooks.ps1` (or `.sh`) points `core.hooksPath` at that
directory, and until it has been run in a clone, nothing checks anything — see
README Setup step 0. `BACKLOG_SKIP_HOOKS=1` and `--no-verify` both bypass them,
so they remain a gate against mistakes rather than against intent.

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
