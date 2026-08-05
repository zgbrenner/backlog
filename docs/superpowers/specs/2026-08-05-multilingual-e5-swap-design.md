# Semantic model swap: MiniLM → multilingual-e5-small — migration plan (2026-08-05)

Status: designed, not implemented. Motivation: the pinned
`Xenova/all-MiniLM-L6-v2` is **English-only** while the corpus is
multilingual (en/es/fr/de observed live). Target:
`Xenova/multilingual-e5-small` (ONNX conversion of
`intfloat/multilingual-e5-small`, MIT, 117.7M params, hidden_size=384,
12 layers, vocab 250037).

## Real pins (fetched from the HF tree API directly — safe to commit)

| File | Bytes | SHA-256 |
|---|---|---|
| `onnx/model_quantized.onnx` | 118,308,185 | `f80102d3f2a1229f387d3c81909990d8945513e347b0eab049f7de3c6f98c193` |
| `tokenizer.json` | 17,082,730 | `0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39` |
| Pinned revision | — | `761b726dd34fb83930e26aab4e9ac3899aa1fa78` |

Payload: ~135.4 MB vs current ~23.2 MB (5.8×), driven by 12 layers vs 6 and
a 17 MB tokenizer.json (250K sentencepiece vocab vs 30K WordPiece).
**Embedding dimension stays 384** — no hardcoded dim assertions found in
`src-tauri/src/` (checked filter.rs, sidecar.rs).

License: Xenova repo untagged; derivative of intfloat's MIT model. NOTICE.md
should archive both cards and cite MIT (replacing Apache-2.0 language).

## The crux: tokenizer format change

`sidecar/semantic.py:219-287` is a hand-rolled pure-Python BERT WordPiece
tokenizer reading `vocab.txt`. e5's tokenizer is XLM-R sentencepiece Unigram —
not hand-rollable (NFKC, Unigram Viterbi, precompiled charsmap). Xenova ships
`tokenizer.json` (self-contained fast-tokenizer serialization), which is
sufficient alone (ignore `sentencepiece.bpe.model`).

**Decision: add the `tokenizers` PyPI package** — HF's standalone Rust-backed
library, no PyTorch/transformers, ~2-3 MB win_amd64/cp311 wheel; loads
`tokenizer.json` via `Tokenizer.from_file(path)` fully offline. Does not
violate the "no transformers/torch" constraint.

Special tokens change to `<s>`/`</s>`/`<pad>`/`<unk>` (cased model, no
lowercasing). Model inputs stay `input_ids`/`attention_mask`/`token_type_ids`
(`type_vocab_size: 2`) — `semantic.py:319`'s conditional needs no change.
Mean-pool + L2-normalize (`semantic.py:322-333`) matches e5's documented
`average_pool()` exactly — no change.

**Prefix requirement (verified from model card):** every input starts with
`query: ` or `passage: `, even non-English. Design: `rank_paragraphs` —
paragraphs `passage: `, probes `query: `. `extract_entities` — e5 FAQ
recommends `query: ` on both sides for classification/clustering; default
uniform `query: `, flag asymmetric split as the A/B alternate. Implement as
`query_prefix`/`passage_prefix` attrs on the embedder, read via
`getattr(embedder, "query_prefix", "")` at both call sites — keeps the fns
model-agnostic and leaves `test_semantic.py`'s `KeywordEmbedder` fake
untouched.

## Ordered step list (every file touched)

1. **`sidecar/semantic.py`** — `MODEL_REVISION` → `761b72…fa78`; `MODEL_ID` →
   `Xenova/multilingual-e5-small@{rev}:q8` (QInt8 confirmed via
   quant_config.json); `MODEL_RELATIVE_DIR` → `semantic/multilingual-e5-small`;
   `VOCAB_*` → `TOKENIZER_*` (tokenizer.json + new hash); replace
   `WordPieceTokenizer` with a thin `tokenizers.Tokenizer.from_file()` wrapper
   exposing the same `.encode(text, max_length) -> (ids, attention, type)`
   shape `_batch()` consumes (verify tokenizer.json's post_processor adds
   `<s>`/`</s>`; enable padding/truncation to max_length); rename
   `OnnxMiniLmEmbedder` → `OnnxSentenceEmbedder`; add prefix attrs; apply
   prefixes in `rank_paragraphs` (line ~414) and `extract_entities`
   (~766, ~799). Keep `DEFAULT_MAX_LENGTH = 256` initially (e5 supports 512).
2. **`sidecar/requirements.in`/`.txt`** — add `tokenizers>=0.20,<1`;
   regenerate `requirements.lock` via the release-machine flow.
3. **`scripts/build-sidecar.ps1`** — lines 41-44 paths+hashes; add
   `--collect-all tokenizers` to PyInstaller (compiled Rust extension, same
   reasoning as onnxruntime/cv2 at lines 126-134). Smoke test unchanged.
4. **`src-tauri/src/model_download.rs`** — lines 48-53 consts renamed
   (`SEMANTIC_TOKENIZER_*`), new targets
   `semantic/multilingual-e5-small/{model.onnx,tokenizer.json}`, new SHAs;
   lines 106-121 MODEL_FILES: repo/revision/hf_path/size_hints. Blast radius
   verified: only model_download.rs + lib.rs touch these consts.
5. **`src-tauri/src/lib.rs`** — bundled-seed loop (actual lines 1601-1639):
   only the two identifier renames; no logic change.
6. **`src-tauri/tauri.conf.json:46`** — resource glob dir rename.
7. **Verification scripts** — `scripts/dev-stubs.ps1:70-77`,
   `dev-stubs.sh:32-34,95-100`, `verify-binaries.ps1:24-26,82-84`,
   `stage-release-inputs.ps1:24-35,147-159,213-216` (URLs now
   `…/resolve/{rev}/onnx/model_quantized.onnx` and `…/tokenizer.json`),
   `package-portable.ps1:92-93,112`, `portable-contract.mjs:38-39,53-56`.
   `check-stub-marker.mjs`: no change (binaries/ only).
8. **`models/download_models.py:71-82`** + **`models/models.lock.json`** +
   **`models/tests/test_download_models.py:20-23,72-109`** — the lock test is
   the master cross-validation gate (asserts specs ↔ lock ↔ tauri.conf ↔
   stage/build scripts agree); it fails loudly on any drift. Treat as the
   integration checklist.
9. **`RELEASING.md:77`** pin-table row.
10. **`src-tauri/src/filter.rs:24-26`** (`SEMANTIC_TOP_K=12`,
    `SEMANTIC_MIN_SCORE=0.12`, `SEMANTIC_DIVERSITY=0.22`) and
    `convertd.py` defaults (~1071 `min_score=0.12`, ~1111 `threshold=0.42`)
    are **calibrated to MiniLM's cosine distribution — must be re-tuned, not
    carried over**. Protocol: run both models over the `~/backlog-stress`
    multilingual sample, log raw cosine scores per op per language, pick
    thresholds at the same recall/precision operating point (raw scores are
    not comparable across embedding spaces). TOP_K/DIVERSITY likely fine.
    Adjacent follow-up (not blocking): `filter.rs:222-256` probe strings are
    hardcoded English; cross-lingual matching should still work — measure in
    the A/B.
11. **License/docs** — NOTICE.md:26,29-34,65;
    docs/DEPENDENCY_COMPATIBILITY.md:32,47; docs/RELEASE_CHECKLIST.md:39;
    docs/USER_GUIDE.md:31; sidecar/BUILD.md:53; CHANGELOG.md new entry only.
12. **`.gitignore:33,35`** — add `**/tokenizer.json` ignore twins.
13. **Tests** — test_semantic.py: add prefix-application tests (spy on
    `.calls`); test_convertd_unit.py unaffected (mocks the embedder);
    test_download_models.py per step 8.

## RAM / size impact

Installer/portable +~112 MB. Inference session larger (12 layers, 250K
embedding table) — measure on the 8 GB reference machine before shipping,
per the pipeline design spec's stated target; don't assume. 17 MB
tokenizer.json parses once per process — negligible vs the session.

## Rollback

Every touchpoint is a pinned constant + hash; rollback = revert the same
file list. The lock test makes partial rollback fail CI instead of shipping
mismatched. Land as one atomic PR.

## Validation plan

1. Python unit tests (no weights).
2. `models/download_models.py --verify-only` + lock test.
3. `build-sidecar.ps1` frozen smoke test (both semantic ops available: true).
4. **Multilingual A/B** over `~/backlog-stress` en/es/fr/de edges: compare
   evidence selection + entity extraction vs MiniLM; run the threshold
   recalibration on the same pass. This is where the actual quality win must
   show up.
5. `verify-binaries.ps1` + portable validation with new pins.
6. `npm run check:release && npm run check`.
