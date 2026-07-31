"""Torch-free semantic evidence helpers for BackLog.

The module deliberately has no import-time dependency on NumPy or ONNX Runtime.
Unit tests and deterministic fallbacks therefore run on a bare Python install,
while the frozen sidecar loads the local ONNX model lazily on the first semantic
request. No code in this module downloads a model or contacts a network.

Two operations share one normalized sentence embedder:

* paragraph ranking selects *unchanged* source paragraphs with stable indices;
* cached-label extraction classifies deterministic candidate spans while
  preserving their exact paragraph-relative character offsets.

Nothing here summarizes or rewrites document text.
"""

from __future__ import annotations

import datetime as _datetime
import hashlib
import json
import math
import re
import unicodedata
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

MODEL_REVISION = "751bff37182d3f1213fa05d7196b954e230abad9"
MODEL_ID = f"Xenova/all-MiniLM-L6-v2@{MODEL_REVISION}:q8"
MODEL_RELATIVE_DIR = Path("semantic") / "all-MiniLM-L6-v2"
MODEL_FILENAME = "model.onnx"
VOCAB_FILENAME = "vocab.txt"
DEFAULT_MAX_LENGTH = 256

DEFAULT_ENTITY_LABELS = [
    {
        "label": "PERSON",
        "description": "a human person's full name, employee, signer, deponent, attorney, or individual",
    },
    {
        "label": "ORGANIZATION",
        "description": "a company, organization, agency, court, employer, vendor, or other legal entity",
    },
    {
        "label": "PARTY",
        "description": "a party entering, receiving, assigning, or terminating an agreement or notice",
    },
    {
        "label": "SUBJECT",
        "description": "the main subject, document title, matter, transaction, or purpose",
    },
    {
        "label": "DOCUMENT_DATE",
        "description": "the date a document, letter, order, invoice, or notice was written, signed, issued, or filed",
    },
    {
        "label": "EFFECTIVE_DATE",
        "description": "the date on which an agreement, amendment, policy, or obligation becomes effective",
    },
    {
        "label": "TERMINATION_DATE",
        "description": "the date employment, an agreement, service, or another relationship terminates or ends",
    },
    {
        "label": "CASE_NUMBER",
        "description": "a court case, claim, docket, cause, or matter number",
    },
    {
        "label": "INVOICE_NUMBER",
        "description": "an invoice, receipt, purchase order, or billing identifier",
    },
    {
        "label": "AMOUNT",
        "description": "a monetary amount, price, payment, balance, total, or amount due",
    },
]


def _dot(left: Sequence[float], right: Sequence[float]) -> float:
    return float(sum(a * b for a, b in zip(left, right)))


def _normalized(vector: Sequence[float]) -> list[float]:
    norm = math.sqrt(sum(float(value) * float(value) for value in vector))
    if not math.isfinite(norm) or norm <= 1e-12:
        return [0.0 for _ in vector]
    return [float(value) / norm for value in vector]


def _mean(vectors: Sequence[Sequence[float]]) -> list[float]:
    if not vectors:
        return []
    width = len(vectors[0])
    total = [0.0] * width
    for vector in vectors:
        if len(vector) != width:
            raise ValueError("embedder returned vectors with inconsistent dimensions")
        for index, value in enumerate(vector):
            total[index] += float(value)
    return _normalized([value / len(vectors) for value in total])


def _finite_score(value: float) -> float:
    value = float(value)
    if not math.isfinite(value):
        return 0.0
    return max(0.0, min(1.0, value))


def _valid_paragraphs(paragraphs: Iterable[dict]) -> list[dict]:
    valid: list[dict] = []
    seen: set[int] = set()
    for fallback_index, raw in enumerate(paragraphs):
        if not isinstance(raw, dict):
            continue
        text = str(raw.get("text") or "")
        if not text.strip():
            continue
        try:
            index = int(raw.get("index", fallback_index))
            start_char = int(raw.get("start_char", 0))
            end_char = int(raw.get("end_char", start_char + len(text)))
        except (TypeError, ValueError):
            continue
        if index < 0 or index in seen or start_char < 0 or end_char < start_char:
            continue
        seen.add(index)
        valid.append(
            {
                "index": index,
                "text": text,
                "start_char": start_char,
                "end_char": end_char,
            }
        )
    return valid


# ---------------------------------------------------------------------------
# Minimal BERT uncased WordPiece tokenizer
# ---------------------------------------------------------------------------


def _is_whitespace(char: str) -> bool:
    return char in " \t\n\r" or unicodedata.category(char) == "Zs"


def _is_control(char: str) -> bool:
    if char in "\t\n\r":
        return False
    return unicodedata.category(char) in ("Cc", "Cf")


def _is_punctuation(char: str) -> bool:
    code = ord(char)
    if 33 <= code <= 47 or 58 <= code <= 64 or 91 <= code <= 96 or 123 <= code <= 126:
        return True
    return unicodedata.category(char).startswith("P")


def _is_chinese_char(code: int) -> bool:
    return (
        0x4E00 <= code <= 0x9FFF
        or 0x3400 <= code <= 0x4DBF
        or 0x20000 <= code <= 0x2A6DF
        or 0x2A700 <= code <= 0x2B73F
        or 0x2B740 <= code <= 0x2B81F
        or 0x2B820 <= code <= 0x2CEAF
        or 0xF900 <= code <= 0xFAFF
        or 0x2F800 <= code <= 0x2FA1F
    )


def _clean_text(text: str) -> str:
    output: list[str] = []
    for char in text:
        code = ord(char)
        if code == 0 or code == 0xFFFD or _is_control(char):
            continue
        output.append(" " if _is_whitespace(char) else char)
    return "".join(output)


def _tokenize_chinese_chars(text: str) -> str:
    output: list[str] = []
    for char in text:
        if _is_chinese_char(ord(char)):
            output.extend((" ", char, " "))
        else:
            output.append(char)
    return "".join(output)


def _strip_accents(text: str) -> str:
    return "".join(
        char for char in unicodedata.normalize("NFD", text) if unicodedata.category(char) != "Mn"
    )


def _split_punctuation(token: str) -> list[str]:
    output: list[list[str]] = []
    current: list[str] = []
    for char in token:
        if _is_punctuation(char):
            if current:
                output.append(current)
                current = []
            output.append([char])
        else:
            current.append(char)
    if current:
        output.append(current)
    return ["".join(chars) for chars in output]


class WordPieceTokenizer:
    """Enough of BertTokenizer to feed the pinned uncased MiniLM graph."""

    def __init__(self, vocab: dict[str, int]):
        self.vocab = dict(vocab)
        required = ("[PAD]", "[UNK]", "[CLS]", "[SEP]")
        missing = [token for token in required if token not in self.vocab]
        if missing:
            raise ValueError(f"vocabulary is missing special tokens: {', '.join(missing)}")
        self.pad_id = self.vocab["[PAD]"]
        self.unk_id = self.vocab["[UNK]"]
        self.cls_id = self.vocab["[CLS]"]
        self.sep_id = self.vocab["[SEP]"]

    @classmethod
    def from_file(cls, path: Path) -> "WordPieceTokenizer":
        lines = path.read_text(encoding="utf-8").splitlines()
        return cls({token: index for index, token in enumerate(lines)})

    def _basic_tokens(self, text: str) -> list[str]:
        cleaned = _tokenize_chinese_chars(_clean_text(text))
        output: list[str] = []
        for token in cleaned.strip().split():
            token = _strip_accents(token.lower())
            output.extend(_split_punctuation(token))
        return [token for token in output if token]

    def _wordpieces(self, token: str) -> list[str]:
        if len(token) > 100:
            return ["[UNK]"]
        pieces: list[str] = []
        start = 0
        while start < len(token):
            end = len(token)
            current = None
            while start < end:
                fragment = token[start:end]
                if start > 0:
                    fragment = "##" + fragment
                if fragment in self.vocab:
                    current = fragment
                    break
                end -= 1
            if current is None:
                return ["[UNK]"]
            pieces.append(current)
            start = end
        return pieces

    def tokenize(self, text: str) -> list[str]:
        pieces: list[str] = []
        for token in self._basic_tokens(text):
            pieces.extend(self._wordpieces(token))
        return pieces

    def encode(self, text: str, max_length: int = DEFAULT_MAX_LENGTH) -> tuple[list[int], list[int], list[int]]:
        if max_length < 2:
            raise ValueError("max_length must leave room for [CLS] and [SEP]")
        pieces = self.tokenize(text)[: max_length - 2]
        ids = [self.cls_id]
        ids.extend(self.vocab.get(piece, self.unk_id) for piece in pieces)
        ids.append(self.sep_id)
        attention = [1] * len(ids)
        padding = max_length - len(ids)
        if padding > 0:
            ids.extend([self.pad_id] * padding)
            attention.extend([0] * padding)
        token_types = [0] * max_length
        return ids, attention, token_types


class OnnxMiniLmEmbedder:
    """Local quantized MiniLM inference using only NumPy and ONNX Runtime."""

    model_id = MODEL_ID

    def __init__(self, model_path: Path, vocab_path: Path, max_length: int = DEFAULT_MAX_LENGTH):
        import onnxruntime as ort

        self._np = __import__("numpy")
        self.tokenizer = WordPieceTokenizer.from_file(vocab_path)
        self.max_length = int(max_length)
        options = ort.SessionOptions()
        options.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
        options.intra_op_num_threads = max(1, min(2, (getattr(__import__("os"), "cpu_count")() or 1)))
        options.inter_op_num_threads = 1
        self.session = ort.InferenceSession(
            str(model_path),
            sess_options=options,
            providers=["CPUExecutionProvider"],
        )
        self.input_names = {item.name for item in self.session.get_inputs()}

    def _batch(self, texts: Sequence[str]) -> list[list[float]]:
        np = self._np
        encoded = [self.tokenizer.encode(text, self.max_length) for text in texts]
        input_ids = np.asarray([item[0] for item in encoded], dtype=np.int64)
        attention = np.asarray([item[1] for item in encoded], dtype=np.int64)
        token_types = np.asarray([item[2] for item in encoded], dtype=np.int64)
        inputs = {"input_ids": input_ids, "attention_mask": attention}
        if "token_type_ids" in self.input_names:
            inputs["token_type_ids"] = token_types
        output = self.session.run(None, inputs)[0]
        if output.ndim == 3:
            mask = attention.astype(np.float32)[..., None]
            pooled = (output * mask).sum(axis=1) / np.clip(mask.sum(axis=1), 1e-9, None)
        elif output.ndim == 2:
            pooled = output
        else:
            raise ValueError(f"unexpected semantic model output rank {output.ndim}")
        norm = np.linalg.norm(pooled, axis=1, keepdims=True)
        pooled = pooled / np.clip(norm, 1e-12, None)
        if not np.isfinite(pooled).all():
            raise ValueError("semantic model returned non-finite embeddings")
        return pooled.astype(np.float32).tolist()

    def encode(self, texts: Iterable[str], batch_size: int = 16) -> list[list[float]]:
        items = [str(text) for text in texts]
        output: list[list[float]] = []
        for offset in range(0, len(items), max(1, int(batch_size))):
            output.extend(self._batch(items[offset : offset + batch_size]))
        return output


def load_embedder(models_dir: Path) -> OnnxMiniLmEmbedder:
    root = Path(models_dir) / MODEL_RELATIVE_DIR
    model = root / MODEL_FILENAME
    vocab = root / VOCAB_FILENAME
    if not model.is_file() or not vocab.is_file():
        missing = [str(path) for path in (model, vocab) if not path.is_file()]
        raise FileNotFoundError("missing local semantic model asset(s): " + ", ".join(missing))
    return OnnxMiniLmEmbedder(model, vocab)


# ---------------------------------------------------------------------------
# Paragraph ranking
# ---------------------------------------------------------------------------


def _structural_prior(text: str) -> float:
    lowered = text.lower().lstrip("#>*- \t")
    prior = 0.0
    if re.match(r"^(subject|re|regarding|notice|effective date|termination date)\s*[:\-]", lowered):
        prior += 0.08
    if any(term in lowered for term in ("effective date", "terminat", "dated as of", "by and between")):
        prior += 0.04
    if len(text) < 35:
        prior -= 0.04
    return prior


def rank_paragraphs(
    embedder,
    paragraphs: Iterable[dict],
    probes: Iterable[str],
    *,
    top_k: int = 12,
    min_score: float = 0.12,
    diversity: float = 0.22,
) -> dict:
    """Rank exact paragraphs with probe relevance plus MMR diversity.

    Results stay in rank order and include the source text and source offsets.
    The Rust layer may restore document order when it renders the evidence lane.
    """

    items = _valid_paragraphs(paragraphs)
    if not items or top_k <= 0:
        return {
            "available": True,
            "model": getattr(embedder, "model_id", "unknown"),
            "results": [],
            "source_chars": sum(len(item["text"]) for item in items),
            "selected_chars": 0,
        }

    probe_texts = [str(probe).strip() for probe in probes if str(probe).strip()]
    if not probe_texts:
        probe_texts = [
            "date of this document",
            "parties to this document",
            "subject matter of this document",
        ]

    paragraph_vectors = [_normalized(vector) for vector in embedder.encode([item["text"] for item in items])]
    probe_vectors = [_normalized(vector) for vector in embedder.encode(probe_texts)]
    if len(paragraph_vectors) != len(items) or len(probe_vectors) != len(probe_texts):
        raise ValueError("semantic embedder returned the wrong number of vectors")
    centroid = _mean(paragraph_vectors)

    scored: list[dict] = []
    for item, vector in zip(items, paragraph_vectors):
        similarities = [_dot(vector, probe) for probe in probe_vectors]
        strongest = max(range(len(similarities)), key=similarities.__getitem__)
        # Sentence-embedding cosine similarity is already centered around zero.
        # Shifting it into [0, 1] made an unrelated zero-vector probe look 50%
        # relevant and could crowd genuinely different evidence out of the MMR
        # selection. Treat negative/zero similarity as no evidence instead.
        probe_score = max(0.0, similarities[strongest])
        centroid_score = max(0.0, _dot(vector, centroid)) if centroid else 0.0
        score = _finite_score(0.88 * probe_score + 0.12 * centroid_score + _structural_prior(item["text"]))
        scored.append(
            {
                **item,
                "score": score,
                "probe": probe_texts[strongest],
                "vector": vector,
            }
        )

    eligible = [item for item in scored if item["score"] >= float(min_score)]
    if not eligible:
        eligible = sorted(scored, key=lambda item: (-item["score"], item["index"]))[:1]

    selected: list[dict] = []
    remaining = list(eligible)
    while remaining and len(selected) < min(int(top_k), len(items)):
        best = None
        best_mmr = -float("inf")
        for candidate in remaining:
            redundancy = (
                max(_dot(candidate["vector"], chosen["vector"]) for chosen in selected)
                if selected
                else 0.0
            )
            mmr = candidate["score"] - max(0.0, min(0.95, float(diversity))) * max(0.0, redundancy)
            if best is None or mmr > best_mmr or (
                math.isclose(mmr, best_mmr) and candidate["index"] < best["index"]
            ):
                best = candidate
                best_mmr = mmr
        assert best is not None
        remaining.remove(best)
        selected.append(best)

    results = []
    for rank, item in enumerate(selected, start=1):
        results.append(
            {
                "index": item["index"],
                "text": item["text"],
                "start_char": item["start_char"],
                "end_char": item["end_char"],
                "score": round(float(item["score"]), 6),
                "probe": item["probe"],
                "rank": rank,
            }
        )
    return {
        "available": True,
        "model": getattr(embedder, "model_id", "unknown"),
        "results": results,
        "source_chars": sum(len(item["text"]) for item in items),
        "selected_chars": sum(len(item["text"]) for item in results),
    }


# ---------------------------------------------------------------------------
# Cached-label entity extraction
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class _Candidate:
    paragraph_index: int
    text: str
    start_char: int
    end_char: int
    context: str
    hints: tuple[str, ...]
    floor: float
    iso: str | None = None


_MONTHS = (
    "January|February|March|April|May|June|July|August|September|October|November|December|"
    "Jan|Feb|Mar|Apr|Jun|Jul|Aug|Sep|Sept|Oct|Nov|Dec"
)
_DATE_PATTERNS = [
    re.compile(r"\b\d{4}-\d{2}-\d{2}\b"),
    re.compile(r"\b\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b"),
    re.compile(rf"\b(?:{_MONTHS})\.?\s+\d{{1,2}}(?:st|nd|rd|th)?(?:,)?\s+\d{{4}}\b", re.I),
    re.compile(rf"\b\d{{1,2}}(?:st|nd|rd|th)?\s+(?:{_MONTHS})\.?\s+\d{{4}}\b", re.I),
]
_MONEY_PATTERN = re.compile(
    r"(?<!\w)(?:US\s*)?[$€£]\s?\d[\d,]*(?:\.\d{2})?|\b\d[\d,]*(?:\.\d{2})?\s*(?:USD|dollars?)\b",
    re.I,
)
# A period is valid *inside* an identifier but sentence punctuation is not part
# of the source span. `\.(?=[A-Z0-9])` makes that distinction without trimming
# after the fact and accidentally changing the provenance offsets.
_IDENTIFIER = r"[A-Z0-9](?:[A-Z0-9/-]|\.(?=[A-Z0-9])){2,}"
_CASE_ID_PATTERN = re.compile(
    rf"\b(?:case|cause|claim|docket|matter)\s*(?:no\.?|number|#)?\s*[:#-]?\s*({_IDENTIFIER})",
    re.I,
)
_INVOICE_ID_PATTERN = re.compile(
    rf"\b(?:invoice|receipt|purchase\s+order|po)\s*(?:no\.?|number|#)?\s*[:#-]?\s*({_IDENTIFIER})",
    re.I,
)
_SUBJECT_PATTERN = re.compile(r"(?im)^\s*(?:subject|re|regarding)\s*:\s*(.{3,180})$")
_ORG_PATTERN = re.compile(
    r"\b(?:[A-Z][\w&'.-]*(?:\s+|$)){1,6}(?:LLC|L\.L\.C\.|Inc\.?|Incorporated|Corp\.?|Corporation|Company|Co\.?|LP|LLP|PLC|University|Association|Department|Agency|Court)\b"
)
_NAME_PATTERN = re.compile(
    r"\b[A-Z][A-Za-z'’-]{1,30}(?:\s+(?:[A-Z]\.|[A-Z][A-Za-z'’-]{1,30})){1,3}\b"
)
_PARTY_PATTERN = re.compile(
    r"(?i)\b(?:between|by and between|from)\s+([A-Z][^\n,;]{2,100}?)\s+(?:and|to)\s+([A-Z][^\n,;]{2,100}?)(?=[,;.\n]|$)"
)
_ORG_SUFFIX = re.compile(
    r"\b(?:LLC|L\.L\.C\.|Inc\.?|Incorporated|Corp\.?|Corporation|Company|Co\.?|LP|LLP|PLC|University|Association|Department|Agency|Court)\b",
    re.I,
)
_NAME_STOP = {
    "invoice number",
    "purchase order",
    "effective date",
    "termination date",
    "united states",
    "subject matter",
    "dear sir",
    "dear madam",
}


def _normalize_date(text: str) -> str | None:
    cleaned = re.sub(r"(\d)(st|nd|rd|th)", r"\1", text.strip(), flags=re.I)
    cleaned = re.sub(r"\b(Sept)\.", "Sep", cleaned, flags=re.I)
    cleaned = re.sub(r"\b([A-Za-z]{3,9})\.", r"\1", cleaned)
    formats = (
        "%Y-%m-%d",
        "%m/%d/%Y",
        "%m/%d/%y",
        "%m-%d-%Y",
        "%B %d, %Y",
        "%B %d %Y",
        "%b %d, %Y",
        "%b %d %Y",
        "%d %B %Y",
        "%d %b %Y",
    )
    for date_format in formats:
        try:
            return _datetime.datetime.strptime(cleaned, date_format).date().isoformat()
        except ValueError:
            continue
    return None


def _date_hints(text: str, start: int, end: int) -> tuple[str, ...]:
    context = text[max(0, start - 80) : min(len(text), end + 80)].lower()
    if any(term in context for term in ("terminat", "last day", "ends on", "ended on")):
        return ("TERMINATION_DATE", "EFFECTIVE_DATE", "DOCUMENT_DATE")
    if any(term in context for term in ("effective", "commence", "takes effect", "as of")):
        return ("EFFECTIVE_DATE", "DOCUMENT_DATE")
    return ("DOCUMENT_DATE", "EFFECTIVE_DATE", "TERMINATION_DATE")


def _candidate_context(text: str, start: int, end: int) -> str:
    return text[max(0, start - 120) : min(len(text), end + 120)].strip()


def _make_candidate(
    paragraph_index: int,
    paragraph_text: str,
    start: int,
    end: int,
    hints: Sequence[str],
    floor: float,
    iso: str | None = None,
) -> _Candidate | None:
    if start < 0 or end <= start or end > len(paragraph_text):
        return None
    value = paragraph_text[start:end].strip()
    if len(value) < 2:
        return None
    # Adjust offsets after trimming so the returned text always slices exactly.
    left_trim = len(paragraph_text[start:end]) - len(paragraph_text[start:end].lstrip())
    right_trimmed = paragraph_text[start:end].rstrip()
    start += left_trim
    end = start + len(right_trimmed.lstrip())
    value = paragraph_text[start:end]
    return _Candidate(
        paragraph_index=paragraph_index,
        text=value,
        start_char=start,
        end_char=end,
        context=_candidate_context(paragraph_text, start, end),
        hints=tuple(hints),
        floor=float(floor),
        iso=iso,
    )


def _generate_candidates(paragraphs: Sequence[dict], limit: int = 512) -> list[_Candidate]:
    output: list[_Candidate] = []
    seen: set[tuple[int, int, int, tuple[str, ...]]] = set()

    def add(candidate: _Candidate | None) -> None:
        if candidate is None or len(output) >= limit:
            return
        key = (
            candidate.paragraph_index,
            candidate.start_char,
            candidate.end_char,
            candidate.hints,
        )
        if key not in seen:
            seen.add(key)
            output.append(candidate)

    for paragraph in paragraphs:
        paragraph_index = paragraph["index"]
        text = paragraph["text"]

        for pattern in _DATE_PATTERNS:
            for match in pattern.finditer(text):
                iso = _normalize_date(match.group(0))
                if iso:
                    add(
                        _make_candidate(
                            paragraph_index,
                            text,
                            match.start(),
                            match.end(),
                            _date_hints(text, match.start(), match.end()),
                            0.88,
                            iso,
                        )
                    )
        for match in _MONEY_PATTERN.finditer(text):
            add(_make_candidate(paragraph_index, text, match.start(), match.end(), ("AMOUNT",), 0.9))
        for pattern, label in (
            (_CASE_ID_PATTERN, "CASE_NUMBER"),
            (_INVOICE_ID_PATTERN, "INVOICE_NUMBER"),
        ):
            for match in pattern.finditer(text):
                add(
                    _make_candidate(
                        paragraph_index,
                        text,
                        match.start(1),
                        match.end(1),
                        (label,),
                        0.92,
                    )
                )
        for match in _SUBJECT_PATTERN.finditer(text):
            add(
                _make_candidate(
                    paragraph_index,
                    text,
                    match.start(1),
                    match.end(1),
                    ("SUBJECT",),
                    0.82,
                )
            )
        for match in _PARTY_PATTERN.finditer(text):
            for group in (1, 2):
                add(
                    _make_candidate(
                        paragraph_index,
                        text,
                        match.start(group),
                        match.end(group),
                        ("PARTY", "ORGANIZATION", "PERSON"),
                        0.72,
                    )
                )
        for match in _ORG_PATTERN.finditer(text):
            add(
                _make_candidate(
                    paragraph_index,
                    text,
                    match.start(),
                    match.end(),
                    ("ORGANIZATION", "PARTY"),
                    0.82,
                )
            )
        for match in _NAME_PATTERN.finditer(text):
            value = match.group(0).strip()
            lowered = value.lower()
            if lowered in _NAME_STOP or _ORG_SUFFIX.search(value):
                continue
            # Avoid all-uppercase headings and phrases ending in ordinary title words.
            if value.isupper() or lowered.endswith((" date", " number", " agreement", " notice")):
                continue
            add(
                _make_candidate(
                    paragraph_index,
                    text,
                    match.start(),
                    match.end(),
                    ("PERSON", "PARTY", "ORGANIZATION"),
                    0.58,
                )
            )
        if len(output) >= limit:
            break
    return output


def _canonical_labels(labels: Iterable[dict]) -> list[dict]:
    normalized: dict[str, str] = {}
    for raw in labels:
        if not isinstance(raw, dict):
            continue
        label = str(raw.get("label") or "").strip().upper()
        description = str(raw.get("description") or label).strip()
        if label and description:
            normalized[label] = description
    return [
        {"label": label, "description": normalized[label]}
        for label in sorted(normalized)
    ]


_LABEL_CACHE: dict[tuple[str, str], tuple[list[dict], list[list[float]]]] = {}


def clear_label_cache() -> None:
    _LABEL_CACHE.clear()


def _label_vectors(embedder, labels: Iterable[dict]) -> tuple[list[dict], list[list[float]], str, bool]:
    canonical = _canonical_labels(labels)
    serialized = json.dumps(canonical, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    cache_key = hashlib.sha256(serialized.encode("utf-8")).hexdigest()
    model_id = str(getattr(embedder, "model_id", "unknown"))
    key = (model_id, cache_key)
    if key in _LABEL_CACHE:
        cached_labels, vectors = _LABEL_CACHE[key]
        return cached_labels, vectors, cache_key, True
    vectors = [_normalized(vector) for vector in embedder.encode([item["description"] for item in canonical])]
    if len(vectors) != len(canonical):
        raise ValueError("semantic embedder returned the wrong number of label vectors")
    _LABEL_CACHE[key] = (canonical, vectors)
    return canonical, vectors, cache_key, False


def extract_entities(
    embedder,
    paragraphs: Iterable[dict],
    labels: Iterable[dict] = DEFAULT_ENTITY_LABELS,
    *,
    threshold: float = 0.42,
    max_per_label: int = 8,
    candidate_limit: int = 512,
) -> dict:
    """Extract exact candidate spans using cached semantic label embeddings."""

    items = _valid_paragraphs(paragraphs)
    canonical, label_vectors, cache_key, reused = _label_vectors(embedder, labels)
    candidates = _generate_candidates(items, max(1, int(candidate_limit)))
    if not candidates or not canonical:
        return {
            "available": True,
            "model": getattr(embedder, "model_id", "unknown"),
            "spans": [],
            "label_cache_key": cache_key,
            "label_embeddings_reused": reused,
            "candidates_considered": len(candidates),
        }

    label_index = {item["label"]: index for index, item in enumerate(canonical)}
    candidate_inputs = [f"{candidate.text}. Context: {candidate.context}" for candidate in candidates]
    candidate_vectors = [_normalized(vector) for vector in embedder.encode(candidate_inputs)]
    if len(candidate_vectors) != len(candidates):
        raise ValueError("semantic embedder returned the wrong number of candidate vectors")

    scored: list[dict] = []
    floor_threshold = max(0.0, min(1.0, float(threshold)))
    for candidate, vector in zip(candidates, candidate_vectors):
        allowed = [label for label in candidate.hints if label in label_index]
        if not allowed:
            allowed = list(label_index)
        best_label = None
        best_score = -1.0
        for label in allowed:
            score = (_dot(vector, label_vectors[label_index[label]]) + 1.0) / 2.0
            if label == candidate.hints[0]:
                score = max(score, candidate.floor)
            if score > best_score:
                best_label = label
                best_score = score
        if best_label is None or best_score < floor_threshold:
            continue
        span = {
            "label": best_label,
            "text": candidate.text,
            "score": round(_finite_score(best_score), 6),
            "paragraph_index": candidate.paragraph_index,
            "start_char": candidate.start_char,
            "end_char": candidate.end_char,
        }
        if candidate.iso:
            span["iso"] = candidate.iso
        scored.append(span)

    # Prefer higher-confidence, longer exact spans when candidates overlap.
    scored.sort(
        key=lambda span: (
            -span["score"],
            span["paragraph_index"],
            span["start_char"],
            -(span["end_char"] - span["start_char"]),
            span["label"],
        )
    )
    accepted: list[dict] = []
    per_label: dict[str, int] = {}
    exact_seen: set[tuple] = set()
    for span in scored:
        if per_label.get(span["label"], 0) >= max(1, int(max_per_label)):
            continue
        exact_key = (
            span["label"],
            span["paragraph_index"],
            span["start_char"],
            span["end_char"],
            span["text"],
        )
        if exact_key in exact_seen:
            continue
        # Same label plus overlapping source range: keep the stronger/longer one already accepted.
        overlaps = any(
            current["label"] == span["label"]
            and current["paragraph_index"] == span["paragraph_index"]
            and span["start_char"] < current["end_char"]
            and current["start_char"] < span["end_char"]
            for current in accepted
        )
        if overlaps:
            continue
        exact_seen.add(exact_key)
        accepted.append(span)
        per_label[span["label"]] = per_label.get(span["label"], 0) + 1

    accepted.sort(
        key=lambda span: (
            span["paragraph_index"],
            span["start_char"],
            span["label"],
        )
    )
    return {
        "available": True,
        "model": getattr(embedder, "model_id", "unknown"),
        "spans": accepted,
        "label_cache_key": cache_key,
        "label_embeddings_reused": reused,
        "candidates_considered": len(candidates),
    }
