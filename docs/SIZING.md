# Sizing: what this needs, and what it does with it

Measured, not estimated. Every number below came off a real run of the shipped
binaries; where a figure is calculated rather than observed it says so.

**The headline result on this branch is an evidence-coverage fix, not a model
change.** `SEMANTIC_TOP_K` was a hardcoded 12 with no relationship to
`evidence_token_budget`; it is now derived from it, and the semantic lane's
share of the character budget went 35% → 60%. That made the shipped
configuration **19% faster and substantially wider at once** — see
[The coverage fix](#the-coverage-fix).

A proposed model-tier change — Qwen3-1.7B primary and Qwen3-4B-Q4_K_M
escalation — was measured against the pair it would replace and **rejected**:
at matched coverage it cost **2.0x the wall clock** for a two-document
faithfulness difference at n=26. It was rejected on quality-per-second, not on
memory; the 1.7B primary fits the 16 GB class comfortably. See
[The rejected model-tier swap](#the-rejected-model-tier-swap).

Two things about the numbers in this document, both learned the hard way:

- **Two end-to-end batches run on 2026-08-06 were void** — measured against a
  stale sidecar that silently disabled the semantic evidence lane. Their
  figures have been withdrawn rather than corrected in place. See
  [Why the first two runs were void](#why-the-first-two-runs-were-void).
- **Any throughput figure in this repository dated before 2026-08-07 may have
  been measured with the semantic lane silently off**, because the same failure
  mode leaves no trace. Enabling that lane costs 58–82% of wall clock by
  itself. The historical tables are left as recorded, with that caveat attached
  where they appear.

Reproduce any of it with the load harness described at the bottom — and note
the sidecar path in that recipe, which is what went wrong.

## The short answer

| Question | Answer |
|---|---|
| Runs on 8 GB RAM, no dedicated GPU? | **Yes**, at `slm_parallel: 1`, which is the automatic default on that machine — and it keeps the Qwen3-0.6B primary and the single collapsed server it has always had. |
| Runs on 8 GB with the shipped pre-0.4.0 defaults? | **No.** `slm_parallel: 4` puts the two model servers alone at 6,078 MB. |
| What does the 16 GB target machine run? | Qwen3-0.6B primary and an optional Qwen3-1.7B escalation, one slot each: **1,838 MiB resident most of the time, 4,815 MiB while an escalation is live.** About 2.3 GiB of the pair is mmapped weights, so private commit is ~2.4 GiB. See [Memory](#memory). |
| How large is the semantic evidence model? | **22.1 MiB on disk**: the pinned quantized ONNX graph is 22,972,370 bytes and its tokenizer is 231,508 bytes. The model is loaded by `convertd` and is separate from the Qwen KV-cache figures below. |
| A GPU helps? | Not used at all. `llama-server` is the CPU build; there is no CUDA/Vulkan path in this product. |
| 1,000 tax PDFs/DOCX of 2–12 pages? | Completes, nothing is dropped. **~5.6 hours** — 20.03 s/file, measured on the shipped configuration: `slm_parallel: 1`, `convert_workers: 3`, Qwen3-0.6B primary, semantic coverage at its raised setting. That is faster than anything previously recorded here, and the coverage fix is why. The configuration is part of the number; it is meaningless without it. |
| Should the 1.7B/4B swap be made? | **On this evidence, no.** At matched coverage it is 2.0x the wall clock (40.11 against 20.03 s/file) for a two-document faithfulness difference at n=26. See [The rejected model-tier swap](#the-rejected-model-tier-swap). |
| Did the bigger model name better? | **No difference detectable on this corpus.** At matched coverage: faithfulness 25/25 against 23/25, party named 25/26 against 25/26, top-entity proxy 13/26 against 19/26. Two documents on the only metric favouring the larger pair. The corpus is not stratified for difficulty and n=26 is below what this document's own variance warning says is readable — an absence of evidence either way, not evidence of no difference. |
| Why 3.2 hours when 0.4.2 did it in 2.7? | 0.4.3 bought the party name in the filename — 38 of 40 correct against 18 — for 19% more wall clock. See Naming quality. |

## Memory

The cost is dominated by the **KV cache, not the weights**, and the KV cache is
sized from `slm_parallel`. `slm.rs` derives `--ctx-size` as `6656 * slm_parallel`
— 4,096 before the 1.7B/4B tier change — and llama.cpp preallocates the whole
thing at startup. The context grew because the evidence budget did: 2,500 tokens
on attempts 1 and 2, 4,000 on the escalation, plus the system prompt, the
operator's naming notes and 220 tokens of output all have to fit in one slot.

### KV cost is a property of the model, not a constant

Through 0.9.x this section said Qwen3 0.6B and 1.7B "share the attention shape
that matters", and a single 448 MiB-per-slot constant covered both. That is
true of the shipped pair and it is exactly the kind of coincidence that becomes
a bug the first time a tier moves: Qwen3-4B is a **36-layer** model where both
shipped models are 28-layer, so it pays **1.286x** more KV per token. Anything
hardcoding 28 / 8 / 128 would have been wrong for it by that factor.

The 4B is not shipped — see [the rejected swap](#the-rejected-model-tier-swap) —
but the generalization it forced is kept, and so is its row in the table, so
that an operator who configures one by hand is sized correctly rather than
mistaken for a 1.7B.

| Model | layers | kv-heads | head_dim | KV bytes/token (F16) | KV per slot at ctx 6,656 |
|---|---|---|---|---|---|
| Qwen3-0.6B | 28 | 8 | 128 | 114,688 | **728 MiB** |
| Qwen3-1.7B | 28 | 8 | 128 | 114,688 | **728 MiB** |
| Qwen3-4B | 36 | 8 | 128 | **147,456** | **936 MiB** |

```
kv_bytes_per_token = 2 (K and V) x layers x kv-heads x head-dim x 2 (F16)
per-slot KV        = kv_bytes_per_token x 6656
```

Layer counts are read from each model's own `config.json`, not assumed.
`config.rs` carries them in a per-model table keyed on GGUF basename and a
`kv_bytes_per_token(layers, kv_heads, head_dim)` helper, so the RAM ceilings take
the shape as an argument instead of holding a second hardcoded constant that
would go stale the next time a tier moves.

### There is no such thing as a flat compute-buffer allowance

The first cut of this section budgeted a flat ~200 MiB per server on top of
weights and KV. Measurement killed that. At rest, `--ctx-size 6656`,
`--parallel 1`:

| server | working set | private commit | weights + KV | measured remainder |
|---|---|---|---|---|
| Qwen3-1.7B-Q8_0 | 2,860 MiB | ~1,111 MiB | 2,477 MiB | **~383 MiB** |
| Qwen3-4B-Q4_K_M | 5,068 MiB | 2,842 MiB | 3,318 MiB | **~1,750 MiB** |

**4.6x the overhead for 1.25x the hidden size.** No single constant can be
pessimistic enough for the 4B without being absurd for the 1.7B, so the flat
`COMPUTE_BUFFER_BYTES` is gone and `config.rs`'s per-model shape table carries
an `overhead_bytes` field beside `layers`, `kv_heads` and `head_dim`. The
budgeted values round the measurements up for headroom — **500 MiB** for the
28-layer models, **1,900 MiB** for the 4B — which is what the tier table below
is computed from.

### The shipped tiers

Weights are the GGUF sizes: 0.6B Q8_0 610 MiB, 1.7B Q8_0 1,749 MiB. KV is from
the shape table above. Overhead is the budgeted `overhead_bytes`.

| Tier | primary (0.6B) | escalation (1.7B) | both resident |
|---|---|---|---|
| <= 9 GiB | 610 + 728 + 500 = **1,838 MiB** | collapsed onto the primary | **1,838 MiB** |
| <= 17 GiB | 610 + 728 + 500 = **1,838 MiB** | 1,749 + 728 + 500 = **2,977 MiB** | **4,815 MiB** |
| > 17 GiB | 610 + 2x728 + 500 = **2,566 MiB** | same 2,977 MiB | **5,543 MiB** |

The 4B is still in `config.rs`'s `GGUF_SHAPES` even though nothing ships or
downloads it: `shape_or_largest` uses it to size an unknown operator-supplied
GGUF pessimistically, and anyone who configures a 4B by hand is then budgeted
correctly at its real 5,218 MiB rather than mistaken for a 1.7B.

**Most of the time only the primary is resident.** `SlmLane` starts the
escalation server on the first document that needs a third attempt and reaps it
after `slm_escalation_idle_secs` (300 s) idle. So the resting figure on the
16 GB target is **1,838 MiB**, and **4,815 MiB is the transient worst case**
during a stretch of the batch with active escalations — not the steady state.

**How transient is not known.** An earlier note here claimed the escalation
server never started across a 33-file batch; that observation came from a run
since found to be void (see [Why the first two runs were
void](#why-the-first-two-runs-were-void)) and is withdrawn. Whether the 4B
starts on a normal corpus is [still pending](#still-pending). Budget for
4,815 MiB.

### The servers grow as they serve

At-rest figures are not what a machine holds an hour into a batch. llama.cpp's
Windows RSS growth is unfixed upstream (ggml-org/llama.cpp#24356), and on this
workload it is not a slow leak, it is fast:

| server | at rest | after 3 requests | by request 16 |
|---|---|---|---|
| Qwen3-1.7B-Q8_0 (shipped escalation) | 1,111 MiB private | — | 5,785 MiB working set |
| Qwen3-4B-Q4_K_M (rejected pair) | 2,842 MiB private | **3,880 MiB private** | 9,258 MiB working set |

That is the whole reason `slm_recycle_after_requests` **drops from 64 to 8**,
and the reason recycling now applies to **both** slots rather than only the
primary. The escalation server was previously governed by idle-reaping alone,
which by construction never fires on the server that is busy — that is, on
precisely the server that is growing. Sixteen consecutive escalations took the
1.7B from 2,860 to 5,785 MiB — and the 4B row, from the rejected pair, shows
where that curve goes on a larger model.

Recycling is affordable because a warm model load measured **3.5–3.6 s** against
naming requests measured in tens of seconds: one reload every 8 requests is a
small single-digit percentage of wall clock. That is the trade — a little
throughput for a bound on resident memory that llama.cpp does not otherwise
provide.

#### Whole-machine budget, 16 GB target, both servers up

The deployment target is a ~14.7 GB laptop — 16 logical cores, Radeon 780M,
CPU-only `llama-server` — which `total_ram_gib()` floors to 14 and which
therefore lands in the `<= 17 GiB` branch. That branch, not the `> 17 GiB` one,
is the tier this change has to be correct for.

```
naming lane (both servers)   4815 MiB   <- transient, not steady state
convertd 3x550               1650 MiB
app + WebView2                400 MiB
Windows                      2500-3000 MiB
                            ----------
                            ~9.1-9.6 GiB of ~13.7 GiB usable
```

**That headline overstates the pressure, in two specific ways, and it is worth
being precise about them rather than either hiding them or pretending the
number is comfortable.**

- **~2.3 GiB of the naming lane is memory-mapped weights.** Those pages are
  file-backed and evictable; they are not private commit. Private commit for
  the pair is about **2.4 GiB**. Working set is what Task Manager shows and
  what alarms people; private commit is what has to actually fit.
- **Both servers are up only for documents that failed two attempts.** The
  escalation server starts on the first such document and is reaped after
  `slm_escalation_idle_secs` (300 s). Most of a batch runs primary-only at
  **1,838 MiB**, leaving roughly **7 GiB** free for browsing, email and
  documents.

So the honest reading is: the steady state is comfortable, the worst case is
tight but survivable, and the worst case is bounded rather than open-ended
because of the recycling above. Two default changes are what keep it bounded:

- **`convert_workers_ram_ceiling`'s `<= 17 GiB` branch drops 4 to 3.** The
  at-rest arithmetic alone no longer forces this — four workers fit on paper.
  Two things say three anyway: at-rest is not steady state (the 1.7B measured
  2,860 → 5,785 MiB over 16 requests, and recycling bounds that drift rather
  than removing it, so the headroom this ceiling protects is the headroom that
  absorbs it), and the fourth worker buys throughput the pipeline cannot use
  while `Sidecar` serializes conversions.
- **`slm_escalation_idle_secs` drops 600 to 300.** The 1.7B escalation server
  is 2,977 MiB of the 4,815 MiB two-server footprint — 62% of the naming lane —
  and it is only wanted by the minority of documents that fail twice on the
  primary. Releasing it after five idle minutes rather than ten is what keeps a
  batch with sparse escalations resting at 1,838 MiB instead of 4,815. Reaping
  is completion-timestamped and cannot fire mid-request
  (`SlmLane::reap_idle_escalation`).

#### The 8 GB class collapses the escalation, deliberately

Every RAM tier runs the same Qwen3-0.6B-Q8_0 primary. What installed RAM
chooses is the *escalation*: a machine at 9 GiB or less — and one that will not
report its RAM at all — sets `slm_escalation_gguf` equal to the primary and
runs a single collapsed server instead of a second one.

The arithmetic is the reason. On an 8 GB machine (~7.4 GiB usable) a second
1.7B server would add 2,977 MiB to the 1,838 MiB already resident, and with one
convertd worker (550), the app and WebView2 (400) and Windows (2,500–3,000)
that is over the box. Collapsed, the same machine sits at ~1.8 GiB of naming
lane and has room for the rest of its work. Rung 3 still runs there — on a
wider evidence bundle against the server that is already up — so nothing is
disabled, it just does not pay for a second model.

Unknown RAM matches the smallest tier for the reason it always has: do not
gamble on behalf of a machine that will not say what it is.

What changed for this class on this branch is the context and the coverage: at
`6656 * 1` its single server carries 728 MiB of KV where it carried 448, about
280 MiB more, for a budgeted 1,838 MiB against the 1,123 MB measured at ctx
4,096. It also picks up the recycling change, which matters more here than
anywhere — this is the machine with the least room to absorb a server that
grows as it serves. The 0.6B itself was not re-measured at ctx 6,656; see
[Still pending](#still-pending).

### The 0.9.x baseline — measured, at ctx 4,096

Everything in this subsection is a real capture of the **0.6B/1.7B pair at
`--ctx-size` 4,096**, i.e. the shipped 0.9.x shape. It is kept because it is the
only measured memory data this project has, and because the calculated figures
above are anchored to it. **It does not describe the 1.7B/4B pair and does not
describe the current per-slot context.** At ctx 4,096 the 28-layer KV cost was
114,688 B/token x 4,096 = 448 MiB per parallel slot.

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
interpolates the 448 MiB/slot formula onto the 1.7B's own parallel-1 baseline
(2,262 + 448 ≈ 2,710 MB) rather than a fresh capture, because under 0.9.x
`slm_escalation_parallel` defaulted to 2 only above 9 GiB of installed RAM and
the build machine that produces this table is the one machine that can never
land on the 8 GB branch either. It never was measured, and it is now moot:
`slm_escalation_parallel_for_ram` returns 1 on every tier.

**Both tiers can be resident at once**, in separate `SlmLane` slots, from the
first document that needs a third naming attempt until the escalation server is
reaped for idleness. So the number to budget is the pair — for the 0.9.x pair,
measured:

| Both tiers (0.6B + 1.7B, ctx 4,096) | Working set | Private commit |
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
| convertd (worker + PyInstaller bootstrap stub) | 204 MB[^convertd-rss] |
| **total model + sidecar layer** | **4,396 MB** |

[^convertd-rss]: This 204 MB is a snapshot of a worker that has only run
    MarkItDown conversions. It is not the number `convert_workers_ram_ceiling`
    plans against: `convertd.py`'s loaders memoize each lazily-loaded
    component per-process with no unload path, so a worker that has ever
    serviced an `ocr` op or a `langid` op (effectively every document, once a
    batch runs long enough) keeps RapidOCR and lingua resident for the rest of
    its life. Measured with both loaded: 450–530 MB, which is the figure
    `config.rs`'s `CONVERTD_WORKER_RSS_MB` (550) budgets against. A
    long-running pool converges toward that worse number, not this one.

That 4,396 MB is a machine holding two distinct servers. An 8 GB machine does
not: its escalation tier is collapsed onto the primary, so it runs one
llama-server, and the number that applies to it is the single-server row.

### Reading any of these numbers

`convertd` legitimately appears twice in Task Manager. PyInstaller's one-file
build starts a small bootstrap stub which unpacks to `%TEMP%` and re-execs the
real interpreter as a child, so two processes is one sidecar, not a leak.

Working set includes the memory-mapped GGUF pages, which Windows can evict under
pressure; private commit is the part that must actually fit. That is also why
the failure mode here degrades rather than dies: the largest single contributor
is memory-mapped GGUF pages, which Windows evicts at the cost of speed, not
correctness. On an 8 GB machine with Windows itself using 2.5–3 GB,
`slm_parallel: 4` overcommits and thrashes — which is not a crash, it is every
document getting slower for the whole batch. Do not run a 1,000-file backfill
and a video call on the same 8 GB laptop.

`slm_parallel` costs no naming quality. Per-slot context is 6,656 tokens
regardless of the setting (total is `6656 x n` shared across `n` slots), so the
evidence bundle has exactly as much room at 1 as at 2. What you lose is
cross-file overlap in the naming stage, which is rarely the bottleneck — see
below.

### Defaults are now chosen from installed RAM

`config.rs` reads total physical memory once and lets it choose the primary
model, the escalation model and both slot counts — `default_primary_gguf_for_ram`,
`default_escalation_gguf_for_ram`, `slm_parallel_for_ram`,
`slm_escalation_parallel_for_ram`:

| Installed RAM | primary | `slm_parallel` | escalation | `slm_escalation_parallel` | naming lane, both up |
|---|---|---|---|---|---|
| <= 9 GiB | Qwen3-0.6B-Q8_0 | 1 | collapsed onto the primary | 1 | ~1,838 MiB (budgeted) |
| <= 17 GiB | Qwen3-0.6B-Q8_0 | 1 | Qwen3-1.7B-Q8_0 | 1 | ~4,815 MiB (1,838 primary-only) |
| > 17 GiB | Qwen3-0.6B-Q8_0 | 2 | Qwen3-1.7B-Q8_0 | 1 | ~5,543 MiB (2,566 primary-only) |
| unknown | Qwen3-0.6B-Q8_0 | 1 | collapsed onto the primary | 1 | ~1,838 MiB (budgeted) |

"Collapsed" means `slm_escalation_gguf` is set equal to `slm_primary_gguf` and
`SlmLane::escalation_collapsed()` takes over: the third rung still runs, on a
wider evidence bundle, against the server that is already up. Nothing is
disabled by the small tier — it just does not pay for a second model.

An explicit value in `backlog.config.json` always wins. These decide what a
fresh install writes; `apply_memory_ceilings` will additionally clamp a value
*down* on a machine that cannot hold it, never up.

## Throughput

> **Every figure in this section and in Naming quality predates the 1.7B/4B
> tier change.** They were measured on the 0.6B primary / 1.7B escalation pair,
> `--ctx-size` 4,096, `evidence_token_budget` 1,500. They are kept because they
> are real and because they are the baseline the new pair is compared against —
> not because they describe what ships now. For current figures see
> [The coverage fix](#the-coverage-fix): **20.03 s/file** on the shipped
> configuration, at `slm_parallel: 1`, `convert_workers: 3`.
>
> **These older figures may have been measured with the semantic evidence lane
> silently off**, which is worth 58–82% of wall clock on its own — see
> [The semantic lane costs more than the model tier
> does](#what-the-semantic-lane-itself-costs). There is no
> way to tell after the fact, because the failure leaves no trace in the run
> output. They are annotated rather than rewritten for that reason: an
> unreliable recorded number is more useful than a deleted one, provided it is
> labelled.

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

> **This is the 0.4.3 baseline on the 0.6B/1.7B pair, and it is scored on a
> deliberately hard 40-document sample.** It is the most thoroughly scored
> naming measurement this project has — party accuracy, date provenance,
> fixture-shape breakdown — and it is the standard the
> [model-tier A/B](#the-rejected-model-tier-swap) does *not* meet: that
> comparison used the general 33-file corpus at n=26 and found no difference
> either way.
>
> It also predates the discovery that the semantic evidence lane can be
> silently off, so which state it was measured in is unknown.

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
from six in 0.4.2. All four are `dated_deep`, the harvest-window limit of
`docs/KNOWN_ISSUES.md` item 4.

**"Named" is still not the same as "named well."** `SUBJECT_TRUNCATED` fired on
**35 of 40**, up from 10, because the checker's word trim is now the thing doing the
shortening instead of the schema severing the answer mid-word. That is the intended
direction — a flagged trim at a word boundary beats a silent cut mid-word — but it
means the model's raw subject needs repair on seven documents in eight, and the flag
has stopped distinguishing anything. Recorded as `docs/KNOWN_ISSUES.md` item 5: noise, not a defect in the
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
also means the harvest-window ceiling of `docs/KNOWN_ISSUES.md` item 4 is a smaller
problem than 2-of-8 suggested — though the intermediate prompt reached 6 of 8, so
how much of that ceiling is the window and how much is the model is still open.

**Two parties are still absent**, and the fix cost 19% throughput — see the
Throughput table.

Run-to-run variance is worth knowing before reading too much into any single
number here: the same 12 documents have produced 2, 4 and 7 successes across
runs. llama.cpp's slot assignment and batching shift the numerics even at
`temperature: 0`. Compare configurations on tens of documents, not twelve.

## The coverage fix

**This is the change on this branch that paid.** `SEMANTIC_TOP_K` was a
hardcoded 12 with no relationship to `evidence_token_budget`, and the semantic
lane got 35% of the character budget. `semantic_top_k(char_budget)` now derives
the count from the budget and the lane's share is 60%.

Both numbers had to move together, which is why the ceiling survived so long:
12 paragraphs rendered at ~350 characters each want ~4,200 characters against a
3,500-character lane, so raising either one alone changed nothing measurable.
Whichever you tried first looked like it did not matter.

Measured, same corpus, same models, `slm_parallel: 1`, `convert_workers: 3`:

| | before | after |
|---|---|---|
| bundle size | 6,526–7,652 chars | **9,277–10,000 chars** |
| ...as a share of the budget | 65–76% | **92–100%** |
| ranked paragraphs, shipped budget | 12 | **17** |
| ranked paragraphs, rung-3 budget | 12 | **27** |
| **wall clock** | 24.69 s/file | **20.03 s/file** |
| **per 1,000 files** | 6.9 h | **5.6 h** |

**19% faster and substantially wider at the same time.** The speedup is not a
paradox: the lane was spending effort assembling and then truncating an
oversized selection, and a budget the selection actually fits wastes less.

**20.03 s/file / 5.6 hours per 1,000 is the current headline figure** for the
shipped configuration — `slm_parallel: 1`, `convert_workers: 3`, Qwen3-0.6B
primary, coverage at its raised setting. It is faster than anything previously
recorded in this document. The configuration is part of the number.

It also means `evidence_token_budget` does something for the first time. Before
this, raising it widened the lane's share of a budget the lane could not fill —
a verbosity lever. It is now a coverage lever. See `docs/KNOWN_ISSUES.md`
item 16.

**What it did not do is improve naming quality on this corpus** — see the 2x2
below, where the coverage rows move wall clock and essentially nothing else.
`date_source` was 29/30 in all four configurations. That is a saturated corpus,
not a null effect, and it is the reason a stratified hard corpus is now the
blocking next step.

## The rejected model-tier swap

The question the A/B existed to answer: should the naming lane move from the
Qwen3-0.6B/1.7B pair to a Qwen3-1.7B/4B-Q4_K_M pair? **No.**

Same corpus, same code, same `--ctx-size` and evidence budget, `slm_parallel: 1`,
`convert_workers: 3`, semantic evidence lane confirmed live in every arm. Both
model pairs measured at both coverage settings, which separates the two changes
from each other:

| config | s/file | h/1000 | FAITHFUL | NAMES PARTY | TOP ENTITY |
|---|---|---|---|---|---|
| 0.6B/1.7B, top_k 12 | 24.69 | 6.9 | 24/25 | 25/26 | 19/26 |
| **0.6B/1.7B, top_k 17** | **20.03** | **5.6** | 23/25 | 25/26 | 19/26 |
| 1.7B/4B, top_k 12 | 38.47 | 10.7 | 24/24 | 24/26 | 16/26 |
| 1.7B/4B, top_k 17 | 40.11 | 11.1 | 25/25 | 25/26 | 13/26 |

**At matched coverage — the shipped `top_k 17` row against its 1.7B/4B
counterpart — the swap costs 2.0x the wall clock**: 40.11 against 20.03 s/file.
Read the two coverage settings as a pair and the shape is clear: coverage made
the small pair faster and left the large pair slightly slower, so the gap
widened rather than closed.

**Rejected on quality-per-second, not on memory.** The 1.7B primary fits the
16 GB class comfortably — that was checked first and was never the objection.
It is simply 2x the wall clock for a difference that does not survive the
sample.

### Quality: no demonstrated difference

**Do not read either pair as better.** At matched coverage the larger pair is
+2 on faithfulness, level on party, and −6 on the top-entity proxy. This
document's own variance warning applies directly:

> the same 12 documents have produced 2, 4 and 7 successes across runs.
> llama.cpp's slot assignment and batching shift the numerics even at
> `temperature: 0`. Compare configurations on tens of documents, not twelve.

Twenty-six is barely past that floor, and this corpus is **not stratified for
difficulty** — it is the general v0.9.0 corpus, not the deliberately-hard
40-document sample the 0.4.x figures were scored on. `date_source` came out
29/30 in **all four** configurations, which is the clearest sign the corpus is
saturated: nothing can separate on a metric where everything already passes.

The honest statement is **"no difference detectable on this corpus"**, which is
not the same as "no difference exists".

### The one signal that did distinguish the models

Worth recording rather than burying, because it is what would justify
revisiting this with a corpus that can discriminate.

**Under raised coverage the 0.6B began fabricating by concatenation** where the
1.7B stayed 100% faithful. The failure looks like this:

```
'Termination of Employment - Blue Harbor Systems Ironquill Corporation (Ironquill Corporation)'
```

Two real parties from the document, welded into one subject that names neither
correctly. It is the faithfulness column moving 24/25 → 23/25 on the small pair
while the large pair went 24/24 → 25/25.

**Two documents is not enough to buy 2x wall clock.** But the direction is the
one a capacity argument predicts — more evidence in front of a smaller model
producing more confident nonsense — and it is exactly what a stratified hard
corpus would resolve. If the swap is ever revisited, this is the thread to pull,
not the aggregate scores.

### Why the first two runs were void


Two end-to-end batches were run on 2026-08-06 and **both are withdrawn.** They
are described here rather than deleted, because the failure mode is silent and
will recur.

Both ran against `%LOCALAPPDATA%\BackLog\convertd.exe` — a PyInstaller
**onefile** build from 23 July reporting version 0.2.0, sitting beside the
current **onedir** build. It predates the semantic evidence pipeline and
answers `unknown op` to both `rank_paragraphs` and `extract_entities`.
`filter.rs` folds that into `available: false` and falls back to a
deterministic paragraph slice, so **the batch completed, reported 30/33 named
and entirely healthy totals, and every naming figure described the fallback
path rather than the product.**

The evidence cache from those runs is unambiguous in hindsight:

| | void runs (stale 0.2.0 sidecar) | correct runs (0.3.0 sidecar) |
|---|---|---|
| `routing` | `semantic_unavailable` | `semantic_ranked` |
| `semantic_available` | false | true |
| `entity_available` | false | true |
| bundle size | pinned near 6,000 chars from a 27,366-char source | 6,526–7,652 chars (65–76% of the 10,000-char budget) |
| paragraphs selected | 12 of 95 | ranked |

Nothing in the run output said "degraded". The totals looked healthy because
they *were* healthy — the pipeline did exactly what it is designed to do when
an optional capability is missing, which is carry on. See
`docs/KNOWN_ISSUES.md` item 15; `e2e_real_batch` now probes `rank_paragraphs`
at startup and panics with the onedir/onefile explanation rather than
producing another set of plausible numbers.

### What the semantic lane itself costs

Measured while establishing the above, and worth recording on its own:

| pair | semantic lane off | semantic lane on | cost |
|---|---|---|---|
| 0.6B/1.7B | 13.60 s/file | 24.69 s/file | **+82%** |
| 1.7B/4B | 24.31 s/file | 38.47 s/file | **+58%** |

That is the same order of magnitude as the model tier itself, which is not
obvious before you measure it: the evidence pipeline is not a cheap preamble to
the expensive part.

It is also the reason for the caveat at the top of this document. Any
throughput figure in this repository from before 2026-08-07 may have been taken
with this lane silently off, and there is no way to tell after the fact — the
failure leaves no trace in the run output. The historical tables below are
annotated rather than rewritten for that reason.

Note that these figures predate [the coverage fix](#the-coverage-fix), which
moved the shipped pair to 20.03 s/file *with* the lane on. Wider coverage and
lower wall clock at once.

### Per-request timing, the rejected pair

From the direct model benchmark rather than a batch. Retained because it is the
only per-request data this project has and because it is what the rejected
swap's wall-clock cost decomposes into.

| tier | rung | prompt-eval tok/s | decode tok/s | wall median | wall max |
|---|---|---|---|---|---|
| Qwen3-1.7B-Q8_0 | 1–2 | 89.6 | 10.25 | 35.1 s | 41.6 s |
| Qwen3-1.7B-Q8_0 | 3 | 82.9 | 8.11 | 55.3 s | 56.3 s |
| Qwen3-4B-Q4_K_M | 1–2 | 58.4 | 5.60 | 57.6 s | 62.8 s |
| Qwen3-4B-Q4_K_M | 3 | 57.2 | 5.45 | 80.5 s | 84.1 s |

**The worst single naming request was 84.1 s against a
`per_file_wall_clock_secs` of 180**, so that default has real margin rather than
being a guess, and the 300 s `NAMING_HTTP_TIMEOUT` sits above it. Warm model
load: 3.5–3.6 s.

A document named on the first attempt costs one 1.7B request. A document that
escalates all the way costs two 1.7B requests plus one 4B rung-3 request:
35.1 + 35.1 + 80.5 ≈ 151 s at the medians.

### Context and tokenizer

- **Real chars-per-token on BackLog's own evidence text: 4.71 median** on rungs
  1–2, 4.74 on rung 3. So the codebase's chars/4 budget unit is mildly
  conservative and the 3.0 chars/token slot reserve is *very* conservative —
  deliberately, because OCR output and dense tables tokenize worse than this
  corpus does.
- Prompt tokens actually sent: median 2,456 (rungs 1–2), 3,706 (rung 3), max
  3,916.
- **Worst observed context use was 60.1% of the 6,656-token slot.** The
  headroom exists for pathological input, not as waste — and the fact that it
  was never approached is the evidence that `filter::max_bundle_chars` is not
  silently truncating anything on this corpus.

### Why the page window did not move

`max_head_pages` (10) and `max_tail_pages` (3) are **deliberately unchanged**
even though the evidence budget rose from 1,500 tokens to 2,500. This gets
re-litigated every time the budget moves, so the argument is recorded here and
in a doc comment on the fields themselves.

`convertd.py::_truncate_pdf_markdown` engages only when a document exceeds
`head + tail` pages **and** 40,000 characters of extracted markdown. Above that
threshold the character budget binds long before the page window does: a
20-page document keeps 65% of its text, and the filter then selects roughly
10,000 characters (2,500 tokens x 4) out of what survives. So widening the
window adds *candidates*, not evidence — more mid-document text competing for a
budget that did not change, against identifying signal which in real documents
is front-loaded. Wider is not more informative here; it is more dilute.

**The honest caveat: this is reasoning, not measurement.** No document in the
v0.9.0 corpus clears both thresholds, so nothing in the run above exercised the
truncation path at all. Tuning these two properly needs a corpus of long
text-layer PDFs, which does not exist yet — see [Still pending](#still-pending).

**And there is a tighter ceiling upstream of this one.** `SEMANTIC_TOP_K` caps
the semantic lane at 12 paragraphs regardless of any budget
(`docs/KNOWN_ISSUES.md` item 16), so on a long document the page window is not
what is binding — the paragraph count is. Widening the pages first would be
tuning the looser constraint.

### A negative result worth not re-testing

**`--batch-size` does not affect this footprint.** Measured at 2048 / 1024 /
512 with `n_ubatch` 512, at-rest memory was identical (5,068 MiB working set,
2,842 MiB private) and prompt-eval was flat at 58–59 tok/s. The tempting
1,244 MiB logits-buffer theory — Qwen3's 151,936 vocab x `n_batch` x 4 B — is
**wrong**: llama.cpp sizes that buffer from actual need, not from `n_batch`.
Dropping `n_ubatch` to 256 saved 44 MiB and cost 4 tok/s of prefill. The
defaults stay.

### Still pending

The coverage fix is measured and shipped. The model swap is measured and
rejected. What is left, in the order it would change a decision:

| What | Status |
|---|---|
| **A stratified hard corpus** | **Blocking, and the reason every quality comparison here is null rather than conclusive.** The 33-file corpus is a general one and it is saturated: `date_source` came out 29/30 in all four measured configurations, so nothing can separate on it. Comparisons ran at n=26 with differences of one to three documents, below the floor this document's own variance warning sets. A sample weighted toward the failures that motivate any of this work — party absent, date deep in a long document — is the prerequisite for the next honest answer about naming quality. Nothing else on this list matters as much. |
| The concatenation failure, on a corpus that can see it | **Pending.** Under raised coverage the 0.6B fabricated by welding two real parties into one subject where the 1.7B did not — two documents, described under [the rejected swap](#the-one-signal-that-did-distinguish-the-models). It is the only signal that separated the models, and it is the specific thing a hard corpus should be built to measure. |
| The 0.6B at ctx 6,656 | Not re-measured. The 8 GB tier's 1,838 MiB is budgeted from the shape table, not observed. |
| An end-to-end exercise of the escalation tier | **Pending.** How often rung 3 starts a second server on a normal corpus is not established — the earlier claim that it never did came from a void run and is withdrawn. |
| `max_head_pages` / `max_tail_pages` against long documents | **Blocked on the same corpus.** No v0.9.0 fixture clears both truncation thresholds, so the page window has never been exercised. Note the coverage fix moved the tighter ceiling that sat upstream of it, so this is now the next one worth looking at rather than the second-order one. |
| KV-cache quantization (`--cache-type-k/v`) | Measured as an experiment only, not wired in. Out of scope. |

## Reproducing this

`pipeline.rs`'s `e2e_real_batch` is an `#[ignore]`d load harness that drives the
real sidecars and real weights against real folders. It is not one of the five
gates and never runs in `cargo test`.

```powershell
$env:BACKLOG_E2E_PROCESSING = "C:\...\Processing"
$env:BACKLOG_E2E_OUTBOX     = "C:\...\Outbox"
$env:BACKLOG_E2E_QUARANTINE = "C:\...\Quarantine"
$env:BACKLOG_E2E_CONVERTD   = "$env:LOCALAPPDATA\BackLog\convertd\convertd.exe"
$env:BACKLOG_E2E_LLAMA      = "$env:LOCALAPPDATA\BackLog\llama-server.exe"
$env:BACKLOG_E2E_PRIMARY    = "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-0.6B-Q8_0.gguf"
$env:BACKLOG_E2E_ESCALATION = "$env:APPDATA\ai.sonomos.backlog\models\Qwen3-1.7B-Q8_0.gguf"
$env:BACKLOG_E2E_PARALLEL   = "1"
$env:BACKLOG_E2E_WORKERS    = "1"
cd src-tauri
cargo test -p backlog --lib e2e_real_batch -- --ignored --nocapture
```

Those two paths are the shipped pair. Note the `convertd` path: it must be the
**onedir** build, not a stale `convertd.exe` sitting beside it — a sidecar that
cannot answer `rank_paragraphs` silently disables the semantic evidence lane
and every number the run produces describes the fallback path instead
(`docs/KNOWN_ISSUES.md` item 15). `e2e_real_batch` now probes for this at
startup, so it fails loudly rather than plausibly.

The per-slot context is 6,656 regardless of which models are pointed at, so
these runs will not reproduce the 0.9.x 448 MiB/slot figures without also
rebuilding an older tree.

Omit `BACKLOG_E2E_ESCALATION` to measure the primary-only shape. It prints a
flag-reason histogram, the ten slowest documents, the auto-named percentage and
a 1,000-file extrapolation, and asserts the invariants that matter: one manifest
per file, every manifest `ok` or `flagged`, quarantine holding exactly the
flagged ones, and Processing holding exactly the `ok` ones.
