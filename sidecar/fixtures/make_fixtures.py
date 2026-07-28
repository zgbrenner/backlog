#!/usr/bin/env python3
"""Regenerate the three sidecar conversion fixtures.

They exist so the build gate and the unit tests exercise a real document
through each lane -- Native/markitdown, PDF, and scanned/OCR -- instead of
`ping`, which returns {} and touches none of the lazily imported machinery.
That is how a build with a missed hidden import or an uncollected ONNX data
file used to pass and then fail on the customer's first real document.

Everything here is written with the standard library on purpose: the fixture
generator must not need the very parsers the fixtures are meant to prove are
installed. Output is byte-deterministic, so regenerating produces no diff
unless the content actually changed.

    python3 sidecar/fixtures/make_fixtures.py
"""

from __future__ import annotations

import struct
import zipfile
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent

# Both fixtures carry a creation date well before their modification date, so
# a test can prove `doc_meta_dates` reports only the former (see
# `_doc_meta_date_entries` in convertd.py).
CREATED = "2019-03-04"
MODIFIED = "2026-03-14"

DOCX_SENTENCE = (
    "Services Agreement dated 4 March 2019 between Northgate Holdings "
    "and Riverside Facilities Limited."
)
PDF_SENTENCE = "Invoice 2019-03-04 Northgate Holdings"
PNG_TEXT = "INVOICE 2019"


# ---------------------------------------------------------------------------
# DOCX
# ---------------------------------------------------------------------------

_CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
<Override PartName="/docProps/core.xml" ContentType="application/vnd.openxmlformats-package.core-properties+xml"/>
</Types>
"""

_ROOT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
<Relationship Id="rId2" Type="http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties" Target="docProps/core.xml"/>
</Relationships>
"""

_CORE_XML = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties
 xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties"
 xmlns:dc="http://purl.org/dc/elements/1.1/"
 xmlns:dcterms="http://purl.org/dc/terms/"
 xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<dc:title>Services Agreement</dc:title>
<dcterms:created xsi:type="dcterms:W3CDTF">{CREATED}T09:12:00Z</dcterms:created>
<dcterms:modified xsi:type="dcterms:W3CDTF">{MODIFIED}T16:40:00Z</dcterms:modified>
</cp:coreProperties>
"""

_DOCUMENT_XML = f"""<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body><w:p><w:r><w:t>{DOCX_SENTENCE}</w:t></w:r></w:p></w:body>
</w:document>
"""


def write_docx(destination: Path) -> None:
    parts = [
        ("[Content_Types].xml", _CONTENT_TYPES),
        ("_rels/.rels", _ROOT_RELS),
        ("docProps/core.xml", _CORE_XML),
        ("word/document.xml", _DOCUMENT_XML),
    ]
    with zipfile.ZipFile(destination, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, text in parts:
            # A fixed timestamp keeps the bytes reproducible across runs.
            info = zipfile.ZipInfo(name, date_time=(2019, 3, 4, 9, 12, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o600 << 16
            archive.writestr(info, text)


# ---------------------------------------------------------------------------
# PDF
# ---------------------------------------------------------------------------


def write_pdf(destination: Path) -> None:
    """A one-page PDF with a real text layer, assembled by hand.

    Uncompressed Helvetica in a single content stream: the point is that
    pdfminer/pdfplumber and pdfium both find extractable text, which is what
    routes it to Native rather than Scanned.
    """
    stream = (
        "BT /F1 18 Tf 72 700 Td (%s) Tj ET\n"
        "BT /F1 12 Tf 72 660 Td (Payment due within 30 days.) Tj ET\n"
    ) % PDF_SENTENCE
    objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
        "/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        f"<< /Length {len(stream)} >>\nstream\n{stream}endstream",
        f"<< /CreationDate (D:{CREATED.replace('-', '')}091200Z) "
        f"/ModDate (D:{MODIFIED.replace('-', '')}164000Z) "
        "/Producer (BackLog fixture generator) >>",
    ]

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for number, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{number} 0 obj\n{body}\nendobj\n".encode("latin-1")

    xref_at = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode("latin-1")
    out += b"0000000000 65535 f \n"
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode("latin-1")
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R /Info 6 0 R >>\n"
        f"startxref\n{xref_at}\n%%EOF\n"
    ).encode("latin-1")
    destination.write_bytes(bytes(out))


# ---------------------------------------------------------------------------
# PNG
# ---------------------------------------------------------------------------

# A 5x7 bitmap font, scaled up hard so the result reads like a clean fax
# rather than an eye chart. Only the glyphs PNG_TEXT needs are defined.
_FONT = {
    "I": ("11111", "00100", "00100", "00100", "00100", "00100", "11111"),
    "N": ("10001", "11001", "11001", "10101", "10011", "10011", "10001"),
    "V": ("10001", "10001", "10001", "10001", "10001", "01010", "00100"),
    "O": ("01110", "10001", "10001", "10001", "10001", "10001", "01110"),
    "C": ("01110", "10001", "10000", "10000", "10000", "10001", "01110"),
    "E": ("11111", "10000", "10000", "11110", "10000", "10000", "11111"),
    "0": ("01110", "10001", "10011", "10101", "11001", "10001", "01110"),
    "1": ("00100", "01100", "00100", "00100", "00100", "00100", "01110"),
    "2": ("01110", "10001", "00001", "00010", "00100", "01000", "11111"),
    "9": ("01110", "10001", "10001", "01111", "00001", "00001", "01110"),
    " ": ("00000",) * 7,
}

_SCALE = 10
_MARGIN = 24
_TRACKING = 2  # blank font columns between glyphs


def _chunk(tag: bytes, payload: bytes) -> bytes:
    return (
        struct.pack(">I", len(payload))
        + tag
        + payload
        + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)
    )


def write_png(destination: Path) -> None:
    glyph_columns = sum(len(_FONT[c][0]) + _TRACKING for c in PNG_TEXT) - _TRACKING
    width = glyph_columns * _SCALE + 2 * _MARGIN
    height = 7 * _SCALE + 2 * _MARGIN

    # 8-bit greyscale, white page, black ink -- what a bilevel scan looks like
    # once the driver has smoothed it.
    rows = [bytearray(b"\xff" * width) for _ in range(height)]
    pen = _MARGIN
    for character in PNG_TEXT:
        glyph = _FONT[character]
        for row_index, row_bits in enumerate(glyph):
            for column_index, bit in enumerate(row_bits):
                if bit != "1":
                    continue
                top = _MARGIN + row_index * _SCALE
                left = pen + column_index * _SCALE
                for y in range(top, top + _SCALE):
                    rows[y][left : left + _SCALE] = b"\x00" * _SCALE
        pen += (len(glyph[0]) + _TRACKING) * _SCALE

    raw = b"".join(b"\x00" + bytes(row) for row in rows)  # filter type 0
    png = b"\x89PNG\r\n\x1a\n"
    png += _chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 0, 0, 0, 0))
    png += _chunk(b"IDAT", zlib.compress(raw, 9))
    png += _chunk(b"IEND", b"")
    destination.write_bytes(png)


def main() -> None:
    write_docx(HERE / "sample_letter.docx")
    write_pdf(HERE / "sample_text.pdf")
    write_png(HERE / "sample_scan.png")
    for name in ("sample_letter.docx", "sample_text.pdf", "sample_scan.png"):
        print(f"{name}: {(HERE / name).stat().st_size} bytes")


if __name__ == "__main__":
    main()
