# Design decisions

Why the load-bearing choices are what they are. Each entry states the decision,
the alternative that was rejected, and what would make the decision wrong.

For *what is not finished*, see `KNOWN_ISSUES.md`. For the current
architecture, see the code; `../2026-07-21 Sortition Pipeline Design.md` is
historical.

---

## 1. The validator is in Rust, not in the prompt

**Decision.** A model proposes; a deterministic Rust function decides. Every
proposal passes through `backlog-core`'s `checker.rs` before anything is
renamed or indexed, and the checker's rules are ordinary code with ordinary
unit tests — 49 of them, runnable in 0.2 seconds with no model present.

**Rejected.** Constraining the model harder: a stricter grammar, a stricter
system prompt, a second model as judge.

**Why.** The product's actual promise is "no date ships unless it is in the
document". A prompt cannot make that promise; only a function that goes and
looks can. The rule that matters —
`CheckError::DateNotInEvidence` — is a set membership test against dates
harvested by regex from the document text and the file's metadata. It is
trivially auditable, it cannot be talked out of its answer, and it is the same
function whether the proposal came from a 0.6B model, a 1.7B model, or the
person in the review pane.

The corollary is the reason `backlog-core` is a separate crate: the trust core
must be testable with no Tauri, no sidecar binaries, no icon and no GGUF, on a
plain Linux box, in seconds — which is what makes `./scripts/ci-local.sh
trust-core` a thing anyone can run. Before the extraction, `tauri-build` aborted the
whole compile when the sidecars were missing, so a fresh checkout could not run
a single checker test. That is not a build-system nicety; a guarantee nobody can
cheaply re-verify stops being a guarantee.

**What would make this wrong.** If the checker's rejection rate on genuine
documents were high enough that the review queue became the product, the answer
would still not be a laxer checker — it would be better evidence into the model.
See the retry ladder, where each rung varies the *evidence bundle* rather than
just the model tier.

## 2. SQLCipher for the ledger, with the key under DPAPI

**Decision.** `ledger.db` is whole-file encrypted with SQLCipher (via
`rusqlite`'s `bundled-sqlcipher-vendored-openssl`). The 256-bit key is
generated on first open, protected with Windows DPAPI (`CryptProtectData`), and
stored at `<data_dir>/ledger.key`.

**Rejected.** (a) Plain SQLite plus NTFS permissions. (b) Column-level
encryption of the sensitive fields. (c) Asking the user for a passphrase.

**Why.** The ledger holds every proposed subject, description, original
filename and local path for a corpus that is, by the product's own description,
HR- and legal-shaped: folder names like `2024 Terminations`. That is a
plaintext index of sensitive material sitting in a user profile on a laptop.
NTFS permissions do not survive the disk leaving the building. Column-level
encryption would leave the *filenames* — often the most revealing part —
readable in an index. And a passphrase is unusable by a product whose stated
user must never open a terminal, and would be written on a sticky note.

DPAPI is the right key store precisely because it is *not* portable: the key
decrypts only for the same Windows user on the same machine, which is exactly
the trust boundary the appliance already has.

**Cost, accepted.** `bundled-sqlcipher-vendored-openssl` compiles OpenSSL from
source, which is why the build machine needs a native Perl and NASM
(`RELEASING.md`). And DPAPI is Windows-only, which is why it sits behind
`#[cfg(windows)]` — that gate is what lets the whole app crate compile and test
on Linux.

**What would make this wrong.** A deployment where the ledger has to be read by
a different user or moved between machines. There is no export path today, and
deleting `ledger.key` makes the database permanently unreadable — stated
plainly in `PRIVACY.md` because it is the intended behaviour of "remove my
data", not a footgun.

## 3. The slim, torch-free sidecar

**Decision.** `convertd` ships without `torch`, `transformers`,
`sentence-transformers` or `gliclass`. The three features that needed them —
GLiClass document-type classification, Granite-embedding sentence salience, and
the Ettin span lane — degrade to deterministic fallbacks that return
`ok=true` with `available: false`.

**Rejected.** Shipping torch so the naming enhancements are available.

**Why.** Three reasons, in order of weight:

1. **No op may fail over a missing enhancement.** `filter.rs`'s
   `build_evidence` flags a document when a sidecar op errors. If `classify`
   raised on a missing library, every document in the batch would land in
   NeedsReview because of an optional feature. The fallbacks are what make the
   dependency genuinely optional — and the contract is load-bearing enough that
   `op_salience` importing numpy at the top of the function, above its own
   short-circuit, was a live bug.
2. **Roughly 3x the Python dependency footprint**, torch alone about 500 MB
   installed, all of it inside a PyInstaller one-file binary that has to be
   downloaded, virus-scanned and installed on an office machine.
3. **The enhancements are marginal.** They rank evidence and hint a doc type.
   Naming quality degrades slightly without them; conversion, OCR, language ID,
   harvest, naming and the checker are untouched.

**What would make this wrong.** Measured naming quality on the pilot corpus
that is materially better with the enhancements on. That is a measurement
nobody has taken, which is the honest state of it.

## 4. One index row per physical copy, keyed by `manifest_id` and not by `sha256`

**Decision.** Three byte-identical files at three paths get one content
SHA-256, three `manifest_id` values, three index rows and three filenames
(`base`, `(2)`, `(3)`). Flow 2's idempotency gate queries `ManifestId` and is
explicitly forbidden from querying `Sha256`.

**Rejected.** Deduplicating on content hash so identical documents are indexed
once.

**Why.** The two are not the same question. "Is this the same document?" is
`sha256`. "Have I already committed *this delivery*?" is `manifest_id`. A
SharePoint index built on the first answer silently loses the second and third
copy of every duplicated document — and in a real corpus, the same signed
agreement filed in three client folders is three real filings, not two
mistakes.

`manifest_id` is `SHA256(content_sha || 0x00 || normalized_relpath)`: stable
across replays of the same delivery, distinct across different deliveries, and
64 lowercase hex so it is a legal NTFS filename. The earlier `{sha}:{uuid}`
form failed both ways — `:` is invalid on NTFS so the write silently failed,
and the fresh UUID made replay non-idempotent.

The duplicates still carry `duplicate_of` pointing at the shared content hash,
so the relationship is queryable without being enforced.

## 5. The installer is per-user, and carries the WebView2 runtime

**Decision.** `bundle.windows.nsis.installMode: "currentUser"` and
`bundle.windows.webviewInstallMode: {"type": "offlineInstaller"}`.

**Rejected.** The CLI defaults, which are per-machine-ish and
`DownloadBootstrapper` respectively.

**Why.** The updater installs passively
(`plugins.updater.windows.installMode: "passive"`). A per-machine install would
raise a UAC prompt on every passive update — a prompt a non-administrator
appliance user cannot satisfy, from a background flow with no visible
consequence, so the update fails silently and the fleet stops updating with no
signal anywhere. Per-user makes the passive update actually passive.

`DownloadBootstrapper` fetches WebView2 from Microsoft in the middle of the
install. That contradicts "offline-first appliance" on exactly the machines
most likely to need it: managed fleets with egress filtering, and Win10 LTSC
images that ship no Evergreen WebView2. `offlineInstaller` embeds the runtime
(about 127 MB) so the install completes with no network at all.

**Cost, accepted.** ~127 MB of installer, against 2.4 GB of model weights the
same machine will download anyway. The size is not the binding constraint.

## 6. Source-available, not open source

**Decision.** `LICENSE` is proprietary with an evaluation grant.
`package.json` declares `SEE LICENSE IN LICENSE`; `NOTICE.md` enumerates every
redistributed third-party component separately.

**Rejected.** (a) Leaving `UNLICENSED` with no LICENSE file. (b) Apache-2.0.

**Why.** (a) was untenable: the repo is public enough that the updater fetches
from `releases/latest`, and the redistribution gate in
`DEPENDENCY_COMPATIBILITY.md` cannot be closed without a notice file — an
Apache-2.0 model bundle imposes obligations that need somewhere to live.
(b) would be a real and irreversible business decision, and nothing in the
codebase indicates it was made.

**What would make this wrong.** The owner intending an open-source release.
This is the one decision in this file that is a placeholder for a person's
choice rather than a technical conclusion — change the file, not the code.
