# Sortition: Local Document Naming and Indexing Pipeline
**Design doc, v1. 2026-07-21.**

Working name "Sortition" (drawing lots to assign names; rename it whatever you want). One Tauri app does all the thinking. Power Automate does dumb I/O at the edges. Nothing leaves the device except a manifest of results.

---

## 0. Decisions up front

| Question | Decision | Why |
|---|---|---|
| Shell | Tauri 2, Rust core, single window | Lightweight utility, no terminals, small binary. Right tool here even though Locke went Electron; this is a single-purpose appliance, not a platform. |
| Conversion (native text) | MarkItDown via a frozen Python sidecar | Best coverage for docx/pptx/xlsx/html/eml in one tool. Not worth rewriting in Rust for v1. |
| Conversion (scanned/image) | RapidOCR (PP-OCRv4 ONNX) in-process via `ort`, NOT MobileNetV3 | MobileNetV3 is an image *classifier*. It cannot read text. See §4. |
| OCR fallback | LFM2.5-VL-450M-Extract on the page image | When OCR confidence is garbage, a small VLM reads the page directly. |
| Evidence filter | Deterministic harvest → fastText langID → GLiClass doc-type → embedding salience (granite-small-r2) | See §5. Most of the listed models get cut. |
| SLM (primary) | LFM2.5-350M, llama.cpp, GBNF grammar-constrained JSON | Liquid explicitly recommends it for data extraction and structured output; ~300 tok/s on CPU; 125K context you'll never need. |
| SLM (escalation) | LFM2.5-1.2B-Instruct (or Qwen3-1.7B if you want a non-Liquid hedge) | Same runtime, same grammar, swap the weights. |
| Deterministic checker | **In the app, in Rust, unit-tested.** Not in Power Automate. | See §2. This is the one place I'm overruling your spec. |
| Filename authorship | SLM emits *fields* (date, subject, description). The app composes and sanitizes the filename. | Never let a language model write a string that hits a filesystem. |
| Job ledger | SQLite (rusqlite), content-hash keyed | Crash-resume, idempotency, dedup, and your audit trail for free. |
| PA's entire job | Move files in; on manifest: rename, copy to archive, insert list row, log flags | Two flows, near-zero logic. |
| Cloud anything | None. All inference, conversion, and validation is on-device. | The only cloud surfaces are SharePoint itself and (optionally) the PA flows. No escalation tier, no telemetry with content, ever. |

---

## 1. End-to-end flow

```
SharePoint /Intake
      │  (PA Flow 1: move to synced /Processing folder)
      ▼
OneDrive-synced local folder  ←── app watches (notify crate + debounce)
      ▼
┌─────────────────────────── TAURI APP ───────────────────────────┐
│ 1. Ingest      hash (SHA-256), sniff type (magic bytes), ledger │
│ 2. Route       native-text? scanned? image? junk? encrypted?    │
│ 3. Convert     MarkItDown ──or── rasterize → RapidOCR → md      │
│ 4. Filter      harvest → langID → doc-type → salience → ≤2K tok │
│ 5. Name        LFM2.5-350M, grammar-locked JSON                 │
│ 6. Validate    deterministic checker (date, subject, desc)      │
│ 7. Retry       ladder per §7, else quarantine + flag            │
│ 8. Emit        manifest JSON per file to /Outbox/_manifests     │
└─────────────────────────────────────────────────────────────────┘
      ▼
OneDrive sync → SharePoint /Outbox/_manifests
      │  (PA Flow 2: per manifest → rename, copy, index row)
      ▼
SharePoint index list + /Archive + /NeedsReview
```

The correlation key everywhere is the file's SHA-256, not its name. Names change; hashes don't. This kills duplicate list rows, re-run double-processing, and sync-race confusion in one move.

---

## 2. Trust boundary: why the checker lives in the app, not Power Automate

You asked for PA to run the deterministic checker alongside the rename. Don't. Three reasons:

1. **The checker gates the retry loop.** Validation failure has to trigger "re-prompt the SLM with variation" (§7). That loop lives in the app. If PA does validation, a failure means a round trip through SharePoint sync just to tell the app to try again. That's a distributed system where a function call would do.
2. **PA expressions are write-only code.** A date validator plus filename sanitizer plus sentence checker in WDL expressions is untestable and will rot. In Rust it's 200 lines with a test suite.
3. **Validation is your quality product.** Everything deterministic should be in the artifact you control, version, and can show an auditor.

Compromise that costs nothing: PA Flow 2 keeps a single trivial condition, `filename matches ^\d{4}-\d{2}-\d{2} .+$`, as a belt-and-suspenders gate before touching SharePoint. If that ever fires, it means the app shipped a bad manifest and you want to know.

---

## 3. Ingest and routing

- **Type detection by magic bytes** (`infer` crate), never extension. Your intake will contain `.pdf` files that are actually TIFFs and `.doc` files that are RTF. Guaranteed.
- **PDF text-layer test:** open with `pdfium-render`, extract text per page. If median extractable chars/page < ~200 across sampled pages, route to the scanned path. Hybrid PDFs (text pages + scanned exhibits) go native path; the evidence filter only needs the front matter anyway.
- **Special routes:**
  - Password-protected / DRM → flag immediately, reason `ENCRYPTED`, no retry (retrying can't fix a password).
  - Zero-byte, corrupt header → flag, reason `CORRUPT`, one retry (transient sync artifacts do resolve).
  - > 500 pages → process first 10 + last 3 pages only (naming never needs page 247).
  - Suspected multi-document scan packet (multiple date/letterhead resets detected in OCR) → process, but flag `POSSIBLE_MULTIDOC` so a human can split it. Auto-splitting is a v2 feature, not a v1 promise.

---

## 4. Conversion

**Native path:** MarkItDown, run as a PyInstaller-frozen sidecar speaking JSON over stdin/stdout (Tauri sidecar API; no terminal window, `CREATE_NO_WINDOW` on Windows). One warm process, files streamed through it. Covers docx, pptx, xlsx, html, eml, csv, and text-layer PDFs.

**Scanned path:** rasterize needed pages at 300 DPI (pdfium), then RapidOCR (PP-OCRv4 det+rec ONNX models, ~15 MB total) through the `ort` crate, in-process, CPU. Emit markdown with page markers and per-line confidence.

**On MobileNetV3:** it has to come out of the extraction slot. It's an ImageNet classifier; feeding its output ("envelope, 0.62") to MarkItDown produces nothing. The one honest job it could do (triage pages as text/photo/blank) is done cheaper by OCR confidence plus ink-density heuristics, so cut it entirely.

**OCR fallback:** if mean OCR confidence < 0.55 or output is < 50 chars on a visibly non-blank page, send the page *image* to LFM2.5-VL-450M-Extract (Liquid's vision extract model, built for exactly this) and take its transcription. This catches faxes, skewed scans, and handwriting-adjacent garbage that classical OCR mangles. Both attempts fail → flag `UNREADABLE`.

---

## 5. Evidence filter

Goal: any document → ≤ ~1,500 tokens of high-signal evidence, biased toward exactly what the SLM needs (a date, a document type, parties, a subject). Stages run cheap to expensive:

**5a → 5b → 5c → 5d** as below, plus **5e (v1): the fine-tuned Ettin-32M token classifier** proposing `{DATE, PARTY, SUBJECT}` spans that get pinned to the top of the evidence bundle and cross-checked against the SLM's answer (§6, §7). Bootstrap training plan is in the verdict table and §11.

**5a. Deterministic harvest (regex + metadata, free).** Runs first because for most business documents it alone is sufficient:
- All date strings in the first 2 and last 1 pages, normalized (`chrono` + a format table: `July 20, 2026`, `07/20/26`, `2026-07-20`, `20 Jul 2026`), each tagged with position.
- `RE:` / `Subject:` / `In re:` lines, email headers, caption blocks (`v.`, case numbers), heading lines (markdown `#`, ALL-CAPS lines, title-case short lines).
- First ~40 lines of page 1, signature block region of last page.
- File metadata: created/modified dates, docx/pdf title and author properties (tagged as low-trust; scanner defaults lie).

**5b. Language gate: fastText `lid.176`** (< 1 MB, microseconds). Non-English → set OCR language, use it in the SLM prompt, and tag the manifest. Cheap insurance, keep it.

**5c. Doc-type classification: GLiClass base v3.0**, zero-shot against your label set (termination notice, engagement letter, NDA, invoice, complaint, motion, deposition transcript, corporate resolution, correspondence, memo, policy, ~30 labels). Run it on the *harvested evidence*, not the full document. The label does two jobs: it templates the subject ("Termination Notice for {party}") and it selects probe queries for 5d. Zero-shot means you edit the taxonomy in a config file, no retraining.

**5d. Salience ranking: granite-embedding-small-english-r2** (47M, ONNX, in-process). Only fires when 5a came back thin (body text with no obvious subject line). Sentence-split the body, embed, score against type-specific probe queries ("effective date of termination", "parties to this agreement") plus centroid similarity, take top 12 sentences in document order.

**Verdicts on the rest of your list:**

| Model | Verdict | Reason |
|---|---|---|
| SPLADE v2 distil | **Cut** (revisit if you build corpus search) | Sparse expansion earns its keep in a search index. For one-shot extractive filtering, BM25 against probe terms gets ~90% of the value deterministically, and granite covers the semantic remainder. |
| legal-bert-small | **Cut** | 2020-era, and GLiClass zero-shot beats a fine-tune you'd have to build. Redundant. |
| ModernBERT-base | **Cut at runtime** | 149M of overlap with granite-small. Keep on the shelf as a fine-tune base if you outgrow Ettin. |
| Ettin encoder 32M / token classifier | **In v1, via a bootstrap fine-tune** (raw checkpoint is a blank encoder; it extracts nothing until trained) | Fine-tune it as a token classifier over `{DATE, PARTY, SUBJECT}` spans. Training data is silver-labeled from your own pipeline before launch: regex-anchored date spans are exact and free, and a shadow run of slices 2–3 over ~2–5K real intake files yields subject/party spans by aligning accepted SLM outputs back to the source text. Fine-tuning a 32M encoder on that is a laptop-scale job. **v1 role:** span *proposer*, not decider. Its spans get injected at the top of the evidence bundle, and it doubles as a consistency check (Ettin's date span vs. the SLM's chosen date; disagreement triggers a retry per §7). Human corrections from the review pane keep improving it; promoting it to primary namer with the SLM as fallback is the phase-2 flywheel, gated on measured agreement rates, not vibes. |
| Rampart | **Cut from the pipeline; keep on your reading list** | 14.7 MB PII token-classifier for redacting text before it leaves a device. In a pipeline where nothing leaves the device, it has no job. The one runtime role it *could* play (blocking PII from landing in filenames/descriptions in the SharePoint index) is already covered by the checker's regex layer in §6, and Spliicer is the on-brand tool if you ever want model-based coverage there. Separately: study it anyway. A US government design studio shipping free client-side PII redaction with a published eval harness is squarely in Spliicer's category. |
| MobileNetV3 Small | **Cut** | §4. |
| fastText | **Keep** (langID only) | 5b. |
| MarkItDown | **Keep** | §4. |
| Deterministic metadata/regex | **Keep, promoted to first-class stage** | 5a and §6 are the backbone of the whole system. |

---

## 6. Naming and validation

**Prompting.** System prompt states the task, the doc-type label from 5c, the language, and today's date. User message is the evidence block. Decoding is locked with a llama.cpp GBNF grammar so the model *cannot* emit anything but:

```json
{"date": "YYYY-MM-DD", "date_source": "document|metadata|none",
 "subject": "3-8 words", "description": "one sentence"}
```

Grammar-constrained decoding eliminates the entire class of "model returned prose/markdown/apologies" failures before validation ever runs. Temperature 0 on first attempt.

**The app composes the filename:** `{date} {subject}.{original_ext}`, after sanitization. The model never touches path characters.

**Deterministic checker (Rust, exhaustive, boring):**
- JSON parses against schema (grammar nearly guarantees; check anyway).
- Date: real calendar date; within 1900-01-01..today+30d; and, critically, **must match a date found in 5a evidence or file metadata**. This is the anti-hallucination tripwire. `date_source: none` is allowed only via the fallback rule below.
- Undated documents (they exist: policies, org charts): fall back to file *modified* date and mark the manifest `date_source: metadata` so the index is honest about provenance.
- Subject: 3–8 words; strip/reject `\ / : * ? " < > | #` and leading/trailing dots and spaces; reject generic-only subjects ("Document", "Scan", "Untitled", "PDF"); no PII patterns you don't want in filenames (SSNs in a filename would be a fun look for a privacy company).
- Full filename ≤ 120 chars (SharePoint's real constraint is the ~400-char full URL; leave headroom for deep folder paths).
- Description: exactly one sentence (one terminal punctuation mark), 15–200 chars, no newlines, doesn't merely restate the filename.
- Collision handling: check the ledger and a cached copy of the index; append ` (2)`, ` (3)`.
- Cross-check: doc-type label from 5c should be consistent with the subject (a doc classified "invoice" whose subject says "deposition transcript" gets one retry, then a `TYPE_MISMATCH` soft flag on the manifest, processed but visible).

---

## 7. Retry ladder

Retries vary the input; identical retries are prayer, not engineering.

| Failure | Attempt 2 | Attempt 3 | Then |
|---|---|---|---|
| Conversion crash | Re-run once (transient) | Alternate extractor (pdfium text dump / VL-Extract) | Flag `CONVERT_FAIL` |
| OCR low-confidence | 400 DPI re-raster | LFM2.5-VL-450M-Extract on images | Flag `UNREADABLE` |
| SLM no/invalid output | Re-run temp 0, evidence trimmed to 5a only | Escalate to 1.2B, evidence budget ×2 | Flag `SLM_FAIL` |
| Validation reject | Re-prompt with the specific violation quoted ("date 2062-07-20 not present in document") | Escalate to 1.2B | Flag `VALIDATION_FAIL` |
| Ettin/SLM disagreement (e.g., Ettin's top DATE span ≠ SLM's date) | Re-prompt SLM with Ettin's spans pinned at top of evidence | Escalate to 1.2B | Soft flag `SPAN_MISMATCH` (processed, but surfaced) |
| Sidecar dead | Respawn, replay job from ledger | — | Flag `RUNTIME_FAIL` |

Hard cap: 3 attempts per stage, ~90 s wall-clock per file. Flagged files move to `/Quarantine` locally and get a manifest with `status: flagged`, machine-readable reason, and the last evidence bundle. PA Flow 2 routes those to a **Needs Review** SharePoint list instead of the index. The app's review pane shows the evidence next to editable name fields; a human fix re-emits a corrected manifest, and (phase 2) that correction is Ettin training data.

---

## 8. Power Automate: two flows, deliberately stupid

**Flow 1, Intake.** Trigger: file created in `/Intake`. Action: move to `/Processing` (the OneDrive-synced folder). Nothing else. Not even a condition.

**Flow 2, Commit.** Trigger: file created in `/Outbox/_manifests`. Steps:
1. Parse manifest JSON.
2. `status: ok` → sanity regex on filename (§2) → rename the original in `/Processing` (or move-with-rename to `/Archive`) → copy to the designated archive folder → **check index list for existing row with this SHA-256** (idempotency; PA retries duplicate triggers, sync hiccups re-fire them) → insert row: filename, description, hash, doc-type, date_source, processed timestamp, original name.
3. `status: flagged` → insert row in Needs Review list with reason; move original to `/NeedsReview` folder.
4. Any step fails → append to a `_pa_errors` list; never delete an original on a failed run.

PA throttling note: standard connectors rate-limit around 600 calls/min and burst-throttle well below that in practice. For a multi-thousand-file overnight run, either let manifests trickle (the app can pace emission, e.g. 10/min) or have Flow 2 process manifests in batches from a scheduled trigger every 5 minutes. Batch-scheduled is more robust; per-file trigger is simpler. Start per-file, switch if you see 429s.

**Sync race, the classic failure:** the manifest can sync before the renamed target file finishes syncing, or vice versa. Flow 2 must verify the referenced file exists (and, paranoid mode, that its hash matches) before inserting the index row, with a 2-retry delay. Never trust OneDrive sync ordering.

---

## 9. Parallelism (all local)

There is no cloud tier. Parallelism is worker pools inside the app:

- Conversion pool: `min(cores-2, 6)` workers. OCR rasterization dominates total wall-clock; this is where parallelism pays.
- Encoder lane: one ONNX session per model, batched inputs (granite and GLiClass batch beautifully).
- SLM lane: one llama.cpp server, `--parallel 4` continuous batching. At ~300 tok/s and ~100 output tokens, the 350M does ~2-3 files/sec sustained. It will never be your bottleneck; scanned-PDF OCR will.
- Escalation to the 1.2B loads lazily on first flagged retry and stays resident for the batch, so a bad stretch of files doesn't thrash model loads.
- Ballpark: 5,000 mixed files ≈ 2–6 hours on a decent laptop, dominated by scan ratio. Overnight run, progress bar, done.

Network posture: the app makes zero outbound calls at runtime. Model weights ship in the installer (or a one-time verified download on first launch, hash-pinned); after that it runs airgapped. Worth stating in the UI, since "your files never leave this machine" is the sales pitch and should be an observable property, not a promise.

---

## 10. Failure modes not yet covered

- **OneDrive sync stalls silently.** App shows sync-folder freshness; if `/Processing` intake goes quiet mid-batch, surface it instead of "done."
- **File locked by sync client during rename.** Retry with backoff; it clears in seconds.
- **Duplicate content, different files.** Same hash seen twice → second file gets the same name + ` (2)` and the index row notes `duplicate_of`. Legal intake is full of these.
- **Wrong date on the document itself** (typo'd year on a letter). Checker range-caps catch `2062`; it cannot catch `2025` typed for `2026`. Accept this; document dates are ground truth by policy, and the review pane exists.
- **SharePoint list at 5,000-item view threshold.** The list keeps working past 5K but views need indexed columns (index the hash and date columns now, not later).
- **Model/tokenizer drift.** Pin every model file by SHA in the app bundle; ledger records model versions per file so any name is reproducible.
- **Crash mid-batch.** Ledger states (`ingested → converted → filtered → named → validated → emitted`) mean restart resumes exactly where it died. No file processed twice, none skipped.

---

## 11. Build order (six slices, Orchestra-style)

1. **Skeleton + ledger:** Tauri shell, folder watcher, SQLite ledger, hash/route, manifest emission with stub names. PA Flow 2 against stub manifests. *End-to-end plumbing proven before any ML exists.*
2. **Native conversion + deterministic-only naming:** MarkItDown sidecar, 5a harvest, checker, filename composition. With a <5% scan ratio, this slice already covers ~95% of intake volume mechanically; a surprising share gets correctly named with zero models. This is your quality floor and your benchmark baseline.
3. **SLM lane:** llama.cpp sidecar, GBNF grammar, LFM2.5-350M, retry ladder, 1.2B escalation weights.
4. **Ettin bootstrap:** shadow-run slices 2–3 over ~2–5K real intake files (no writes to SharePoint), silver-label spans (regex-exact dates; SLM subjects/parties aligned to source), fine-tune ettin-encoder-32m as the token classifier, integrate as evidence proposer + SLM consistency check. Ship with per-label F1 measured on a held-out slice; if PARTY or SUBJECT F1 is poor at launch, ship it date-only and widen labels as corrections accumulate.
5. **Scanned path:** pdfium raster, RapidOCR, VL-Extract fallback, confidence routing. Deliberately last: at <5% of intake this is ~100–250 files per few-thousand batch, and until it ships those files just land in quarantine with reason `SCANNED_PENDING`, which is an acceptable interim state rather than a blocker.
6. **Filter refinement + review UI:** GLiClass taxonomy, granite salience, quarantine pane, correction re-emit feeding the Ettin retraining set.

Ship slice 2 to real intake early. The deterministic baseline will tell you exactly how much the models are earning, which is the number every later decision depends on.
