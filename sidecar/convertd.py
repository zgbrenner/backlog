#!/usr/bin/env python3
"""BackLog's fully offline conversion and small-model sidecar.

One warm process speaks newline-delimited JSON over stdin/stdout. Model and
conversion components load lazily from local paths and remain resident.

Operations:
  ping          -> {}
  versions      -> {convertd, models?}
  pdf_probe     {path} -> {median_chars_per_page, pages}
  convert       {path, head_pages, tail_pages} -> ConvertResult
  ocr           {path, dpi, head_pages, tail_pages} -> ConvertResult
                dpi=0 selects enhanced 600-DPI classical OCR
  langid        {text} -> {lang}
  classify      {text, labels} -> {label, score, available}
  salience      {sentences, probes, top_k} -> {indices, available}
  ettin_spans   {text} -> {spans}

classify, salience, and ettin_spans are naming ENHANCEMENTS layered on top of
the deterministic harvest. This is the slim, torch-free sidecar profile:
gliclass/transformers/sentence-transformers are not installed (and, even on a
full install, the gliclass/granite model snapshots or BACKLOG_ETTIN_DIR may
be absent). All three ops degrade to cheap deterministic fallbacks instead of
failing -- ok=true always, with available:false on classify/salience when the
real model wasn't used -- so a missing enhancement lane never flags a
document as a processing failure upstream.
"""

from __future__ import annotations

import io
import json
import os
import re
import statistics
import sys
import traceback
from pathlib import Path

# Missing local assets must fail rather than silently reaching a model hub.
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")
os.environ.setdefault("HF_DATASETS_OFFLINE", "1")

_DEFAULT_MODELS_DIR = (
    Path(sys.executable).resolve().parent / "models"
    if getattr(sys, "frozen", False)
    else Path(__file__).resolve().parent.parent / "models"
)
MODELS_DIR = Path(os.environ.get("BACKLOG_MODELS_DIR", _DEFAULT_MODELS_DIR))
VERSION = "0.2.0"
_CACHE: dict[str, object] = {}
_UNAVAILABLE = object()  # sentinel: an optional loader's factory raised once


def _get(name: str, factory):
    if name not in _CACHE:
        _CACHE[name] = factory()
    return _CACHE[name]


def _get_optional(name: str, factory):
    """Like `_get`, but for OPTIONAL naming-enhancement components only
    (gliclass classify, granite salience, ettin spans) -- never for the core
    pipeline (markitdown/pdfium/rapidocr/lingua), which must keep failing
    loudly if genuinely broken.

    A missing library (ImportError, when torch/transformers/gliclass/
    sentence-transformers aren't installed -- the slim sidecar profile) or a
    missing/corrupt local model snapshot (OSError and friends) is caught and
    cached as unavailable, so the expensive import/load is attempted at most
    once per process rather than retried on every request. Returns None when
    unavailable; callers treat None as "fall back to the deterministic
    default".
    """
    if name not in _CACHE:
        try:
            _CACHE[name] = factory()
        except Exception:
            _CACHE[name] = _UNAVAILABLE
    value = _CACHE[name]
    return None if value is _UNAVAILABLE else value


def _markitdown():
    def create():
        from markitdown import MarkItDown

        return MarkItDown(enable_plugins=False)

    return _get("markitdown", create)


def _pdfium():
    def create():
        import pypdfium2 as pdfium

        return pdfium

    return _get("pdfium", create)


def _rapidocr():
    def create():
        from rapidocr import RapidOCR

        return RapidOCR()

    return _get("rapidocr", create)


def _language_detector():
    def create():
        from lingua import LanguageDetectorBuilder

        # Evidence samples are normally hundreds of characters. Low-accuracy
        # mode keeps memory near 100 MB and remains accurate on long text.
        return (
            LanguageDetectorBuilder.from_all_spoken_languages()
            .with_low_accuracy_mode()
            .build()
        )

    return _get("language_detector", create)


def _gliclass():
    """Zero-shot doc-type classifier. Returns None (and op_classify falls
    back to a deterministic default) when gliclass/transformers aren't
    installed -- always true on the slim sidecar profile -- or the local
    gliclass-base-v3.0 snapshot under MODELS_DIR is absent."""

    def create():
        from gliclass import GLiClassModel, ZeroShotClassificationPipeline
        from transformers import AutoTokenizer

        path = str(MODELS_DIR / "gliclass-base-v3.0")
        model = GLiClassModel.from_pretrained(path)
        tokenizer = AutoTokenizer.from_pretrained(path)
        return ZeroShotClassificationPipeline(
            model,
            tokenizer,
            classification_type="single-label",
            device="cpu",
        )

    return _get_optional("gliclass", create)


def _granite():
    """Sentence-embedding model for salience ranking. Returns None (and
    op_salience falls back to document order) when sentence-transformers
    isn't installed -- always true on the slim sidecar profile -- or the
    local granite-embedding-small-english-r2 snapshot is absent."""

    def create():
        from sentence_transformers import SentenceTransformer

        return SentenceTransformer(
            str(MODELS_DIR / "granite-embedding-small-english-r2"),
            device="cpu",
        )

    return _get_optional("granite", create)


def _ettin():
    """Fine-tuned span extractor. Returns None (and op_ettin_spans falls
    back to no spans) when BACKLOG_ETTIN_DIR is unset/missing, or when
    transformers isn't installed -- always true on the slim sidecar profile
    -- even if a directory was configured."""

    def create():
        ettin_dir = os.environ.get("BACKLOG_ETTIN_DIR", "")
        if not ettin_dir or not Path(ettin_dir).is_dir():
            return None
        from transformers import pipeline

        return pipeline(
            "token-classification",
            model=ettin_dir,
            aggregation_strategy="simple",
            device=-1,
        )

    return _get_optional("ettin", create)


# ---------------------------------------------------------------------------
# PDF and metadata helpers
# ---------------------------------------------------------------------------


def _open_pdf(path: str):
    pdfium = _pdfium()
    try:
        return pdfium.PdfDocument(path)
    except Exception as error:
        message = str(error).lower()
        if "password" in message or "encrypt" in message:
            raise RuntimeError("encrypted: password protected PDF") from error
        raise


def _page_indices(n_pages: int, head: int, tail: int) -> list[int]:
    indices = list(range(min(max(head, 0), n_pages)))
    tail_start = max(n_pages - max(tail, 0), len(indices))
    for index in range(tail_start, n_pages):
        if index not in indices:
            indices.append(index)
    return indices


def op_pdf_probe(args: dict) -> dict:
    document = _open_pdf(args["path"])
    try:
        counts = []
        for index in _page_indices(len(document), 10, 3):
            page = document[index]
            try:
                text_page = page.get_textpage()
                try:
                    counts.append(len(text_page.get_text_range() or ""))
                finally:
                    text_page.close()
            finally:
                page.close()
        median = int(statistics.median(counts)) if counts else 0
        return {"median_chars_per_page": median, "pages": len(document)}
    finally:
        document.close()


DATE_META_RE = re.compile(r"(\d{4})[-/]?(\d{2})[-/]?(\d{2})")
LETTERHEAD_RE = re.compile(
    r"(?im)^\s*(?:dear\s+\w|to whom it may concern|re\s*:)",
    re.M,
)


def _doc_meta_dates(path: str) -> list[str]:
    """Return real ISO dates found in DOCX core properties or PDF metadata."""
    import datetime

    values: list[str] = []
    source = Path(path)
    try:
        if source.suffix.lower() == ".docx":
            import xml.etree.ElementTree as ET
            import zipfile

            with zipfile.ZipFile(source) as archive:
                if "docProps/core.xml" in archive.namelist():
                    root = ET.fromstring(archive.read("docProps/core.xml"))
                    for element in root.iter():
                        if element.tag.endswith(("created", "modified")):
                            match = DATE_META_RE.search(element.text or "")
                            if match:
                                values.append("-".join(match.groups()))
        elif source.suffix.lower() == ".pdf":
            document = _open_pdf(str(source))
            try:
                for key in ("CreationDate", "ModDate"):
                    match = DATE_META_RE.search(
                        document.get_metadata_value(key) or ""
                    )
                    if match:
                        values.append("-".join(match.groups()))
            finally:
                document.close()
    except Exception:
        # Document metadata is optional, low-trust evidence. Conversion should
        # continue when a producer wrote malformed properties.
        pass

    dates: list[str] = []
    for value in values:
        try:
            datetime.date.fromisoformat(value)
        except ValueError:
            continue
        if value not in dates:
            dates.append(value)
    return dates


def _letterhead_resets(markdown: str) -> int:
    return max(0, len(LETTERHEAD_RE.findall(markdown)) - 1)


# ---------------------------------------------------------------------------
# Conversion and OCR
# ---------------------------------------------------------------------------


def _conversion_result(
    path: str,
    markdown: str,
    *,
    encrypted: bool = False,
    ocr_used: bool = False,
    ocr_mean_conf: float = 0.0,
    page_count: int = 0,
) -> dict:
    return {
        "markdown": markdown,
        "encrypted": encrypted,
        "doc_meta_dates": [] if encrypted else _doc_meta_dates(path),
        "ocr_used": ocr_used,
        "ocr_mean_conf": ocr_mean_conf,
        "page_count": page_count,
        "letterhead_resets": _letterhead_resets(markdown),
    }


def op_convert(args: dict) -> dict:
    path = args["path"]
    try:
        markdown = _markitdown().convert(path).text_content or ""
    except Exception as error:
        message = str(error).lower()
        if "password" in message or "encrypt" in message:
            return _conversion_result(path, "", encrypted=True)
        raise

    if Path(path).suffix.lower() == ".pdf":
        head = int(args.get("head_pages", 10))
        tail = int(args.get("tail_pages", 3))
        try:
            document = _open_pdf(path)
            page_count = len(document)
            document.close()
            if page_count > head + tail and len(markdown) > 40_000:
                per_page = max(1, len(markdown) // page_count)
                markdown = (
                    markdown[: per_page * head]
                    + "\n\n[...]\n\n"
                    + markdown[-per_page * tail :]
                )
        except Exception:
            pass

    return _conversion_result(
        path,
        markdown,
        ocr_used=False,
        ocr_mean_conf=1.0,
    )


def _render_pages(path: str, dpi: int, head: int, tail: int):
    from PIL import Image

    source = Path(path)
    if source.suffix.lower() != ".pdf":
        with Image.open(source) as image:
            yield image.convert("RGB")
        return

    document = _open_pdf(path)
    try:
        for index in _page_indices(len(document), head, tail):
            page = document[index]
            try:
                bitmap = page.render(scale=dpi / 72.0)
                yield bitmap.to_pil().convert("RGB")
            finally:
                page.close()
    finally:
        document.close()


def _ocr_profile(requested_dpi: int) -> tuple[int, bool]:
    """Map the retry sentinel to a stronger fully local OCR pass."""
    return (600, True) if requested_dpi == 0 else (requested_dpi, False)


def _rapidocr_lines(result) -> list[tuple[str, float]]:
    """Normalize RapidOCR 3.x and legacy tuple results."""
    if result is None:
        return []

    if hasattr(result, "txts"):
        texts = getattr(result, "txts", None)
        scores = getattr(result, "scores", None)
        if texts is None:
            return []
        score_values = [] if scores is None else list(scores)
        return [
            (
                str(text).strip(),
                float(score_values[index])
                if index < len(score_values)
                else 0.0,
            )
            for index, text in enumerate(texts)
            if str(text).strip()
        ]

    payload = result[0] if isinstance(result, tuple) and len(result) == 2 else result
    if not payload:
        return []
    lines: list[tuple[str, float]] = []
    for item in payload:
        if not isinstance(item, (list, tuple)) or len(item) < 3:
            continue
        text = str(item[1]).strip()
        if text:
            lines.append((text, float(item[2])))
    return lines


def _prepare_ocr_image(image, enhanced: bool):
    if not enhanced:
        return image
    from PIL import ImageOps

    return ImageOps.autocontrast(ImageOps.grayscale(image))


def op_ocr(args: dict) -> dict:
    import numpy as np

    path = args["path"]
    requested_dpi = int(args.get("dpi", 300))
    dpi, enhanced = _ocr_profile(requested_dpi)
    head = int(args.get("head_pages", 10))
    tail = int(args.get("tail_pages", 3))

    engine = _rapidocr()
    texts: list[str] = []
    confidences: list[float] = []
    page_count = 0
    for image in _render_pages(path, dpi, head, tail):
        page_count += 1
        result = engine(np.array(_prepare_ocr_image(image, enhanced)))
        for text, confidence in _rapidocr_lines(result):
            texts.append(text)
            confidences.append(confidence)
        texts.append("")

    markdown = "\n".join(texts)
    confidence = (
        float(statistics.mean(confidences)) if confidences else 0.0
    )
    return _conversion_result(
        path,
        markdown,
        ocr_used=True,
        ocr_mean_conf=confidence,
        page_count=page_count,
    )


# ---------------------------------------------------------------------------
# Language, classification, salience, and Ettin
# ---------------------------------------------------------------------------


def _language_code(language) -> str:
    if language is None:
        return "en"
    iso = getattr(language, "iso_code_639_1", None)
    name = getattr(iso, "name", "") if iso is not None else ""
    return str(name).lower() if name else "en"


def op_langid(args: dict) -> dict:
    text = (args.get("text") or "").replace("\n", " ")[:2000]
    if not text.strip():
        return {"lang": "en"}
    language = _language_detector().detect_language_of(text)
    return {"lang": _language_code(language)}


_CLASSIFY_FALLBACK_LABEL = "correspondence"


def _classify_fallback(labels: list) -> dict:
    """Neutral default when gliclass is unavailable or a live inference
    call fails: pick "correspondence" if it's in the offered label set (it
    always is for BackLog's own taxonomy, filter.rs::default_labels), else
    the first offered label, so the caller's downstream logic always gets a
    member of the set it asked about."""
    label = _CLASSIFY_FALLBACK_LABEL if _CLASSIFY_FALLBACK_LABEL in labels else labels[0]
    return {"label": label, "score": 0.0, "available": False}


def op_classify(args: dict) -> dict:
    text = (args.get("text") or "")[:3000]
    labels = args.get("labels") or [_CLASSIFY_FALLBACK_LABEL]
    pipeline = _gliclass()
    if pipeline is None:
        return _classify_fallback(labels)

    try:
        results = pipeline(text, labels, threshold=0.0)[0]
    except Exception:
        # A live model that errors mid-inference (bad snapshot, OOM, etc.)
        # degrades exactly like an absent one -- classify must never bubble
        # an error up to the Rust pipeline and flag the document.
        return _classify_fallback(labels)

    if not results:
        return {"label": _CLASSIFY_FALLBACK_LABEL, "score": 0.0, "available": True}
    best = max(results, key=lambda result: result["score"])
    return {"label": best["label"], "score": float(best["score"]), "available": True}


def op_salience(args: dict) -> dict:
    import numpy as np

    sentences = args.get("sentences") or []
    probes = args.get("probes") or []
    top_k = int(args.get("top_k", 12))
    if not sentences:
        return {"indices": []}

    def document_order_fallback() -> dict:
        return {"indices": list(range(min(top_k, len(sentences)))), "available": False}

    model = _granite()
    if model is None:
        return document_order_fallback()

    try:
        sentence_embeddings = model.encode(
            sentences,
            normalize_embeddings=True,
        )
        scores = np.zeros(len(sentences))
        if probes:
            probe_embeddings = model.encode(probes, normalize_embeddings=True)
            scores += (sentence_embeddings @ probe_embeddings.T).max(axis=1)
        centroid = sentence_embeddings.mean(axis=0)
        centroid /= np.linalg.norm(centroid) + 1e-9
        scores += 0.5 * (sentence_embeddings @ centroid)
    except Exception:
        return document_order_fallback()

    return {"indices": np.argsort(-scores)[:top_k].tolist(), "available": True}


def _normalize_span_date(text: str) -> str | None:
    import datetime

    formats = [
        "%Y-%m-%d",
        "%B %d, %Y",
        "%b %d, %Y",
        "%d %B %Y",
        "%d %b %Y",
        "%m/%d/%Y",
        "%m/%d/%y",
        "%m-%d-%Y",
    ]
    cleaned = re.sub(r"(\d)(st|nd|rd|th)", r"\1", text.strip())
    for date_format in formats:
        try:
            return datetime.datetime.strptime(cleaned, date_format).date().isoformat()
        except ValueError:
            continue
    return None


def op_ettin_spans(args: dict) -> dict:
    extractor = _ettin()
    if extractor is None:
        return {"spans": []}

    try:
        raw = extractor((args.get("text") or "")[:8000])
    except Exception:
        # Mirrors classify/salience: a live extractor that errors mid-
        # inference degrades to no spans rather than failing the op.
        return {"spans": []}
    spans = []
    for item in raw:
        label = item.get("entity_group") or item.get("entity") or ""
        if label not in ("DATE", "PARTY", "SUBJECT"):
            continue
        span = {
            "label": label,
            "text": item.get("word", "").strip(),
            "score": float(item.get("score", 0.0)),
        }
        if label == "DATE":
            iso = _normalize_span_date(span["text"])
            if iso:
                span["iso"] = iso
        if span["score"] >= 0.6 and len(span["text"]) >= 3:
            spans.append(span)
    spans.sort(key=lambda span: -span["score"])
    return {"spans": spans[:12]}


def op_versions(_args: dict) -> dict:
    versions = {"convertd": VERSION}
    lock = MODELS_DIR / "models.lock.json"
    if lock.exists():
        try:
            versions["models"] = json.loads(lock.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            pass
    return versions


def op_ping(_args: dict) -> dict:
    return {}


OPS = {
    "ping": op_ping,
    "versions": op_versions,
    "pdf_probe": op_pdf_probe,
    "convert": op_convert,
    "ocr": op_ocr,
    "langid": op_langid,
    "classify": op_classify,
    "salience": op_salience,
    "ettin_spans": op_ettin_spans,
}


def main() -> None:
    stdin = io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8")
    stdout = io.TextIOWrapper(
        sys.stdout.buffer,
        encoding="utf-8",
        line_buffering=True,
    )
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        request_id = None
        try:
            request = json.loads(line)
            request_id = request.get("id")
            operation = request.get("op", "")
            handler = OPS.get(operation)
            if handler is None:
                raise ValueError(f"unknown op '{operation}'")
            response = {"id": request_id, "ok": True}
            response.update(handler(request))
        except Exception as error:
            response = {
                "id": request_id,
                "ok": False,
                "error": f"{error.__class__.__name__}: {error}",
                "trace": traceback.format_exc(limit=3),
            }
        stdout.write(json.dumps(response, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
