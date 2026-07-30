# Sizing: what this needs, and what it does with it

Measured, not estimated. Every number below came off a real run of the shipped
binaries; where a figure is calculated rather than observed it says so.

Reproduce any of it with the load harness described at the bottom.

## The short answer

| Question | Answer |
|---|---|
| Runs on 8 GB RAM, no dedicated GPU? | **Yes**, at `slm_parallel: 1`, which is now the automatic default on that machine. |
| Runs on 8 GB with the shipped pre-0.4.0 defaults? | **No.** `slm_parallel: 4` puts the two model servers alone at 6,078 MB. |
| A GPU helps? | Not used at all. `llama-server` is the CPU build; there is no CUDA/Vulkan path in this product. |
| 1,000 tax PDFs/DOCX of 2–12 pages? | Completes, nothing is dropped. Budget **hours, not minutes** — see Throughput. |

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
| Qwen3-1.7B | 4 | 16,384 | 3,609 MB | 1,966 MB |

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
2–12 page PDFs and DOCX:

| Batch | Model tiers | Wall clock | Per file | Named `ok` |
|---|---|---|---|---|
| 1 file | 0.6B only | 19.4 s | 19.4 s | 1/1 |
| 12 files | 0.6B only | 286.9 s | 23.9 s | 2/12 (17%) |
| 12 files | 0.6B + 1.7B | 411.2 s | 34.3 s | 7/12 (58%) |

Single-request generation measured at **31 tokens/sec** on the 0.6B at
`parallel=1`.

**Extrapolated to 1,000 files: 6.6–9.5 hours.** The escalation tier is the
difference between the two ends: it more than triples the auto-naming rate and
costs about 43% more wall clock, because a document that escalates pays for
three naming attempts instead of one. `docs/USER_GUIDE.md`'s advice to leave a
large batch running overnight is the correct operational posture.

These figures are from a 16-core Ryzen 7 PRO 8840HS. An 8 GB laptop will
typically have fewer cores and will be slower; treat the numbers as an upper
bound on speed, not a promise.

## Naming quality, honestly

From the 12-file run with both tiers, scored against the corpus's ground truth:

| Fixture shape | `ok` | flagged | Is flagging correct here? |
|---|---|---|---|
| date on page 1 | 7 | 2 | No — those two are genuine misses |
| ambiguous date only (`04/05/2023`) | 0 | 2 | **Yes.** An ambiguous date must go to a human |
| date only on page 3+ | 0 | 1 | No — a genuine miss |

Two of the seven `ok` results named the document from its **file modified time**
(`date_source: metadata`, note `DATE_FROM_FILE_MTIME`) rather than from a date
that was present in the text. That is the designed fallback behaving honestly,
but on a document that *does* carry a date it is a miss wearing a truthful label.

A second run over a deliberately harder stratified sample — equal weight on
undated, ambiguous and deep-dated documents rather than the mostly-page-1 mix
above — came out lower, and is the more honest planning number. First 14
documents of that run:

| Fixture shape | n | named `ok` | of which from mtime | flagged |
|---|---|---|---|---|
| date on page 1 | 6 | 3 | 1 | 3 |
| date only on page 3+ | 4 | 2 | 0 | 2 |
| ambiguous date only | 1 | 1 | 0 | 0 |
| **no date anywhere** | **3** | **0** | 0 | **3** |

That undated row is what 0.4.1 fixes; see `docs/KNOWN_ISSUES.md` item 0. Those
documents now take the mtime fallback and carry `DATE_FROM_FILE_MTIME` plus
`DATE_PROPOSAL_DISCARDED:<what the model proposed>`. The remaining failures on
undated fixtures are subject and description rejections, not date ones — a
separate naming-quality limit of a 0.6B/1.7B model on sparse "draft working
notes" pages, not a rule that cannot be reached.

Run-to-run variance is worth knowing before reading too much into any single
number here: the same 12 documents have produced 2, 4 and 7 successes across
runs. llama.cpp's slot assignment and batching shift the numerics even at
`temperature: 0`. Compare configurations on tens of documents, not twelve.

That run was stopped at 14 of 40 on purpose: a failing document costs three
naming attempts instead of one, so a failure-weighted sample runs several times
slower per file than a representative one and was buying no new information.

The undated row is the one to read twice. Those fixtures contain no date-shaped
text at all, so they are exactly the case `README.md`'s mtime fallback is
advertised for, and none of them reached it — see `docs/KNOWN_ISSUES.md` item 0.
The fallback fires only when the model returns `"none"`; on tax pages full of
years it proposes a date instead, the checker correctly refuses it, and the file
is flagged. Quarantine plus a `NeedsReview` row is the safe outcome, but it is
not the documented one.

So the practical expectation for a 1,000-file tax backfill is that **roughly
half to two-thirds get named automatically** and the rest land in Needs Review
for a person. That is the product working as designed — `checker.rs` refuses any
date it cannot prove against the document text — not a defect. Plan the pilot
around a human reviewing a few hundred documents, and read
`docs/PILOT_RUNBOOK.md`'s staged 50/200/500 batches before committing to the
full set.

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
