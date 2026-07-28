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
