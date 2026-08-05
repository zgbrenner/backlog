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
  rank_paragraphs {paragraphs, probes, top_k} -> exact ranked paragraphs
  extract_entities {paragraphs, labels} -> exact cached-label spans
  ettin_spans   {text} -> {spans}

The two semantic evidence operations share a small local quantized ONNX
sentence embedder and preserve unchanged source text plus source offsets. They
never summarize. All optional naming enhancements degrade to structured
unavailable results or deterministic fallbacks rather than failing the
document, so a missing/corrupt enhancement model never flags the core pipeline.
"""

from __future__ import annotations

import io
import json
import math
import os
import re
import statistics
import sys
import traceback
from pathlib import Path

_SIDECAR_DIR = Path(__file__).resolve().parent
if str(_SIDECAR_DIR) not in sys.path:
    sys.path.insert(0, str(_SIDECAR_DIR))
import semantic as semantic_evidence

# Missing local assets must fail rather than silently reaching a model hub.
# Assignment is deliberate: a parent process must not be able to weaken the
# sidecar's offline contract by exporting a false-y value.
os.environ["HF_HUB_OFFLINE"] = "1"
os.environ["TRANSFORMERS_OFFLINE"] = "1"
os.environ["HF_DATASETS_OFFLINE"] = "1"

_DEFAULT_MODELS_DIR = (
    Path(sys.executable).resolve().parent / "models"
    if getattr(sys, "frozen", False)
    else Path(__file__).resolve().parent.parent / "models"
)
MODELS_DIR = Path(os.environ.get("BACKLOG_MODELS_DIR", _DEFAULT_MODELS_DIR))
VERSION = "0.3.0"
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

        # Cap the ONNX thread pools the same way semantic.py already caps its
        # embedder session. RapidOCR's default (-1) lets each of its three
        # sessions (det/cls/rec) claim every logical core -- per worker
        # process. With a full convert pool that oversubscription starves the
        # llama-server naming lane, which is the measured batch bottleneck.
        return RapidOCR(
            params={
                "EngineConfig.onnxruntime.intra_op_num_threads": 2,
                "EngineConfig.onnxruntime.inter_op_num_threads": 1,
            }
        )

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


def _semantic_embedder():
    """One torch-free ONNX embedder shared by ranking and label extraction.

    The loader only consults BACKLOG_MODELS_DIR. `semantic.load_embedder`
    contains no network path, and `_get_optional` ensures a missing or corrupt
    model is probed once per process rather than once per document.
    """

    return _get_optional(
        "semantic_embedder",
        lambda: semantic_evidence.load_embedder(MODELS_DIR),
    )


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


# A modern Office file is a zip. When it carries OLE compound-file magic
# instead it is one of two very different things, and they need opposite
# advice: an encrypted OOXML package -- Office writes the ciphertext into a
# CFB wrapper holding EncryptionInfo and EncryptedPackage -- or a legacy
# binary .doc/.xls/.ppt that somebody renamed, which is routine in an office
# backfill and whose CFB holds WordDocument / Workbook / PowerPoint Document.
# Both make zipfile raise BadZipFile('File is not a zip file'), a message with
# neither 'password' nor 'encrypt' in it, so both used to end as the
# undiagnosable 'all conversion attempts exhausted'. Sending the second user
# off to find a password for a file that has none is worse than that, so the
# magic alone decides nothing: the CFB directory does.
_OLE_CFB_MAGIC = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"
_OOXML_SUFFIXES = (".docx", ".docm", ".xlsx", ".xlsm", ".pptx", ".pptm")
_ENCRYPTED_CFB_STREAMS = frozenset({"EncryptionInfo", "EncryptedPackage"})
_LEGACY_CFB_STREAMS = frozenset(
    {"WordDocument", "Workbook", "Book", "PowerPoint Document"}
)
_CFB_ENDOFCHAIN = 0xFFFFFFFE
# The directory chain is read from the file's own header, so it is bounded
# rather than followed to its end: a loop in the FAT would otherwise spin here.
_CFB_MAX_DIRECTORY_SECTORS = 32


def _cfb_directory_names(path: str) -> set[str]:
    """Stream and storage names from the head of an OLE compound file.

    Only the directory sectors are read -- never a stream -- and only up to
    `_CFB_MAX_DIRECTORY_SECTORS` of them. Everything this walk needs (sector
    size, FAT locations, chain links) comes from a file dropped into a
    SharePoint intake library, so nothing here may size an allocation or a
    loop from it.
    """
    names: set[str] = set()
    try:
        with open(path, "rb") as handle:
            header = handle.read(512)
            if len(header) < 512 or header[:8] != _OLE_CFB_MAGIC:
                return names
            sector_shift = int.from_bytes(header[30:32], "little")
            if sector_shift not in (9, 12):  # only 512- and 4096-byte sectors
                return names
            sector_size = 1 << sector_shift
            fat_sectors = [
                int.from_bytes(header[76 + 4 * index : 80 + 4 * index], "little")
                for index in range(109)
            ]

            def sector_bytes(index: int) -> bytes | None:
                if index >= _CFB_ENDOFCHAIN:
                    return None
                # Sector 0 starts one sector in: the header occupies the
                # first, padded out to sector_size on 4096-byte containers.
                handle.seek((index + 1) * sector_size)
                data = handle.read(sector_size)
                return data if len(data) == sector_size else None

            def next_in_chain(index: int) -> int:
                which, offset = divmod(index, sector_size // 4)
                if which >= len(fat_sectors):
                    return _CFB_ENDOFCHAIN  # past the header DIFAT; stop here
                fat = sector_bytes(fat_sectors[which])
                if fat is None:
                    return _CFB_ENDOFCHAIN
                return int.from_bytes(fat[4 * offset : 4 * offset + 4], "little")

            sector = int.from_bytes(header[48:52], "little")
            for _ in range(_CFB_MAX_DIRECTORY_SECTORS):
                data = sector_bytes(sector)
                if data is None:
                    break
                for offset in range(0, sector_size - 127, 128):
                    entry = data[offset : offset + 128]
                    # Byte 64 is the name length in bytes, terminator included.
                    length = int.from_bytes(entry[64:66], "little")
                    if length < 4 or length > 64 or length % 2:
                        continue
                    name = entry[: length - 2].decode("utf-16-le", "ignore")
                    if name:
                        names.add(name)
                sector = next_in_chain(sector)
    except OSError:
        return names
    return names


def _ole_container_kind(path: str) -> str | None:
    """Classify a file under an OOXML suffix: encrypted, legacy, or neither.

    None covers both the ordinary case (a real zip) and a CFB whose directory
    claims neither shape -- an unrecognized container is left to markitdown
    rather than guessed at.
    """
    if Path(path).suffix.lower() not in _OOXML_SUFFIXES:
        return None
    try:
        with open(path, "rb") as handle:
            if handle.read(8) != _OLE_CFB_MAGIC:
                return None
    except OSError:
        return None
    names = _cfb_directory_names(path)
    if names & _ENCRYPTED_CFB_STREAMS:
        return "encrypted"
    if names & _LEGACY_CFB_STREAMS:
        return "legacy"
    return None


def _is_encrypted_ooxml(path: str) -> bool:
    return _ole_container_kind(path) == "encrypted"


def _looks_encrypted(error: BaseException) -> bool:
    """Whether `error` is a password/encryption failure anywhere in its chain.

    markitdown wraps the real cause, so a substring test over `str(error)`
    only ever matched by accident -- it works for PDFs because markitdown
    interpolates the exception CLASS name and pdfminer's is
    PDFPasswordIncorrect. Match over `repr()` so the class name always counts
    (and, unlike `str()`, an OSError's repr does not splice the file path in,
    which would make any document called 'password-reset.pdf' look
    encrypted), and walk `__cause__`/`__context__` plus markitdown's
    FileConversionException.attempts.
    """
    seen: set[int] = set()
    queue: list[object] = [error]
    while queue:
        current = queue.pop()
        if not isinstance(current, BaseException) or id(current) in seen:
            continue
        seen.add(id(current))
        haystack = f"{type(current).__name__} {current!r}".lower()
        if "password" in haystack or "encrypt" in haystack:
            return True
        queue.append(current.__cause__)
        queue.append(current.__context__)
        for attempt in getattr(current, "attempts", None) or []:
            exc_info = getattr(attempt, "exc_info", None)
            if isinstance(exc_info, tuple) and len(exc_info) >= 2:
                queue.append(exc_info[1])
            else:
                queue.append(attempt)
    return False


def _open_pdf(path: str):
    pdfium = _pdfium()
    try:
        return pdfium.PdfDocument(path)
    except Exception as error:
        if _looks_encrypted(error):
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
    path = args["path"]
    _check_input_size(path)
    document = _open_pdf(path)
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


# The Processing folder is OneDrive-synced from a SharePoint intake library,
# so anyone who can drop a file there reaches this parser. A 306 KB .docx was
# measured driving 914 MB peak RSS through core.xml alone, and this runs on
# every convert and every OCR attempt -- three times per file across the retry
# ladder -- returning cleanly each time, so nothing upstream ever flags it.
#
# These two numbers are what closes it, and the arithmetic is the whole point:
# DEFLATE's ceiling is ~1032:1, so 306 KB of compressed member is up to
# ~315 MB of XML, and the parsed tree costs a multiple of that again -- no
# entity expansion required. The multiplier is the document's shape: measured
# here at the 4 MB cap, one big text node costs 1x, small elements 10x, and a
# million empty <a/> elements 20x. So the cap bounds the tree at ~85 MB in the
# worst case, against the 914 MB that was measured. The ratio cap stops a
# small member from reaching even that.
MAX_ZIP_MEMBER_BYTES = 4 * 1024 * 1024
MAX_ZIP_RATIO = 200
MAX_ZIP_TEXT_TOTAL_BYTES = 16 * 1024 * 1024
_ZIP_TEXT_SUFFIXES = (".xml", ".rels", ".vml", ".html", ".htm", ".txt", ".csv")


def _read_zip_member(archive, name: str) -> bytes | None:
    """Read one archive member within a decompression budget, else None.

    Both bounds are needed: `file_size` stops a merely enormous member, the
    ratio stops a small one that inflates. The header is attacker-controlled,
    so the read itself is capped as well rather than trusted to match.
    """
    try:
        info = archive.getinfo(name)
    except KeyError:
        return None
    if info.file_size > MAX_ZIP_MEMBER_BYTES:
        return None
    if info.compress_size > 0 and info.file_size / info.compress_size > MAX_ZIP_RATIO:
        return None
    with archive.open(info) as member:
        data = member.read(MAX_ZIP_MEMBER_BYTES + 1)
    return None if len(data) > MAX_ZIP_MEMBER_BYTES else data


def _check_ooxml_text_budget(path: str) -> None:
    """Reject XML-heavy ZIP bombs before a document library opens the package.

    ``_read_zip_member`` protects the optional metadata path, but MarkItDown
    and its Office dependencies perform their own ZIP reads. Their parsers are
    therefore reached only after the package's declared text members pass the
    same per-member/ratio limits and a conservative aggregate limit. Binary
    media is intentionally excluded from this accounting: it is not parsed as
    XML and legitimate scanned Office packages commonly contain compressed
    images larger than one XML part.
    """
    import zipfile

    total = 0
    with zipfile.ZipFile(path) as archive:
        for info in archive.infolist():
            if info.is_dir() or not info.filename.lower().endswith(_ZIP_TEXT_SUFFIXES):
                continue
            if info.file_size > MAX_ZIP_MEMBER_BYTES:
                raise ValueError(
                    f"OOXML text member exceeds the {MAX_ZIP_MEMBER_BYTES}-byte sidecar limit"
                )
            if info.compress_size > 0 and info.file_size / info.compress_size > MAX_ZIP_RATIO:
                raise ValueError("OOXML text member compression ratio exceeds the sidecar limit")
            total += info.file_size
            if total > MAX_ZIP_TEXT_TOTAL_BYTES:
                raise ValueError(
                    f"OOXML text members exceed the {MAX_ZIP_TEXT_TOTAL_BYTES}-byte sidecar limit"
                )


class _DoctypeRefused(Exception):
    """A core.xml declared a DTD; see _parse_core_xml."""


def _parse_core_xml(data: bytes):
    """Parse docProps/core.xml, or None when it declares a DTD.

    What bounds the memory here is MAX_ZIP_MEMBER_BYTES above, not this: see
    the arithmetic there. The DTD refusal is a separate, narrower guarantee --
    core properties never legitimately carry one, and refusing it means
    neither entity expansion nor an external reference is ever attempted,
    without depending on defusedxml being installed.

    It has to be enforced by the parser, not by scanning `data`: a
    UTF-16-encoded core.xml contains neither b'<!DOCTYPE' nor b'<!ENTITY' as
    bytes yet parses -- with its DTD -- exactly as an attacker intends. expat
    reports the declaration whatever the document's encoding is.
    """
    import xml.etree.ElementTree as ET
    from xml.parsers import expat

    def refuse(*_args):
        raise _DoctypeRefused()

    parser = expat.ParserCreate()
    parser.StartDoctypeDeclHandler = refuse
    parser.EntityDeclHandler = refuse
    builder = ET.TreeBuilder()
    parser.StartElementHandler = lambda tag, attrs: builder.start(tag, attrs)
    parser.EndElementHandler = builder.end
    parser.CharacterDataHandler = builder.data
    try:
        parser.Parse(data, True)
    except _DoctypeRefused:
        return None
    return builder.close()


def _doc_meta_date_entries(path: str) -> list[dict]:
    """ISO dates from DOCX core properties or PDF metadata, each tagged with
    the property it came from: {"iso": ..., "prop": "created"|"modified"}.

    The tag is the point. The product invariant is that no date ships unless
    it appears in the document text or file metadata, and a modification
    timestamp is the weakest possible member of that set: a 2019 services
    agreement re-saved last March carries dcterms:modified=2026-03-14, which
    would sail through the presence tripwire and reach the SharePoint index
    labelled date_source: metadata. The checker needs the provenance to
    weigh it.
    """
    import datetime

    found: list[tuple[str, str]] = []
    source = Path(path)
    try:
        if source.suffix.lower() in _OOXML_SUFFIXES:
            import zipfile

            with zipfile.ZipFile(source) as archive:
                core = _read_zip_member(archive, "docProps/core.xml")
                root = _parse_core_xml(core) if core else None
                for element in root.iter() if root is not None else ():
                    for prop in ("created", "modified"):
                        if not element.tag.endswith(prop):
                            continue
                        match = DATE_META_RE.search(element.text or "")
                        if match:
                            found.append(("-".join(match.groups()), prop))
        elif source.suffix.lower() == ".pdf":
            document = _open_pdf(str(source))
            try:
                for key, prop in (("CreationDate", "created"), ("ModDate", "modified")):
                    match = DATE_META_RE.search(
                        document.get_metadata_value(key) or ""
                    )
                    if match:
                        found.append(("-".join(match.groups()), prop))
            finally:
                document.close()
    except Exception:
        # Document metadata is optional, low-trust evidence. Conversion should
        # continue when a producer wrote malformed properties.
        pass

    entries: list[dict] = []
    seen: set[tuple[str, str]] = set()
    for value, prop in found:
        try:
            datetime.date.fromisoformat(value)
        except ValueError:
            continue
        if (value, prop) in seen:
            continue
        seen.add((value, prop))
        entries.append({"iso": value, "prop": prop})
    return entries


def _created_dates(entries: list[dict]) -> list[str]:
    """The flat `doc_meta_dates` wire field: creation dates only.

    ConvertResult in sidecar.rs still deserializes this list and filter.rs
    still labels it FILE METADATA DATES, so it stays -- but it now carries
    only the half of the metadata that is worth trusting as date evidence.
    """
    dates: list[str] = []
    for entry in entries:
        if entry["prop"] == "created" and entry["iso"] not in dates:
            dates.append(entry["iso"])
    return dates


def _letterhead_resets(markdown: str) -> int:
    return max(0, len(LETTERHEAD_RE.findall(markdown)) - 1)


# ---------------------------------------------------------------------------
# Conversion and OCR
# ---------------------------------------------------------------------------


# A response is one JSON line and the protocol puts no ceiling on it anywhere,
# so an unbounded conversion (a 10 MB spreadsheet flattens to megabytes of
# markdown) has to be bounded here, on every route -- not only on the PDF
# branch that happened to have a page count handy.
# The parser-side ceiling is intentionally byte-based and checked before any
# document library opens the file; the output ceiling below remains a separate
# character-based protocol bound.
MAX_INPUT_BYTES = 64 * 1024 * 1024
MAX_MARKDOWN_CHARS = 200_000
_ELISION = "\n\n[...]\n\n"


def _check_input_size(path: str) -> None:
    """Reject oversized or unstatable inputs before a parser sees them."""
    try:
        size = Path(path).stat().st_size
    except OSError as error:
        raise RuntimeError("cannot inspect input file size before parsing") from error
    if size > MAX_INPUT_BYTES:
        raise ValueError(
            f"input file exceeds the {MAX_INPUT_BYTES}-byte sidecar limit"
        )


def _cap_markdown(markdown: str) -> str:
    if len(markdown) <= MAX_MARKDOWN_CHARS:
        return markdown
    head = MAX_MARKDOWN_CHARS * 3 // 4
    tail = MAX_MARKDOWN_CHARS - head
    return markdown[:head] + _ELISION + markdown[len(markdown) - tail :]


def _truncate_pdf_markdown(
    markdown: str, page_count: int, head: int, tail: int
) -> str:
    """Keep the first `head` and last `tail` pages' worth of a long PDF."""
    if page_count <= 0 or page_count <= head + tail or len(markdown) <= 40_000:
        return markdown
    per_page = max(1, len(markdown) // page_count)
    head_text = markdown[: per_page * max(head, 0)]
    # tail == 0 is reachable -- max_tail_pages is a user-editable Settings
    # field -- and `markdown[-0:]` is the whole string, so the old slice made
    # "truncation" return more than it was given.
    tail_text = markdown[max(0, len(markdown) - per_page * tail) :] if tail > 0 else ""
    return head_text + _ELISION + tail_text


def _conversion_result(
    path: str,
    markdown: str,
    *,
    encrypted: bool = False,
    ocr_used: bool = False,
    ocr_mean_conf: float = 0.0,
    page_count: int = 0,
    pages_with_text: int = 0,
) -> dict:
    markdown = _cap_markdown(markdown)
    entries = [] if encrypted else _doc_meta_date_entries(path)
    if not math.isfinite(ocr_mean_conf):
        # A bare NaN is not JSON serde_json will accept: sidecar.rs drops the
        # line as noise and the caller waits out its whole 45 s timeout.
        ocr_mean_conf = 0.0
    return {
        "markdown": markdown,
        "encrypted": encrypted,
        "doc_meta_dates": _created_dates(entries),
        "doc_meta_date_entries": entries,
        "ocr_used": ocr_used,
        "ocr_mean_conf": ocr_mean_conf,
        "page_count": page_count,
        "pages_with_text": pages_with_text,
        "letterhead_resets": _letterhead_resets(markdown),
    }


# Plain text needs no structural parser, and charset detection is the one
# conversion stage whose cost scales with the WHOLE file: MarkItDown hands
# charset-normalizer the entire input (its own docs call large inputs a weak
# point) even though `_cap_markdown` throws away everything past
# MAX_MARKDOWN_CHARS immediately afterwards. 2 MiB comfortably covers the
# 200k-char cap in any encoding MarkItDown would have detected; a giant
# single-line txt drops from minutes to milliseconds.
_TEXT_FAST_PATH_SUFFIXES = {".txt", ".text", ".log", ".md"}
TEXT_FAST_PATH_CAP = 2 * 1024 * 1024


def _convert_text_fast(path: str) -> str:
    with open(path, "rb") as handle:
        raw = handle.read(TEXT_FAST_PATH_CAP)
    if not raw:
        return ""
    from charset_normalizer import from_bytes

    best = from_bytes(raw).best()
    return str(best) if best is not None else raw.decode("utf-8", "replace")


def op_convert(args: dict) -> dict:
    path = args["path"]
    _check_input_size(path)
    container = _ole_container_kind(path)
    if container == "encrypted":
        return _conversion_result(path, "", encrypted=True)
    if container is None and Path(path).suffix.lower() in _TEXT_FAST_PATH_SUFFIXES:
        return _conversion_result(
            path,
            _convert_text_fast(path),
            ocr_used=False,
            ocr_mean_conf=1.0,
        )
    if container is None and Path(path).suffix.lower() in _OOXML_SUFFIXES:
        _check_ooxml_text_budget(path)
    try:
        markdown = _markitdown().convert(path).text_content or ""
    except Exception as error:
        if _looks_encrypted(error):
            return _conversion_result(path, "", encrypted=True)
        if container == "legacy":
            # markitdown may still read a renamed .xls (its XLS branch matches
            # on the sniffed mime type, not the suffix), so this only fires
            # once conversion has actually failed. Say what is wrong -- the
            # file needs re-saving, not a password -- while avoiding the words
            # pipeline.rs's error_code() maps to ENCRYPTED.
            raise RuntimeError(
                f"legacy OLE compound document renamed {Path(path).suffix.lower()}: "
                "re-save it in the modern Office format"
            ) from error
        raise

    if Path(path).suffix.lower() == ".pdf":
        head = int(args.get("head_pages", 10))
        tail = int(args.get("tail_pages", 3))
        try:
            document = _open_pdf(path)
            page_count = len(document)
            document.close()
        except Exception:
            page_count = 0
        markdown = _truncate_pdf_markdown(markdown, page_count, head, tail)

    return _conversion_result(
        path,
        markdown,
        ocr_used=False,
        ocr_mean_conf=1.0,
    )


# pdfium renders at whatever scale it is handed, and the enhanced retry rung
# asks for 600 DPI: an ISO A0 page is then ~1.4 gigapixels of RGB. Bound the
# output instead of trusting the page box, which the document controls.
MAX_RENDER_PIXELS = 40_000_000


def _render_scale(width_pt: float, height_pt: float, dpi: int) -> float:
    scale = max(int(dpi), 1) / 72.0
    pixels = max(float(width_pt), 1.0) * max(float(height_pt), 1.0) * scale * scale
    if pixels > MAX_RENDER_PIXELS:
        scale *= (MAX_RENDER_PIXELS / pixels) ** 0.5
    return scale


def _fit_pixel_budget(frame):
    """Downscale a frame to MAX_RENDER_PIXELS, before anything expands it.

    Route::Scanned takes whatever is dropped into the intake library, and
    Pillow's own default only errors above 2x MAX_IMAGE_PIXELS -- a ~170 MP
    PNG merely warns. That frame becomes ~510 MB at .convert("RGB") and
    another ~510 MB at np.array() in op_ocr, per frame, times convert_slots,
    on a 4-core desktop. A PDF page is already clamped to this same budget by
    _render_scale; there is no reason for an image to get four times as much.
    """
    width, height = (int(value) for value in frame.size)
    pixels = max(width, 1) * max(height, 1)
    if pixels <= MAX_RENDER_PIXELS:
        return frame
    scale = (MAX_RENDER_PIXELS / pixels) ** 0.5
    target = (max(1, int(width * scale)), max(1, int(height * scale)))
    # draft() lets a JPEG decode straight to the smaller size instead of
    # materializing the full frame first; it is a no-op for other formats.
    draft = getattr(frame, "draft", None)
    if draft is not None:
        draft("RGB", target)
        if frame.size[0] * frame.size[1] <= MAX_RENDER_PIXELS:
            return frame
    return frame.resize(target)


def _select_frames(image, head: int, tail: int):
    """Yield the head/tail frames of a possibly multi-page image.

    Multi-page TIFF is the standard fax/scan container this user base
    receives; reading frame 0 only meant a 10-page agreement was named from
    its cover sheet and archived as fully processed, with nothing to tell the
    user the other nine pages were never read.
    """
    from PIL import ImageSequence

    frames = int(getattr(image, "n_frames", 1) or 1)
    wanted = set(_page_indices(frames, head, tail))
    for index, frame in enumerate(ImageSequence.Iterator(image)):
        if index in wanted:
            yield _fit_pixel_budget(frame).convert("RGB")


def _render_pages(path: str, dpi: int, head: int, tail: int):
    from PIL import Image

    source = Path(path)
    if source.suffix.lower() != ".pdf":
        # Pillow's decompression-bomb ceiling is a module global and defaults
        # to ~89.5 MP (error only at twice that). Pull it down to the one
        # budget both branches of op_ocr honour, so anything absurd enough to
        # be past even the downscale above raises instead of being decoded.
        Image.MAX_IMAGE_PIXELS = MAX_RENDER_PIXELS
        with Image.open(source) as image:
            yield from _select_frames(image, head, tail)
        return

    document = _open_pdf(path)
    try:
        for index in _page_indices(len(document), head, tail):
            page = document[index]
            try:
                width, height = page.get_size()
                bitmap = page.render(scale=_render_scale(width, height, dpi))
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
    path = args["path"]
    _check_input_size(path)
    import numpy as np

    requested_dpi = int(args.get("dpi", 300))
    dpi, enhanced = _ocr_profile(requested_dpi)
    head = int(args.get("head_pages", 10))
    tail = int(args.get("tail_pages", 3))

    engine = _rapidocr()
    texts: list[str] = []
    confidences: list[float] = []
    page_count = 0
    pages_with_text = 0
    for image in _render_pages(path, dpi, head, tail):
        page_count += 1
        result = engine(np.array(_prepare_ocr_image(image, enhanced)))
        lines = _rapidocr_lines(result)
        if lines:
            pages_with_text += 1
        for text, confidence in lines:
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
        pages_with_text=pages_with_text,
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
        # numpy is imported here, not at the top of the op: on the shipped
        # slim profile _granite() is always None, so the deterministic
        # fallback the module docstring promises must need no array library
        # at all -- and if the model somehow loaded without one, that is just
        # one more reason to fall back rather than fail.
        import numpy as np

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


def _paragraph_source_chars(paragraphs) -> int:
    total = 0
    for paragraph in paragraphs or []:
        if isinstance(paragraph, dict):
            total += len(str(paragraph.get("text") or ""))
    return total


def _semantic_rank_unavailable(paragraphs, reason: str) -> dict:
    return {
        "available": False,
        "model": semantic_evidence.MODEL_ID,
        "reason": reason,
        "results": [],
        "source_chars": _paragraph_source_chars(paragraphs),
        "selected_chars": 0,
    }


def op_rank_paragraphs(args: dict) -> dict:
    """Rank exact source paragraphs with a local semantic model.

    Missing assets and inference faults are intentionally data, not protocol
    errors. The Rust filter owns the deterministic fallback and conservative
    bypass decision, so this operation never invents replacement text.
    """

    paragraphs = args.get("paragraphs") or []
    if not paragraphs:
        return {
            "available": True,
            "model": semantic_evidence.MODEL_ID,
            "results": [],
            "source_chars": 0,
            "selected_chars": 0,
        }
    embedder = _semantic_embedder()
    if embedder is None:
        return _semantic_rank_unavailable(paragraphs, "model_unavailable")
    try:
        return semantic_evidence.rank_paragraphs(
            embedder,
            paragraphs,
            args.get("probes") or [],
            top_k=max(0, int(args.get("top_k", 12))),
            min_score=float(args.get("min_score", 0.12)),
            diversity=float(args.get("diversity", 0.22)),
        )
    except Exception:
        return _semantic_rank_unavailable(paragraphs, "inference_failed")


def _semantic_entities_unavailable(reason: str) -> dict:
    return {
        "available": False,
        "model": semantic_evidence.MODEL_ID,
        "reason": reason,
        "spans": [],
        "label_cache_key": "",
        "label_embeddings_reused": False,
        "candidates_considered": 0,
    }


def op_extract_entities(args: dict) -> dict:
    """Classify deterministic full-document candidates with cached labels."""

    paragraphs = args.get("paragraphs") or []
    if not paragraphs:
        return {
            "available": True,
            "model": semantic_evidence.MODEL_ID,
            "spans": [],
            "label_cache_key": "",
            "label_embeddings_reused": False,
            "candidates_considered": 0,
        }
    embedder = _semantic_embedder()
    if embedder is None:
        return _semantic_entities_unavailable("model_unavailable")
    try:
        return semantic_evidence.extract_entities(
            embedder,
            paragraphs,
            args.get("labels") or semantic_evidence.DEFAULT_ENTITY_LABELS,
            threshold=float(args.get("threshold", 0.42)),
            max_per_label=max(1, int(args.get("max_per_label", 8))),
            candidate_limit=max(1, int(args.get("candidate_limit", 512))),
        )
    except Exception:
        return _semantic_entities_unavailable("inference_failed")


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
    "rank_paragraphs": op_rank_paragraphs,
    "extract_entities": op_extract_entities,
    "ettin_spans": op_ettin_spans,
}


def _serialize(response: dict) -> str:
    """Render one response line, raising rather than emitting anything the
    Rust reader cannot parse.

    `allow_nan=False` because serde_json rejects bare NaN/Infinity: sidecar.rs
    would skip the line as noise and the caller would sit out its full 45 s
    timeout. The explicit encode is for lone surrogates, which a broken PDF
    ToUnicode CMap produces -- `json.dumps` is happy to emit one, and the
    UnicodeEncodeError then landed on the write, outside the request handler.
    """
    text = json.dumps(response, ensure_ascii=False, allow_nan=False)
    text.encode("utf-8")
    return text + "\n"


MAX_REQUEST_ID = (1 << 64) - 1
PROTOCOL_ERROR_ID = 0


def _validate_request_id(request: object) -> int:
    if not isinstance(request, dict):
        raise ValueError("request must be a JSON object")
    value = request.get("id")
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or value < 0
        or value > MAX_REQUEST_ID
    ):
        raise ValueError("request id must be an unsigned 64-bit integer")
    return value


def handle_line(line: str) -> str:
    """Turn one request line into exactly one response line. Never raises.

    Serialization belongs inside the failure handler: it used to sit outside
    it, so one bad character killed the process, sidecar.rs saw the stream
    close and reported RUNTIME_FAIL. That is deterministic per document, so
    all three retry rungs died identically and the resident RapidOCR/Lingua/
    magika state this warm process exists to amortize was lost every time.
    """
    # Zero is reserved for errors whose malformed ID cannot safely be echoed
    # into the Rust u64 response field. Normal app requests start at one.
    request_id = PROTOCOL_ERROR_ID
    try:
        request = json.loads(line)
        request_id = _validate_request_id(request)
        operation = request.get("op", "")
        handler = OPS.get(operation)
        if handler is None:
            raise ValueError(f"unknown op '{operation}'")
        response = {"id": request_id, "ok": True}
        response.update(handler(request))
        return _serialize(response)
    except Exception as error:
        failure = {
            "id": request_id,
            "ok": False,
            "error": f"{error.__class__.__name__}: {error}",
            "trace": traceback.format_exc(limit=3),
        }

    try:
        return _serialize(failure)
    except Exception:
        # The diagnostic itself was unserializable -- it can quote the very
        # bytes that broke the response. An answer the caller can parse now
        # beats a better one it only gets after a 45 s timeout.
        return json.dumps({"id": request_id, "ok": False, "error": "unserializable response"}) + "\n"


def _claim_stdout():
    """Take fd 1 away from everything that is not the protocol.

    requirements ships colorlog and tqdm (both pulled in by rapidocr, whose
    config defaults to log_level: info) plus onnxruntime and Pillow. One naive
    write spliced into a response line makes sidecar.rs discard the line as
    noise, and the caller then waits its full 45 s timeout before killing a
    warm process that was working. Duplicating fd 1 and pointing the original
    at stderr turns those writes into mere logging.

    Falls back to the raw buffer if the dance is impossible (a stdio handle
    the host did not give us); a sidecar that starts and logs to the wrong
    stream beats one that will not start.
    """
    try:
        protocol_fd = os.dup(1)
        os.dup2(sys.stderr.fileno(), 1)
        return os.fdopen(protocol_fd, "wb")
    except (OSError, AttributeError, ValueError):
        return sys.stdout.buffer


def main() -> None:
    stdin = io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8", errors="replace")
    stdout = io.TextIOWrapper(
        _claim_stdout(),
        encoding="utf-8",
        # Belt and braces behind _serialize: a write must never raise out of
        # the loop, because that ends the process.
        errors="replace",
        line_buffering=True,
    )
    for line in stdin:
        line = line.strip()
        if not line:
            continue
        stdout.write(handle_line(line))


if __name__ == "__main__":
    main()
