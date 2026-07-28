# Ettin bootstrap (optional, not shipped)

> ## Read this before you spend a week on it
>
> **The shipped application ignores whatever this directory produces.**
>
> The runtime span lane lives in `sidecar/convertd.py::op_ettin_spans`, and the
> shipped sidecar is the **slim, torch-free** profile: no `torch`, no
> `transformers`. The loader therefore always fails, the op always returns
> `ok=true` with `{"spans": []}` and `available: false`, and — because no
> degradation path is allowed to fail a document — **nothing reports an
> error**. Settings has an "Ettin model dir" field and the environment variable
> `BACKLOG_ETTIN_DIR` exists, but pointing either of them at a trained model
> changes nothing you can observe.
>
> Making this lane real requires a **torch-inclusive sidecar rebuild**: adding
> `torch` and `transformers` back to `sidecar/requirements.in`, re-freezing the
> lock, and rebuilding `convertd` with PyInstaller. That roughly triples the
> sidecar's Python footprint (torch alone is ~500 MB installed) and is a
> decision with a rationale on the other side — see
> `docs/DECISIONS.md` §3.
>
> **There is also a second, harder blocker:** `silver_label.py` opens the
> ledger with `sqlite3.connect`, and the ledger has been SQLCipher-encrypted
> since 0.2.0 (`src-tauri/src/dbkey.rs`). A plain `sqlite3` open of
> `ledger.db` now fails with `file is not a database`. Labeling needs a
> SQLCipher-capable driver plus the DPAPI-protected key, or an export path out
> of the app that does not exist yet. This is tracked in
> `docs/KNOWN_ISSUES.md`.
>
> Everything below is accurate about the *scripts*. It is not a supported path.

---

## What this is for

The span lane's job is a second, independent opinion on the document's date:
a small token-classification model that tags DATE / PARTY / SUBJECT spans in
the raw text. When it disagrees with the SLM's proposed date, the checker
attaches a `SPAN_MISMATCH:ettin=<date>` soft flag so the batch can be audited.
It is advisory — it never blocks a name and never proposes one.

The base model is [`jhu-clsp/ettin-encoder-32m`](https://huggingface.co/jhu-clsp/ettin-encoder-32m)
(MIT), a 32M-parameter encoder. It is *not* an extractor as shipped; it has to
be fine-tuned on your own corpus, which is what these two scripts do.

## The procedure

### 0. Prerequisites

```
python3.11 -m venv .venv-training
.venv-training/bin/pip install -r training/requirements.txt
```

A separate venv, deliberately: none of this may leak into
`sidecar/requirements.txt`.

### 1. Produce a corpus

Run a real batch (or a shadow batch with the Outbox pointed at an unsynced
local folder) of 2,000-5,000 files through the app, with
`"retain_cache": true` set in `backlog.config.json` so the converted markdown
survives emission. Without that flag the text is deleted the moment each file
is filed and there is nothing to label.

### 2. Build silver labels from the ledger + cached markdown

```
python silver_label.py \
  --ledger "%APPDATA%/ai.sonomos.backlog/ledger.db" \
  --cache  "%APPDATA%/ai.sonomos.backlog/cache" \
  --out data/
```

"Silver" because the labels are derived from what the pipeline already
accepted, not from human annotation: the accepted date, subject and description
are projected back onto the cached text as spans. That means the model learns
the pipeline's *current* behavior, including its mistakes — which is fine for a
consistency check and useless as ground truth.

**This step fails today.** See the SQLCipher blocker above.

Writes `data/train.jsonl`, `data/dev.jsonl` and `data/labels.json`.

### 3. Fine-tune

```
python train_ettin.py --data data/ --out ettin-backlog-v1
```

Defaults: base `jhu-clsp/ettin-encoder-32m`, 4 epochs, lr 5e-5, batch 16,
512-token windows, best checkpoint selected on `DATE_f1`.

### 4. The ship gate

Printed at the end of training, as per-label F1 on the held-out dev split:

- **DATE F1 ≥ 0.90** — required to enable the lane at all.
- **PARTY and SUBJECT F1 ≥ 0.75 each** — otherwise ship date-only.

The script **prints** the three numbers and the policy; it does not enforce
them and there is no "ship date-only" switch in it (the docstring's reference
to a `SHIP_LABELS` constant is stale — no such symbol exists). Reading the
numbers and deciding is a human step.

`docs/RELEASE_CHECKLIST.md` gates on this: the trained directory is disabled,
or its held-out metrics meet these thresholds.

### 5. Point the app at it

Settings → Ettin model dir, or `BACKLOG_ETTIN_DIR` for a frozen sidecar. Then
restart the pipeline.

**And nothing will happen**, for the reason at the top of this file.

## Data handling

The corpus is your own documents in plain text. `training/data/` and
`training/ettin-backlog-*/` are gitignored, and `docs/SECURITY.md` forbids real
customer documents in fixtures, logs, screenshots, release artifacts **or
training data**. A trained model memorises; treat the output directory with the
same care as the corpus.
