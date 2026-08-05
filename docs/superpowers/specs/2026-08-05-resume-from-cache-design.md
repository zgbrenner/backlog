# Resume-from-cache — design doc (2026-08-05)

Status: designed, not implemented. Grounded against `pipeline.rs`
`process_inner` (~395-976), cache artifact code ({sha}.md +
{sha}.evidence.json; write_evidence_trace ~2336-2394; purge/sweep
~2302-2444, 2963-3007), and `ledger.rs` (JobState ladder, CAS semantics).

## 0. Scope decision (read first)

The two existing cache artifacts only support skipping **CONVERT**, not
FILTER. `filter::build_evidence` (`filter.rs:264-270`) needs
`doc_meta_dates: Vec<String>` as an input, and nothing persists it before
`Validated`. The evidence.json trace is a UI-facing explanation
(ranked_paragraphs/entities/lanes/compression), not the assembled
`Evidence.bundle`, `Harvest`, `ettin_spans`, or `meta_dates` the naming
ladder consumes. Reconstructing `Evidence` from it needs a schema change
materially bigger than this feature.

**Phase 1 (this design): skip CONVERT only** — reuse cached markdown, skip
the sidecar convert/OCR round-trip, run FILTER and NAME fresh. That is what
a kill-mid-name restart needs: by `Filtered`, the expensive stage (OCR) is
already paid and shouldn't be paid twice. Phase 2 (skip FILTER) is future
work, not designed here.

## 1. Key integrity + two real bugs found

`sha` is computed once at `pipeline.rs:422-428` from the original source
bytes, before convert touches anything; the cache write at ~726-728 stores
conversion OUTPUT under that immutable content key. No key/mutation skew.

Two real problems instead:

1. **Write ordering is backwards for crash-safety.**
   `advance(&sha, JobState::Converted)` runs at ~720-722, *then* the `.md`
   write at ~726-728. A crash between them leaves `state=converted` with no
   cache file — exactly the case resume most wants. Fix: write cache first,
   advance second (mirrors write_manifest-then-advance(Emitted) at
   ~940-967).
2. **The `.md` write is not atomic** — plain `std::fs::write`, no
   temp+rename, unlike `write_evidence_trace`'s temp → backup → rename.
   Give it the same treatment.

## 2. New artifact: `{sha}.convert-meta.json`

Two `ConvertResult` fields consumed downstream are persisted nowhere:
- `doc_meta_dates` → `Checker::check` (checker.rs:271-317;
  DATE_NOT_IN_EVIDENCE at :37,58). Dropping it on resume can turn a valid
  embedded-metadata date into a rejection that wasn't there on first pass —
  a correctness regression, not just degradation.
- `letterhead_resets` → POSSIBLE_MULTIDOC soft flag (~716-719).

`job.model_versions` is not usable as provenance: it's written right before
emit (~914); a job resumed at Converted/Filtered has NULL there.

```rust
const CONVERT_CACHE_SCHEMA_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
struct ConvertCacheMeta {
    schema_version: u8,
    route: String,              // "native" | "scanned" — must match routing this run
    max_head_pages: usize,
    max_tail_pages: usize,
    convertd_version: Option<String>, // model_versions["convertd"] at write time
    doc_meta_dates: Vec<String>,
    letterhead_resets: u32,
}
```
Written atomically (same pattern as write_evidence_trace) alongside
`{sha}.md`, BEFORE advance(Converted). convertd's convert/ocr ops don't
touch models.lock.json (that's classify/semantic, i.e. FILTER), so
`convertd_version` alone is the right provenance for a CONVERT-only gate.

## 3. Validity gate

| Knob | Why it invalidates |
|---|---|
| route (native/scanned) | different sidecar op entirely |
| `max_head_pages` / `max_tail_pages` | changes sampled pages |
| convertd version | upgraded extractor changes output |
| artifact schema version | forward-compat |

NOT gated in Phase 1 (FILTER-only knobs): `evidence_token_budget`,
`ettin_model_dir`/enabled, classify/semantic model versions.

```rust
/// None => cache miss; caller falls through to a normal convert. Never
/// returns Err and never flags — a miss is exactly as normal as a cold
/// cache.
fn try_resume_convert(&self, sha: &str, route: Route) -> Option<CachedConvert> {
    let md_path = self.cfg.cache_dir.join(format!("{sha}.md"));
    let markdown = std::fs::read_to_string(&md_path).ok()?;
    if markdown.trim().len() < 30 { return None; } // same floor as pipeline.rs:710

    let meta_path = self.cfg.cache_dir.join(format!("{sha}.convert-meta.json"));
    let meta: ConvertCacheMeta =
        serde_json::from_slice(&std::fs::read(&meta_path).ok()?).ok()?; // tolerant

    if meta.schema_version != CONVERT_CACHE_SCHEMA_VERSION { return None; }
    if meta.route != route_name(route) { return None; }
    if meta.max_head_pages != self.cfg.max_head_pages { return None; }
    if meta.max_tail_pages != self.cfg.max_tail_pages { return None; }
    if meta.convertd_version.as_deref()
        != self.model_versions.get("convertd").and_then(|v| v.as_str())
    { return None; }

    Some(CachedConvert { markdown, doc_meta_dates: meta.doc_meta_dates, letterhead_resets: meta.letterhead_resets })
}
```

## 4. Entry point

Branch after route is persisted (~689-691), before the convert stage marker:

```rust
let _ = self.ledger.mark_stage(&sha, "convert");
let conv: ConvertResult = if resume_state != JobState::Ingested {
    match self.try_resume_convert(&sha, route) {
        Some(cached) => {
            let _ = self.ledger.log_event(&sha, "convert", "cache hit: skipped convert/OCR");
            log::info!("convert cache hit for {sha}");
            ConvertResult {
                markdown: cached.markdown,
                doc_meta_dates: cached.doc_meta_dates,
                ocr_used: false, ocr_mean_conf: 0.0, encrypted: false,
                letterhead_resets: cached.letterhead_resets,
            }
        }
        None => { /* normal convert_with_retries under convert_slots */ }
    }
} else { /* normal path */ };
// downstream (encrypted check, len<30, letterhead, cache write BEFORE
// advance(Converted)) unchanged apart from the reordering.
```

Qualifying states: gate on `resume_state != JobState::Ingested` only. No
special-casing Flagged/Emitted/Dismissed — a stale gate read is cheap and
`advance()`'s CAS (`ledger.rs:133-146` transition_allowed) already refuses
resurrecting finished jobs, the same net that protects today's replay path.
`JobState::ladder_rank` is private — the design needs no ladder comparison.

**Routing still runs every time.** It's cheap, and its output is the field
the gate checks — recomputing it IS the correctness check.

## 5. WorkClock / retry-ladder interplay

- Cache hit never acquires `convert_slots` — no queue time charged, no
  convert budget consumed.
- `bump_stage_attempts` (~594) runs before this branch; corrupt-cache jobs
  that repeatedly fail full reprocess still trip CRASH_LOOP_LIMIT.
- `set_state` attempts-reset semantics (`ledger.rs:761-798`) unchanged —
  replaying advance(Converted) at higher resume rank doesn't reset, same as
  any restart today.
- `convert_with_retries`' local attempt loop simply never runs on a hit.

## 6. Corruption safety

Follow the tolerant-read idiom from `local_output.rs:633-668`
(`durable_transaction_exists`): every IO/parse failure collapses to `None`
→ full reprocess, indistinguishable from a cold miss. Do NOT reuse
`get_evidence`'s (lib.rs:895-903) error-surfacing semantics — that's a
human-facing review path. The `len < 30` re-check is a second,
content-shaped defense against a torn `.md`.

## 7. Metrics / visibility

- `log::info!("convert cache hit for {sha}")`.
- Ledger event `"cache hit: skipped convert/OCR"` at stage "convert".
- Recommended: `log::debug!` on miss naming the failed gate (schema/route/
  pages/version) — misses are normal, keep it below WARN.

## 8. Test plan

**Honest finding: there is no sidecar-call-counting stub today.** The
Harness points Sidecar at `no-such-convertd`; existing process_file tests
either fail before sidecar output matters or use the terminal-manifest
recovery shortcut (`recover_terminal_manifest` ~1588-1652; precedent test
`valid_existing_ok_manifest_recovers_ledger_without_model_work`
~5898-5934). `Sidecar::counter` (sidecar.rs:272) is private.

Additions:
(a) `#[cfg(test)] pub(crate) fn call_count(&self) -> u64` on Sidecar, and
(b) unit-test `try_resume_convert` as a pure function. Do both.

Matrix:
1. Gate unit tests: valid pair → Some with round-tripped fields; missing
   .md → None; missing meta → None; <30 chars → None; garbage meta → None,
   no panic; each knob individually mismatched → None; all matching → Some.
2. Integration "kill mid-name, resume skips convert": seed ledger at
   `Filtered` (realistic — `Named` is a few-instruction window, set only
   after a validated attempt at ~1193; add a narrow Named variant for
   completeness), hand-write matching cache artifacts, run process_file,
   assert the ledger event log contains "cache hit: skipped convert/OCR"
   and NO convert-stage "attempt N failed" event (that event only fires
   inside convert_with_retries — its absence proves the branch). Robust
   without a live sidecar.
3. Corrupt-cache fallback: garbage meta → "attempt 1 failed" IS present →
   flagged CONVERT_FAIL/UNREADABLE, not stuck, no panic.
4. Sweep interaction: add `.convert-meta.json` to `purge_cache_artifacts`
   (~2423-2438), `cache_artifact_sha` (~2440-2444, new strip_suffix arm),
   and `sweep_cache_with_ledger`'s artifact list (~2993-2996) — otherwise
   the new file leaks forever on resolved jobs.

## 9. Expected win — honest

Pays off only in restart-heavy scenarios (crash/kill/OS update mid-batch).
Does nothing for steady-state throughput, config-changed caches, FILTER
cost, or the pre-write crash window until the §1 ordering fix lands.
Highest-value case: skipping a Route::Scanned re-OCR (multi-DPI passes) on
restart; native re-convert is a smaller, still-real win.
