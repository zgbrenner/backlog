# Sizing: what this needs, and what it does with it

Measured, not estimated. Every number below came off a real run of the shipped
binaries; where a figure is calculated rather than observed it says so.

Reproduce any of it with the load harness described at the bottom.

## The short answer

| Question | Answer |
|---|---|
| Runs on 8 GB RAM, no dedicated GPU? | **Yes**, at `slm_parallel: 1`, which is now the automatic default on that machine. |
| Runs on 8 GB with the shipped pre-0.4.0 defaults? | **No.** `slm_parallel: 4` puts the two model servers alone at 6,078 MB. |
| How large is the semantic evidence model? | **22.1 MiB on disk**: the pinned quantized ONNX graph is 22,972,370 bytes and its tokenizer is 231,508 bytes. The model is loaded by `convertd` and is separate from the Qwen KV-cache figures below. |
| A GPU helps? | Not used at all. `llama-server` is the CPU build; there is no CUDA/Vulkan path in this product. |
| 1,000 tax PDFs/DOCX of 2–12 pages? | Completes, nothing is dropped. **~3.2 hours** measured on a 16-core desktop (see Throughput) — an 8 GB laptop with fewer cores will be slower. Treat it as an upper bound on speed, not a promise; an overnight run either way. |
| Why 3.2 hours when 0.4.2 did it in 2.7? | 0.4.3 bought the party name in the filename — 38 of 40 correct against 18 — for 19% more wall clock. See Naming quality. |

## Memory

The cost is dominated by the **KV cache, not the weights**, and the KV cache is
sized from `slm_parallel`. `slm.rs` derives `--ctx-size` as `4096 * slm_parallel`
and llama.cpp preallocates the whole thing at startup.

Qwen3 0.6B and 1.7B share the attention shape that matters (28 layers, 8 KV
heads, head_dim 128, F16 cache), so they pay the **same** KV cost:

```
2 (K and V) x 28 layers x 8 kv-heads x 128 head-dim x 2 bytes = 114,688 B/token
114,688 B x 4096 tokens = 448 MiB per parallel slot
```

Measured on Windows 11, `llama-server` b10091, Q8_0 weights:

| Model | `slm_parallel` | `--ctx-size` | Working set | Private commit |
|---|---|---|---|---|
| Qwen3-0.6B | 1 | 4,096 | 1,123 MB | 590 MB |
| Qwen3-0.6B | 2 | 8,192 | 1,572 MB | 1,040 MB |
| Qwen3-0.6B | 4 | 16,384 | 2,469 MB | 1,938 MB |
| Qwen3-1.7B | 1 | 4,096 | 2,262 MB | 617 MB |
| Qwen3-1.7B | 2 (calculated) | 8,192 | ~2,710 MB | not measured |
| Qwen3-1.7B | 4 | 16,384 | 3,609 MB | 1,966 MB |

The `slm_escalation_parallel: 2` row is **calculated, not measured**: it
interpolates the 448 MiB/slot formula above onto the 1.7B's own parallel-1
baseline (2,262 + 448 ≈ 2,710 MB) rather than a fresh capture, because
`slm_escalation_parallel` defaults to 2 only above 9 GiB of installed RAM and
the build machine that produces this table is the one machine that can never
land on the 8 GB branch either — see the note on `slm_parallel_is_chosen_from_installed_ram`
below for the same limitation. Treat it as an estimate until it is measured.

**Both tiers are resident at once on any real batch.** `SlmLane` holds `primary`
and `escalation` in separate slots, and once a document needs a third naming
attempt the 1.7B server starts and stays up for the rest of the run. So the
number to budget is the pair:

| Both tiers | Working set | Private commit |
|---|---|---|
| `slm_parallel: 1` | **3,385 MB** | **1,207 MB** |
| `slm_parallel: 4` | 6,078 MB | 3,904 MB |

Those are freshly-loaded figures. Working set grows as the servers actually serve
requests. Measured mid-batch at `slm_parallel: 1`, with both model servers and
convertd all live:

| Component | Working set |
|---|---|
| llama-server (escalation, 1.7B) | 2,477 MB |
| llama-server (primary, 0.6B) | 1,715 MB |
| convertd (worker + PyInstaller bootstrap stub) | 204 MB |
| **total model + sidecar layer** | **4,396 MB** |

Add the app and WebView2 (~400 MB) and Windows itself (2.5–3 GB) and an 8 GB
machine is at roughly 7.8 GB — inside the box, but without much room. It
degrades rather than dies: the largest single contributor is memory-mapped GGUF
pages, which Windows evicts under pressure at the cost of speed, not
correctness. Do not run a 1,000-file backfill and a video call on the same 8 GB
laptop.

`convertd` legitimately appears twice in Task Manager. PyInstaller's one-file
build starts a small bootstrap stub which unpacks to `%TEMP%` and re-execs the
real interpreter as a child, so two processes is one sidecar, not a leak.

Working set includes the memory-mapped GGUF pages, which Windows can evict under
pressure; private commit is the part that must actually fit. On an 8 GB machine
with Windows itself using 2.5–3 GB, `slm_parallel: 4` overcommits and thrashes —
which is not a crash, it is every document getting slower for the whole batch.

`slm_parallel` costs no naming quality. Per-slot context is 4,096 tokens
regardless of the setting (total is `4096 x n` shared across `n` slots), so the
evidence bundle has exactly as much room at 1 as at 4. What you lose is
cross-file overlap in the naming stage, which is rarely the bottleneck — see
below.

### Defaults are now chosen from installed RAM

`config.rs`'s `default_slm_parallel()` reads total physical memory:

| Installed RAM | `slm_parallel` | Both tiers, working set |
|---|---|---|
| <= 9 GiB | 1 | ~3.4 GB |
| <= 17 GiB | 2 | ~4.3 GB |
| > 17 GiB | 4 | ~6.1 GB |
| unknown | 2 | conservative on purpose |

An explicit `slm_parallel` in `backlog.config.json` always wins. This only
affects the value written on a fresh install.

## Throughput

`Sidecar` runs a pool of `convert_workers` convertd processes, so that many
documents convert at once. Until 0.4.1 it held a single process behind one mutex
for the whole request/response round trip, and `convert_workers` bought queue
depth rather than parallelism.

Measured on this corpus, warm pool, conversion stage only
(`convert_throughput_scales_with_workers`):

| Workers | 13 documents |
|---|---|
| 1 | 3.2 s |
| 4 | 2.4 s (1.33x) |

1.33x rather than 4x because these fixtures are small text-layer PDFs where the
per-request JSON round trip, not the conversion, dominates. The gain grows with
per-document work — a scanned page running three escalating RapidOCR passes is
seconds of CPU, and that is what parallelises.

**Warm the pool before timing anything.** convertd is a PyInstaller one-file
build, so a worker's first request pays for unpacking to `%TEMP%` and starting a
Python interpreter. Timing that measures N cold starts against one and concludes
pooling is *slower* — the first version of the benchmark above reported 0.75x for
exactly that reason. A real backfill holds one pool across the whole run, so
startup is paid once.

**On this workload the naming lane, not conversion, sets the wall clock.** With
`slm_parallel: 1` on an 8 GB machine, converted documents queue behind a single
naming slot, so pooling conversion alone leaves the end-to-end time roughly
unchanged — a 12-file batch measured 34.3 s/file before the pool and 40.25 s/file
after it with four workers, the difference being CPU the extra workers took from
llama-server. Conversion parallelism pays off when `slm_parallel` can also rise,
which is a function of RAM.

Measured, `slm_parallel: 1`, `convert_workers: 1`, mixed synthetic tax corpus of
2–12 page PDFs and DOCX, versions 0.4.0/0.4.1 — kept here to show the trend;
superseded by the version table below:

| Batch | Model tiers | Wall clock | Per file | Named `ok` |
|---|---|---|---|---|
| 1 file | 0.6B only | 19.4 s | 19.4 s | 1/1 |
| 12 files | 0.6B only | 286.9 s | 23.9 s | 2/12 (17%) |
| 12 files | 0.6B + 1.7B | 411.2 s | 34.3 s | 7/12 (58%) |

Single-request generation measured at **31 tokens/sec** on the 0.6B at
`parallel=1`.

At the time, that extrapolated to **6.6–9.5 hours for 1,000 files.** The
escalation tier was the difference between the two ends: it more than tripled
the auto-naming rate and cost about 43% more wall clock, because a document
that escalated paid for three naming attempts instead of one.

**0.4.3 is the current, authoritative measurement.** Same hardware, same
`slm_parallel: 1` and `convert_workers: 1`, both tiers live, over the
40-document deliberately-hard stratified sample used throughout this doc (see
Naming quality, honestly):

| Version | Documents | Wall clock | Per file | Named `ok` | Extrapolated to 1,000 files |
|---|---|---|---|---|---|
| 0.4.1 | 40 | 424 s | 10.6 s | 40/40 | 2.9 hours |
| 0.4.2 | 40 | 383 s | 9.58 s | 40/40 | 2.7 hours |
| 0.4.3 | 40 | 464 s | 11.61 s | 40/40 | **3.2 hours** |

**0.4.3 costs 19% more wall clock, and the reason is the prompt, measured.** It
bought the party name — 38 of 40 filenames name the right party against 18 — by
widening the subject schema cap and prescribing the subject's shape.

An intermediate version stated each subject prohibition as its own rule and cost
**22.95 s/file, 6.4 hours per 1,000**, for 37 of 40 on the party and 39 of 40 named:
twice the wall clock for slightly worse results. A system prompt is re-sent on every
naming attempt and every escalation, so prompt length is a throughput decision here,
not only a quality one. The shipped prompt is the shorter of the two and wins on
party accuracy, naming rate and speed at once.

Neither `slm_parallel` nor `convert_workers` moved from the earlier runs — both
are still 1. **The step change belongs to 0.4.1, not 0.4.2**: fixing false
rejections in the naming checker, not adding parallelism, is what took this from
34.3 s/file to 10.6. A rejection costs three naming attempts instead of one, so
removing false rejections did far more for wall clock than pooling conversion
did. 0.4.2 holds that rate and spends its changes on date provenance instead —
see Naming quality, honestly. `docs/USER_GUIDE.md`'s advice to leave a large
batch running overnight is still the correct operational posture; it is just a
more comfortable margin now.

These figures are from a 16-core Ryzen 7 PRO 8840HS. An 8 GB laptop will
typically have fewer cores and will be slower; treat the numbers as an upper
bound on speed, not a promise.

## Naming quality, honestly

Measured on 0.4.3, `slm_parallel: 1`, `convert_workers: 1`, both tiers live: the
40-document deliberately hard stratified sample used throughout this doc — 2–12
page PDFs and DOCX, weighted equally toward undated, ambiguous and deep-dated
documents rather than the easier mostly-page-1 mix a random sample would give.

**40 of 40 named `ok`. Zero flagged, zero quarantined**, held from 0.4.1 and 0.4.2.

`date_source` across the 40:

| `date_source` | 0.4.2 | 0.4.3 |
|---|---|---|
| `document` | 24 | **26** |
| `metadata` | 16 | 14 |

Fixture shape, taken from the corpus generator's own ground truth
(`corpus_manifest.csv`'s `shape` column), against whether the file ended up named
from a date belonging to the document:

| Fixture shape | n | 0.4.2 | 0.4.3 |
|---|---|---|---|
| `dated_page1` — date printed on page 1 | 16 | 16 of 16 | **16 of 16** |
| `dated_deep` — only date is on page 3 or later | 8 | 2 of 8 | **4 of 8** |
| `ambiguous` — only date is ambiguous (`04/05/2023`) | 4 | 4 of 4 | **4 of 4** |
| `sensitive` — carries an SSN or card number | 4 | 2 of 4 | 2 of 4 |
| `undated` — no date-shaped text anywhere | 8 | 0 of 8, by design | 0 of 8, by design |
| **total** | **40** | 24 | **26** |

Two rows are correct outcomes rather than failures. The `undated` fixtures contain
no date to find, so the file modified time is the right answer and they are named
from it with `date_source: metadata`. Two of the four `sensitive` fixtures also
have an empty `date_str` in the ground truth — they are undated too — so that row
is really 2 of 2 where a date exists. Because the corpus was generated the same day
it was measured, an mtime *is* the run date, which is why the run-date count cannot
fall below ten on this sample.

Date provenance across the three releases:

| Metric | 0.4.1 | 0.4.2 | 0.4.3 |
|---|---|---|---|
| Named with the RUN date rather than the document's own | 37 of 40 | 16 of 40 | **14 of 40** |
| Page-1 date correctly used | 2 of 16 | 16 of 16 | **16 of 16** |
| `dated_deep` date correctly used | — | 2 of 8 | **4 of 8** |

**Ten of the fourteen run-dated files are fixtures with no date to find**, so the
genuine remainder is **four documents that had a date and did not get it** — down
from six in 0.4.2. All four are `dated_deep`, the harvest-window limit of item 0c.

**"Named" is still not the same as "named well."** `SUBJECT_TRUNCATED` fired on
**35 of 40**, up from 10, because the checker's word trim is now the thing doing the
shortening instead of the schema severing the answer mid-word. That is the intended
direction — a flagged trim at a word boundary beats a silent cut mid-word — but it
means the model's raw subject needs repair on seven documents in eight, and the flag
has stopped distinguishing anything. Recorded as item 0h: noise, not a defect in the
names that ship.

Soft-flag histogram (a document can carry more than one; a soft flag is a recorded
note on a document that was still named, not a failure):

| flag | 0.4.2 | 0.4.3 |
|---|---|---|
| SUBJECT_TRUNCATED | 10 | **35** |
| DATE_SOURCE_CORRECTED | 15 | 12 |
| DATE_PREFERRED_FROM_DOCUMENT | 21 | **9** |
| DESCRIPTION_TRIMMED_TO_ONE_SENTENCE | 1 | 2 |
| DATE_FROM_FILE_MTIME | 1 | 2 |
| DATE_PROPOSAL_DISCARDED | 1 | 2 |
| DATE_FROM_BODY | 0 | 2 |
| SUBJECT_UNGROUNDED | 0 | 1 |

`DATE_PREFERRED_FROM_DOCUMENT` falling from 21 to 9 is the one to notice: the
printed date is more often chosen outright, without the checker having to substitute
it.

Representative results: `2022-08-12 Form 1120-S - Patrick Kowalski.pdf`,
`2021-09-28 Form 1099-NEC - Ironwood & Vance Roofing - 2021 Tax Return.pdf`.

### Does the filename name the right party?

The date work above is measured; the *party* had never been until 0.4.2 was
already out. It is half of what a filename is for. Scored against each document's
own `Taxpayer / Entity:` line — read out of the document text, because the corpus
generator draws the filename's party and the body's party independently, so an
original filename is never evidence:

| Result | 0.4.2 | 0.4.3 |
|---|---|---|
| **EXACT** — the full party appears in the new filename | 18 (45%) | **38 (95%)** |
| **PARTIAL** — a correct prefix, cut short | 8 | **0** |
| **GARBLED** — characters altered, not merely cut | 2 | **0** |
| **ABSENT** — no party in the filename at all | 12 | **2** |
| flagged, so never named | 0 | **0** |

**What was actually wrong was not a word budget.** The first write-up of this
called it one — ten words, spent on the form's legal title. The real cause was the
naming schema's `maxLength: 64` on `subject`. llama.cpp enforces `maxLength` by
refusing to emit another character, so it did not shorten the subject, it severed
it: **18 of the 40 came back at exactly 64 characters**, mid-word, and none carried
a flag because the word count was still under ten.

```
'Form 4562 - Depreciation and Amortization Return for Yolanda Bea'   (Beaumont)
'Tax Return - Supplemental Income and Loss (Rental Real Estate) -'   (party was next)
'S Corporation Tax Return for Whitmore & Associates - Internal 11'
```

The cap is now 95 — the whole filename budget, `120 - 11 - 14`, with a test
deriving it from those constants — and the prompt prescribes `<form> - <party>`
using the short form identifier. The longest subject in the 0.4.3 run is 87
characters and none sits at the cap, so nothing is being severed any more.

**The date lane improved without being touched**, which was not predicted and is
the most interesting number here:

| | 0.4.2 | 0.4.3 |
|---|---|---|
| `dated_deep` named from its own date | 2 of 8 | **4 of 8** |
| named with the run date | 16 of 40 | **14 of 40** |
| `date_source: document` | 24 | **26** |
| `DATE_PREFERRED_FROM_DOCUMENT` fired | 21 | **9** |

Nothing in the date logic changed between those two runs. The most likely reading
is that the evidence bundle always held the date and the model was the bottleneck:
one not spending its output on a severed form title also picks the date better. It
also means the harvest-window ceiling of `docs/KNOWN_ISSUES.md` item 0c is a smaller
problem than 2-of-8 suggested — though the intermediate prompt reached 6 of 8, so
how much of that ceiling is the window and how much is the model is still open.

**Two parties are still absent**, and the fix cost 19% throughput — see the
Throughput table.

Run-to-run variance is worth knowing before reading too much into any single
number here: the same 12 documents have produced 2, 4 and 7 successes across
runs. llama.cpp's slot assignment and batching shift the numerics even at
`temperature: 0`. Compare configurations on tens of documents, not twelve.

## Reproducing this

`pipeline.rs`'s `e2e_real_batch` is an `#[ignore]`d load harness that drives the
real sidecars and real weights against real folders. It is not one of the five
gates and never runs in `cargo test`.

```powershell
$env:BACKLOG_E2E_PROCESSING = "C:\...\Processing"
$env:BACKLOG_E2E_OUTBOX     = "C:\...\Outbox"
$env:BACKLOG_E2E_QUARANTINE = "C:\...\Quarantine"
$env:BACKLOG_E2E_CONVERTD   = "$env:LOCALAPPDATA\BackLog\convertd.exe"
$env:BACKLOG_E2E_LLAMA      = "$env:LOCALAPPDATA\BackLog\llama-server.exe"
$env:BACKLOG_E2E_PRIMARY    = "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-0.6B-Q8_0.gguf"
$env:BACKLOG_E2E_ESCALATION = "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-1.7B-Q8_0.gguf"
$env:BACKLOG_E2E_PARALLEL   = "1"
$env:BACKLOG_E2E_WORKERS    = "1"
cd src-tauri
cargo test -p backlog --lib e2e_real_batch -- --ignored --nocapture
```

Omit `BACKLOG_E2E_ESCALATION` to measure the primary-only shape. It prints a
flag-reason histogram, the ten slowest documents, the auto-named percentage and
a 1,000-file extrapolation, and asserts the invariants that matter: one manifest
per file, every manifest `ok` or `flagged`, quarantine holding exactly the
flagged ones, and Processing holding exactly the `ok` ones.
