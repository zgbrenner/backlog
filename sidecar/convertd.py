#!/usr/bin/env python3
"""
convertd: BackLog's ML/conversion sidecar. One warm process, newline-delimited
JSON over stdin/stdout. All models load lazily on first use and stay resident.
Fully offline: every model is loaded from local paths bundled by
models/download_models.py (HF_HUB_OFFLINE is forced below).

Ops:
  ping         -> {}
  versions     -> {"convertd": ..., "models": {...}}
  pdf_probe    {path} -> {median_chars_per_page, pages}
  convert      {path, head_pages, tail_pages} -> ConvertResult
  ocr          {path, dpi, head_pages, tail_pages} -> ConvertResult
               (dpi=0 selects the LFM2.5-VL-450M-Extract fallback engine)
  langid       {text} -> {lang}
  classify     {text, labels} -> {label, score}
  salience     {sentences, probes, top_k} -> {indices}
  ettin_spans  {text} -> {spans: [{label, text, score, iso?}]}

Protocol: every request has an integer "id"; every response echoes it with
"ok": true|false. Errors never kill the process; they return ok=false.
"""

import io
import json
import os
import re
import statistics
import sys
import traceback
from pathlib import Path

# The app makes zero outbound calls at runtime. Enforce it here too: if a
# model file is missing, we fail loudly instead of silently phoning home.
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")

MODELS_DIR = Path(os.environ.get("BACKLOG_MODELS_DIR", Path(__file__).resolve().parent.parent / "models"))

VERSION = "0.1.0"

# ---------------------------------------------------------------------------
# Lazy singletons
# ---------------------------------------------------------------------------

_cache = {}


def _get(name, factory):
    if name not in _cache:
        _cache[name] = factory()
    return _cache[name]


def _markitdown():
    from markitdown import MarkItDown
    # enable_plugins=False and no llm_client: pure local conversion.
    return MarkItDown(enable_plugins=False)


def _pdfium():
    import pypdfium2 as pdfium
    return pdfium


def _rapidocr():
    from rapidocr_onnxruntime import RapidOCR
    return RapidOCR()


def _fasttext():
    import fasttext
    model_path = MODELS_DIR / "lid.176.ftz"
    return fasttext.load_model(str(model_path))


def _gliclass():
    from gliclass import GLiClassModel, ZeroShotClassificationPipeline
    from transformers import AutoTokenizer
    path = str(MODELS_DIR / "gliclass-base-v3.0")
    model = GLiClassModel.from_pretrained(path)
    tokenizer = AutoTokenizer.from_pretrained(path)
    return ZeroShotClassificationPipeline(
        model, tokenizer, classification_type="single-label", device="cpu"
    )


def _granite():
    from sentence_transformers import SentenceTransformer
    return SentenceTransformer(str(MODELS_DIR / "granite-embedding-small-english-r2"), device="cpu")


def _ettin():
    ettin_dir = os.environ.get("BACKLOG_ETTIN_DIR", "")
    if not ettin_dir or not Path(ettin_dir).exists():
        return None
    from transformers import pipeline
    return pipeline(
        "token-classification",
        model=ettin_dir,
        aggregation_strategy="simple",
        device=-1,
    )


def _vl_extract():
    """LFM2.5-VL-450M-Extract via llama.cpp server is preferred in production;
    this in-process transformers path is the dev fallback. Returns None if the
    weights are not bundled, and the ocr op degrades gracefully."""
    vl_dir = MODELS_DIR / "LFM2.5-VL-450M-Extract"
    if not vl_dir.exists():
        return None
    from transformers import AutoModelForImageTextToText, AutoProcessor
    processor = AutoProcessor.from_pretrained(str(vl_dir))
    model = AutoModelForImageTextToText.from_pretrained(str(vl_dir))
    return (processor, model)


# ---------------------------------------------------------------------------
# PDF helpers
# ---------------------------------------------------------------------------

def _open_pdf(path):
    pdfium = _pdfium()
    try:
        return pdfium.PdfDocument(path)
    except Exception as e:
        msg = str(e).lower()
        if "password" in msg or "encrypt" in msg:
            raise RuntimeError("encrypted: password protected PDF") from e
        raise


def _page_indices(n_pages, head, tail):
    idx = list(range(min(head, n_pages)))
    for i in range(max(n_pages - tail, head), n_pages):
        if i not in idx:
            idx.append(i)
    return idx


def op_pdf_probe(args):
    doc = _open_pdf(args["path"])
    try:
        n = len(doc)
        counts = []
        for i in _page_indices(n, 10, 3):
            page = doc[i]
            tp = page.get_textpage()
            counts.append(len(tp.get_text_range() or ""))
            tp.close()
            page.close()
        median = int(statistics.median(counts)) if counts else 0
        return {"median_chars_per_page": median, "pages": n}
    finally:
        doc.close()


# ---------------------------------------------------------------------------
# Conversion
# ---------------------------------------------------------------------------

DATE_META_RE = re.compile(r"(\d{4})[-/]?(\d{2})[-/]?(\d{2})")
LETTERHEAD_RE = re.compile(r"(?im)^\s*(?:dear\s+\w|to whom it may concern|re\s*:)", re.M)


def _doc_meta_dates(path):
    """ISO dates from document properties (docx core props, pdf info dict)."""
    dates = []
    p = Path(path)
    try:
        if p.suffix.lower() == ".docx":
            import zipfile
            import xml.etree.ElementTree as ET
            with zipfile.ZipFile(p) as z:
                if "docProps/core.xml" in z.namelist():
                    root = ET.fromstring(z.read("docProps/core.xml"))
                    for el in root.iter():
                        if el.tag.endswith("created") or el.tag.endswith("modified"):
                            m = DATE_META_RE.search(el.text or "")
                            if m:
                                dates.append("-".join(m.groups()))
        elif p.suffix.lower() == ".pdf":
            doc = _open_pdf(str(p))
            try:
                for key in ("CreationDate", "ModDate"):
                    v = doc.get_metadata_value(key) or ""
                    m = DATE_META_RE.search(v)
                    if m:
                        dates.append("-".join(m.groups()))
            finally:
                doc.close()
    except Exception:
        pass
    # Validate as real dates; scanner defaults lie but at least parse.
    out = []
    import datetime
    for d in dates:
        try:
            datetime.date.fromisoformat(d)
            if d not in out:
                out.append(d)
        except ValueError:
            continue
    return out


def _letterhead_resets(markdown):
    """Multi-doc scan-packet heuristic: count independent letter openings."""
    return max(0, len(LETTERHEAD_RE.findall(markdown)) - 1)


def op_convert(args):
    path = args["path"]
    md_conv = _markitdown()
    try:
        result = md_conv.convert(path)
        markdown = result.text_content or ""
    except Exception as e:
        msg = str(e).lower()
        if "password" in msg or "encrypt" in msg:
            return {"markdown": "", "encrypted": True, "doc_meta_dates": [], "ocr_used": False,
                    "ocr_mean_conf": 0.0, "page_count": 0, "letterhead_resets": 0}
        raise

    # Oversized PDFs: keep head+tail page text only. MarkItDown already
    # returned everything, so trim by an approximate per-page budget.
    if Path(path).suffix.lower() == ".pdf":
        head = int(args.get("head_pages", 10))
        tail = int(args.get("tail_pages", 3))
        try:
            doc = _open_pdf(path)
            n = len(doc)
            doc.close()
            if n > head + tail and len(markdown) > 40000:
                per_page = max(1, len(markdown) // n)
                markdown = markdown[: per_page * head] + "\n\n[...]\n\n" + markdown[-per_page * tail:]
        except Exception:
            pass

    return {
        "markdown": markdown,
        "encrypted": False,
        "doc_meta_dates": _doc_meta_dates(path),
        "ocr_used": False,
        "ocr_mean_conf": 1.0,
        "page_count": 0,
        "letterhead_resets": _letterhead_resets(markdown),
    }


def _render_pages(path, dpi, head, tail):
    """Yield PIL images for the sampled pages of a PDF, or the image itself."""
    from PIL import Image
    p = Path(path)
    if p.suffix.lower() != ".pdf":
        yield Image.open(p).convert("RGB")
        return
    doc = _open_pdf(path)
    try:
        for i in _page_indices(len(doc), head, tail):
            page = doc[i]
            scale = dpi / 72.0
            bitmap = page.render(scale=scale)
            yield bitmap.to_pil().convert("RGB")
            page.close()
    finally:
        doc.close()


def op_ocr(args):
    import numpy as np
    path = args["path"]
    dpi = int(args.get("dpi", 300))
    head = int(args.get("head_pages", 10))
    tail = int(args.get("tail_pages", 3))

    if dpi == 0:
        return _op_vl_extract(path, head, tail)

    ocr = _rapidocr()
    texts, confs = [], []
    for img in _render_pages(path, dpi, head, tail):
        arr = np.array(img)
        result, _ = ocr(arr)
        if result:
            for _box, text, conf in result:
                texts.append(text)
                confs.append(float(conf))
        texts.append("")  # page break
    markdown = "\n".join(texts)
    mean_conf = float(statistics.mean(confs)) if confs else 0.0
    return {
        "markdown": markdown,
        "encrypted": False,
        "doc_meta_dates": _doc_meta_dates(path),
        "ocr_used": True,
        "ocr_mean_conf": mean_conf,
        "page_count": 0,
        "letterhead_resets": _letterhead_resets(markdown),
    }


def _op_vl_extract(path, head, tail):
    vl = _vl_extract()
    if vl is None:
        raise RuntimeError("VL fallback unavailable: LFM2.5-VL-450M-Extract not bundled")
    processor, model = vl
    pieces = []
    for img in _render_pages(path, 200, min(head, 4), min(tail, 1)):
        conversation = [{
            "role": "user",
            "content": [
                {"type": "image", "image": img},
                {"type": "text", "text": "Transcribe all text on this page exactly as written."},
            ],
        }]
        inputs = processor.apply_chat_template(
            conversation, add_generation_prompt=True, return_tensors="pt",
            return_dict=True, tokenize=True,
        )
        out = model.generate(**inputs, max_new_tokens=1024, do_sample=False)
        text = processor.batch_decode(out[:, inputs["input_ids"].shape[1]:], skip_special_tokens=True)[0]
        pieces.append(text)
    markdown = "\n\n".join(pieces)
    return {
        "markdown": markdown,
        "encrypted": False,
        "doc_meta_dates": _doc_meta_dates(path),
        "ocr_used": True,
        "ocr_mean_conf": 0.75,  # VL has no per-token conf; nominal pass value
        "page_count": 0,
        "letterhead_resets": _letterhead_resets(markdown),
    }


# ---------------------------------------------------------------------------
# Language / classify / salience / ettin
# ---------------------------------------------------------------------------

def op_langid(args):
    model = _fasttext()
    text = (args.get("text") or "").replace("\n", " ")[:2000]
    if not text.strip():
        return {"lang": "en"}
    labels, _scores = model.predict(text, k=1)
    lang = labels[0].replace("__label__", "") if labels else "en"
    return {"lang": lang}


def op_classify(args):
    pipe = _gliclass()
    text = (args.get("text") or "")[:3000]
    labels = args.get("labels") or ["correspondence"]
    results = pipe(text, labels, threshold=0.0)[0]
    if not results:
        return {"label": "correspondence", "score": 0.0}
    best = max(results, key=lambda r: r["score"])
    return {"label": best["label"], "score": float(best["score"])}


def op_salience(args):
    import numpy as np
    model = _granite()
    sentences = args.get("sentences") or []
    probes = args.get("probes") or []
    top_k = int(args.get("top_k", 12))
    if not sentences:
        return {"indices": []}
    sent_emb = model.encode(sentences, normalize_embeddings=True)
    scores = np.zeros(len(sentences))
    if probes:
        probe_emb = model.encode(probes, normalize_embeddings=True)
        scores += (sent_emb @ probe_emb.T).max(axis=1)
    centroid = sent_emb.mean(axis=0)
    centroid /= (np.linalg.norm(centroid) + 1e-9)
    scores += 0.5 * (sent_emb @ centroid)
    idx = np.argsort(-scores)[:top_k].tolist()
    return {"indices": idx}


ISO_FROM_SPAN = None  # set lazily


def _normalize_span_date(text):
    """Best-effort ISO normalization of an extracted DATE span."""
    import datetime
    text = text.strip()
    fmts = ["%Y-%m-%d", "%B %d, %Y", "%b %d, %Y", "%d %B %Y", "%d %b %Y",
            "%m/%d/%Y", "%m/%d/%y", "%m-%d-%Y"]
    cleaned = re.sub(r"(\d)(st|nd|rd|th)", r"\1", text)
    for f in fmts:
        try:
            return datetime.datetime.strptime(cleaned, f).date().isoformat()
        except ValueError:
            continue
    return None


def op_ettin_spans(args):
    pipe = _ettin()
    if pipe is None:
        return {"spans": []}
    text = (args.get("text") or "")[:8000]
    raw = pipe(text)
    spans = []
    for r in raw:
        label = r.get("entity_group") or r.get("entity") or ""
        if label not in ("DATE", "PARTY", "SUBJECT"):
            continue
        span = {
            "label": label,
            "text": r.get("word", "").strip(),
            "score": float(r.get("score", 0.0)),
        }
        if label == "DATE":
            iso = _normalize_span_date(span["text"])
            if iso:
                span["iso"] = iso
        if span["score"] >= 0.6 and len(span["text"]) >= 3:
            spans.append(span)
    spans.sort(key=lambda s: -s["score"])
    return {"spans": spans[:12]}


def op_versions(_args):
    versions = {"convertd": VERSION}
    lock = MODELS_DIR / "models.lock.json"
    if lock.exists():
        try:
            versions["models"] = json.loads(lock.read_text())
        except Exception:
            pass
    return versions


def op_ping(_args):
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


def main():
    stdin = io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8")
    stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", line_buffering=True)
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        rid = None
        try:
            req = json.loads(line)
            rid = req.get("id")
            op = req.get("op", "")
            handler = OPS.get(op)
            if handler is None:
                raise ValueError(f"unknown op '{op}'")
            data = handler(req)
            resp = {"id": rid, "ok": True}
            resp.update(data)
        except Exception as e:
            resp = {
                "id": rid,
                "ok": False,
                "error": f"{e.__class__.__name__}: {e}",
                "trace": traceback.format_exc(limit=3),
            }
        stdout.write(json.dumps(resp, ensure_ascii=False) + "\n")


if __name__ == "__main__":
    main()
