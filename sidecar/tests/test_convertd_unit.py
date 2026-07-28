import importlib.util
import io
import json
import math
import os
import struct
import subprocess
import sys
import tempfile
import types
import unittest
import zipfile
import zlib
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "sidecar" / "convertd.py"
FIXTURES = ROOT / "sidecar" / "fixtures"
SPEC = importlib.util.spec_from_file_location("backlog_convertd", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
CONVERTD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONVERTD)


def _installed(*modules: str) -> bool:
    for module in modules:
        try:
            if importlib.util.find_spec(module) is None:
                return False
        except (ImportError, ValueError):
            return False
    return True


def _run_driver(ops_source: str, requests: list[str], *, timeout: int = 20):
    """Run convertd's real main() in a subprocess with extra ops registered.

    The point of these tests is what happens to the PROCESS, not to a
    function: a response that cannot be serialized used to end it. `ops_source`
    is executed against the freshly imported module (bound as `mod`) so a test
    can register an op producing exactly the payload it wants to survive.
    """
    with tempfile.TemporaryDirectory() as tmp:
        driver = Path(tmp) / "driver.py"
        driver.write_text(
            "import importlib.util\n"
            f"spec = importlib.util.spec_from_file_location('backlog_convertd', {str(MODULE_PATH)!r})\n"
            "mod = importlib.util.module_from_spec(spec)\n"
            "spec.loader.exec_module(mod)\n"
            f"{ops_source}\n"
            "mod.main()\n",
            encoding="utf-8",
        )
        return subprocess.run(
            [sys.executable, str(driver)],
            input="\n".join(requests) + "\n",
            text=True,
            capture_output=True,
            timeout=timeout,
            check=True,
        )


def _select_frames(image, head: int, tail: int) -> list:
    """Drive convertd's frame selector against a stand-in PIL."""
    sequence = types.ModuleType("PIL.ImageSequence")
    sequence.Iterator = lambda img: iter(img.frames)
    package = types.ModuleType("PIL")
    package.ImageSequence = sequence
    with mock.patch.dict(sys.modules, {"PIL": package, "PIL.ImageSequence": sequence}):
        return list(CONVERTD._select_frames(image, head, tail))


class ArrayLikeScores:
    def __init__(self, values):
        self.values = values

    def __iter__(self):
        return iter(self.values)

    def __bool__(self):
        raise AssertionError("score arrays must not be coerced to bool")


class PageSelectionTests(unittest.TestCase):
    def test_page_indices_keep_head_and_tail_without_duplicates(self):
        self.assertEqual(CONVERTD._page_indices(20, 3, 2), [0, 1, 2, 18, 19])

    def test_page_indices_return_every_page_for_short_document(self):
        self.assertEqual(CONVERTD._page_indices(4, 10, 3), [0, 1, 2, 3])


class DateNormalizationTests(unittest.TestCase):
    def test_normalizes_supported_date_formats(self):
        self.assertEqual(CONVERTD._normalize_span_date("July 20, 2026"), "2026-07-20")
        self.assertEqual(CONVERTD._normalize_span_date("20 Jul 2026"), "2026-07-20")
        self.assertEqual(CONVERTD._normalize_span_date("3rd March 2025"), "2025-03-03")

    def test_rejects_impossible_date(self):
        self.assertIsNone(CONVERTD._normalize_span_date("February 30, 2026"))


class PacketHeuristicTests(unittest.TestCase):
    def test_letterhead_resets_count_additional_openings(self):
        text = "Dear Jane\nBody\n\nTo Whom It May Concern\nBody\n\nRE: Third item"
        self.assertEqual(CONVERTD._letterhead_resets(text), 2)


class RapidOcrCompatibilityTests(unittest.TestCase):
    def test_extracts_lines_from_rapidocr_three_result_object(self):
        result = SimpleNamespace(txts=["First", "Second"], scores=[0.91, 0.82])
        self.assertEqual(
            CONVERTD._rapidocr_lines(result),
            [("First", 0.91), ("Second", 0.82)],
        )

    def test_does_not_boolean_coerce_array_like_scores(self):
        result = SimpleNamespace(
            txts=["First", "Second"],
            scores=ArrayLikeScores([0.91, 0.82]),
        )
        self.assertEqual(
            CONVERTD._rapidocr_lines(result),
            [("First", 0.91), ("Second", 0.82)],
        )

    def test_extracts_lines_from_legacy_tuple_result(self):
        result = (
            [
                ([[0, 0], [1, 0], [1, 1], [0, 1]], "Legacy", 0.75),
                ([[0, 2], [1, 2], [1, 3], [0, 3]], "Output", 0.65),
            ],
            0.03,
        )
        self.assertEqual(
            CONVERTD._rapidocr_lines(result),
            [("Legacy", 0.75), ("Output", 0.65)],
        )

    def test_empty_ocr_result_yields_no_lines(self):
        self.assertEqual(CONVERTD._rapidocr_lines(None), [])
        self.assertEqual(
            CONVERTD._rapidocr_lines(SimpleNamespace(txts=None, scores=None)),
            [],
        )

    def test_zero_dpi_selects_enhanced_classical_ocr(self):
        self.assertEqual(CONVERTD._ocr_profile(0), (600, True))
        self.assertEqual(CONVERTD._ocr_profile(300), (300, False))


class LanguageIdentificationTests(unittest.TestCase):
    def test_language_code_returns_lowercase_iso_639_1(self):
        language = SimpleNamespace(
            iso_code_639_1=SimpleNamespace(name="DA"),
            name="DANISH",
        )
        self.assertEqual(CONVERTD._language_code(language), "da")

    def test_language_code_falls_back_to_english(self):
        self.assertEqual(CONVERTD._language_code(None), "en")
        self.assertEqual(
            CONVERTD._language_code(SimpleNamespace(iso_code_639_1=None)),
            "en",
        )


class OptionalLoaderCacheTests(unittest.TestCase):
    """`_get_optional` backs the slim-sidecar graceful-degradation contract:
    a missing library or model must degrade the op, never crash the
    process, and must not retry the expensive import on every request."""

    def test_caches_a_failing_factory_as_unavailable_without_retrying(self):
        CONVERTD._CACHE.clear()
        calls = {"n": 0}

        def flaky_factory():
            calls["n"] += 1
            raise ImportError("no module named gliclass")

        self.assertIsNone(CONVERTD._get_optional("flaky", flaky_factory))
        self.assertIsNone(CONVERTD._get_optional("flaky", flaky_factory))
        self.assertEqual(calls["n"], 1)
        CONVERTD._CACHE.clear()

    def test_a_successful_factory_is_reused_and_returned(self):
        CONVERTD._CACHE.clear()
        sentinel = object()
        self.assertIs(CONVERTD._get_optional("ok", lambda: sentinel), sentinel)
        self.assertIs(CONVERTD._get_optional("ok", lambda: (_ for _ in ()).throw(AssertionError("must not re-call"))), sentinel)
        CONVERTD._CACHE.clear()


class ClassifyGracefulDegradationTests(unittest.TestCase):
    """When gliclass/transformers are absent (the slim sidecar profile) or
    the local snapshot fails to load, classify must return ok-shaped
    output with a neutral default -- never raise -- so the Rust pipeline
    never flags a document over a missing naming enhancement."""

    def test_falls_back_to_correspondence_when_gliclass_unavailable(self):
        with mock.patch.object(CONVERTD, "_gliclass", return_value=None):
            result = CONVERTD.op_classify(
                {"text": "some text", "labels": ["invoice", "correspondence"]}
            )
        self.assertEqual(result["label"], "correspondence")
        self.assertEqual(result["score"], 0.0)
        self.assertFalse(result["available"])

    def test_falls_back_to_first_label_when_correspondence_not_offered(self):
        with mock.patch.object(CONVERTD, "_gliclass", return_value=None):
            result = CONVERTD.op_classify({"text": "x", "labels": ["invoice", "receipt"]})
        self.assertEqual(result["label"], "invoice")
        self.assertFalse(result["available"])

    def test_falls_back_when_a_live_pipeline_raises_mid_inference(self):
        def broken_pipeline(*_args, **_kwargs):
            raise RuntimeError("corrupt snapshot")

        with mock.patch.object(CONVERTD, "_gliclass", return_value=broken_pipeline):
            result = CONVERTD.op_classify({"text": "x", "labels": ["correspondence"]})
        self.assertEqual(result["label"], "correspondence")
        self.assertFalse(result["available"])

    def test_uses_the_live_pipeline_result_when_available(self):
        def fake_pipeline(_text, _labels, threshold=0.0):
            return [[{"label": "invoice", "score": 0.9}, {"label": "receipt", "score": 0.4}]]

        with mock.patch.object(CONVERTD, "_gliclass", return_value=fake_pipeline):
            result = CONVERTD.op_classify({"text": "x", "labels": ["invoice", "receipt"]})
        self.assertEqual(result["label"], "invoice")
        self.assertEqual(result["score"], 0.9)
        self.assertTrue(result["available"])


class SalienceGracefulDegradationTests(unittest.TestCase):
    """When sentence-transformers/granite are absent or fail to load,
    salience must return the first top_k sentence indices in document
    order -- never raise."""

    def test_falls_back_to_document_order_when_granite_unavailable(self):
        sentences = [f"sentence {i}" for i in range(5)]
        with mock.patch.object(CONVERTD, "_granite", return_value=None):
            result = CONVERTD.op_salience({"sentences": sentences, "top_k": 3})
        self.assertEqual(result["indices"], [0, 1, 2])
        self.assertFalse(result["available"])

    def test_empty_sentences_short_circuits_before_loading_a_model(self):
        with mock.patch.object(
            CONVERTD,
            "_granite",
            side_effect=AssertionError("must not load a model for no sentences"),
        ):
            result = CONVERTD.op_salience({"sentences": [], "top_k": 3})
        self.assertEqual(result, {"indices": []})

    def test_falls_back_when_a_live_model_raises_mid_inference(self):
        class BrokenModel:
            def encode(self, *_args, **_kwargs):
                raise RuntimeError("corrupt snapshot")

        sentences = ["a", "b", "c", "d"]
        with mock.patch.object(CONVERTD, "_granite", return_value=BrokenModel()):
            result = CONVERTD.op_salience({"sentences": sentences, "top_k": 2})
        self.assertEqual(result["indices"], [0, 1])
        self.assertFalse(result["available"])


class EttinGracefulDegradationTests(unittest.TestCase):
    """Already-blank BACKLOG_ETTIN_DIR degrades to no spans; a configured
    but broken extractor must degrade the same way rather than raise."""

    def test_no_spans_when_ettin_unavailable(self):
        with mock.patch.object(CONVERTD, "_ettin", return_value=None):
            self.assertEqual(CONVERTD.op_ettin_spans({"text": "some text"}), {"spans": []})

    def test_no_spans_when_a_live_extractor_raises_mid_inference(self):
        def broken_extractor(_text):
            raise RuntimeError("corrupt snapshot")

        with mock.patch.object(CONVERTD, "_ettin", return_value=broken_extractor):
            self.assertEqual(CONVERTD.op_ettin_spans({"text": "some text"}), {"spans": []})


class ProtocolTests(unittest.TestCase):
    def test_unknown_operation_returns_structured_error_without_loading_models(self):
        request = json.dumps({"id": 9, "op": "does_not_exist"}) + "\n"
        completed = subprocess.run(
            [sys.executable, str(MODULE_PATH)],
            input=request,
            text=True,
            capture_output=True,
            timeout=10,
            check=True,
        )
        response = json.loads(completed.stdout.strip())
        self.assertEqual(response["id"], 9)
        self.assertFalse(response["ok"])
        self.assertIn("unknown op", response["error"])

    def test_ping_answers_without_loading_any_model(self):
        request = json.dumps({"id": 1, "op": "ping"}) + "\n"
        completed = subprocess.run(
            [sys.executable, str(MODULE_PATH)],
            input=request,
            text=True,
            capture_output=True,
            timeout=10,
            check=True,
        )
        response = json.loads(completed.stdout.strip())
        self.assertEqual(response, {"id": 1, "ok": True})

    def test_classify_is_ok_true_end_to_end_without_a_local_model_snapshot(self):
        # No models/gliclass-base-v3.0 snapshot exists in this checkout (model
        # weights are gitignored), so this exercises the real degrade path
        # end-to-end through the NDJSON protocol regardless of whether the
        # interpreter running this test happens to have gliclass installed.
        request = json.dumps(
            {"id": 2, "op": "classify", "text": "Dear Sir,", "labels": ["invoice", "correspondence"]}
        ) + "\n"
        completed = subprocess.run(
            [sys.executable, str(MODULE_PATH)],
            input=request,
            text=True,
            capture_output=True,
            timeout=30,
            check=True,
        )
        response = json.loads(completed.stdout.strip())
        self.assertTrue(response["ok"])
        self.assertEqual(response["label"], "correspondence")
        self.assertEqual(response["score"], 0.0)
        self.assertFalse(response["available"])

    def test_salience_is_ok_true_end_to_end_without_a_local_model_snapshot(self):
        sentences = [f"sentence {i}" for i in range(5)]
        request = json.dumps({"id": 3, "op": "salience", "sentences": sentences, "top_k": 3}) + "\n"
        completed = subprocess.run(
            [sys.executable, str(MODULE_PATH)],
            input=request,
            text=True,
            capture_output=True,
            timeout=30,
            check=True,
        )
        response = json.loads(completed.stdout.strip())
        self.assertTrue(response["ok"])
        self.assertEqual(response["indices"], [0, 1, 2])
        self.assertFalse(response["available"])

    def test_ettin_spans_is_ok_true_end_to_end_without_backlog_ettin_dir(self):
        request = json.dumps({"id": 4, "op": "ettin_spans", "text": "some text"}) + "\n"
        completed = subprocess.run(
            [sys.executable, str(MODULE_PATH)],
            input=request,
            text=True,
            capture_output=True,
            timeout=30,
            check=True,
        )
        response = json.loads(completed.stdout.strip())
        self.assertTrue(response["ok"])
        self.assertEqual(response["spans"], [])


class SalienceNeedsNoArrayLibraryTests(unittest.TestCase):
    """The deterministic fallback must not merely happen to work on a box
    where numpy is installed. Shadow numpy with a module that refuses to
    import, so the op is provably taking a numpy-free path."""

    def test_fallback_survives_an_unimportable_numpy(self):
        with tempfile.TemporaryDirectory() as tmp:
            (Path(tmp) / "numpy.py").write_text(
                "raise ImportError('numpy is deliberately unavailable')\n",
                encoding="utf-8",
            )
            request = json.dumps(
                {"id": 7, "op": "salience", "sentences": ["a", "b", "c"], "top_k": 2}
            )
            completed = subprocess.run(
                [sys.executable, str(MODULE_PATH)],
                input=request + "\n",
                text=True,
                capture_output=True,
                timeout=30,
                check=True,
                env={**os.environ, "PYTHONPATH": tmp},
            )
        response = json.loads(completed.stdout.strip())
        self.assertTrue(response["ok"], response)
        self.assertEqual(response["indices"], [0, 1])
        self.assertFalse(response["available"])


class ResponseSerializationTests(unittest.TestCase):
    """Response serialization used to sit outside main()'s handler, so one
    bad character killed a warm process that all three retry rungs then hit
    identically."""

    def _handle(self, payload):
        with mock.patch.dict(CONVERTD.OPS, {"boom": lambda _args: payload}):
            return json.loads(CONVERTD.handle_line(json.dumps({"id": 5, "op": "boom"})))

    def test_nan_becomes_a_structured_failure_not_a_bare_nan_token(self):
        response = self._handle({"ocr_mean_conf": float("nan")})
        self.assertEqual(response["id"], 5)
        self.assertFalse(response["ok"])

    def test_infinity_becomes_a_structured_failure(self):
        self.assertFalse(self._handle({"score": float("inf")})["ok"])

    def test_lone_surrogate_becomes_a_well_formed_failure_line(self):
        response = self._handle({"markdown": "before " + chr(0xD800) + " after"})
        self.assertEqual(response["id"], 5)
        self.assertFalse(response["ok"])

    def test_every_response_line_is_utf8_encodable(self):
        line = CONVERTD.handle_line(json.dumps({"id": 6, "op": "ping"}))
        line.encode("utf-8")
        self.assertEqual(json.loads(line), {"id": 6, "ok": True})

    def test_a_surrogate_does_not_end_the_process(self):
        completed = _run_driver(
            "mod.OPS['boom'] = lambda args: {'markdown': 'x' + chr(0xD800) + 'y'}",
            [json.dumps({"id": 1, "op": "boom"}), json.dumps({"id": 2, "op": "ping"})],
        )
        lines = [json.loads(line) for line in completed.stdout.splitlines() if line.strip()]
        self.assertEqual([line["id"] for line in lines], [1, 2])
        self.assertFalse(lines[0]["ok"])
        self.assertTrue(lines[1]["ok"])


class StdoutIsolationTests(unittest.TestCase):
    def test_a_library_printing_to_stdout_cannot_corrupt_a_response(self):
        # rapidocr defaults to log_level: info and pulls colorlog/tqdm; that
        # chatter must land on stderr, not spliced into a response line.
        completed = _run_driver(
            "mod.OPS['chatty'] = lambda args: (print('naive library chatter'), {'x': 1})[1]",
            [json.dumps({"id": 1, "op": "chatty"})],
        )
        lines = [line for line in completed.stdout.splitlines() if line.strip()]
        self.assertEqual(len(lines), 1, completed.stdout)
        self.assertEqual(json.loads(lines[0]), {"id": 1, "ok": True, "x": 1})
        self.assertIn("naive library chatter", completed.stderr)


class MarkdownCeilingTests(unittest.TestCase):
    def test_oversized_markdown_is_capped_on_every_route(self):
        result = CONVERTD._conversion_result("missing.docx", "z" * 900_000)
        self.assertLessEqual(
            len(result["markdown"]),
            CONVERTD.MAX_MARKDOWN_CHARS + len(CONVERTD._ELISION),
        )
        self.assertIn("[...]", result["markdown"])

    def test_short_markdown_is_untouched(self):
        self.assertEqual(CONVERTD._cap_markdown("short"), "short")

    def test_non_finite_ocr_confidence_is_clamped(self):
        for value in (float("nan"), float("inf")):
            result = CONVERTD._conversion_result("missing.pdf", "x", ocr_mean_conf=value)
            self.assertEqual(result["ocr_mean_conf"], 0.0)
            self.assertTrue(math.isfinite(result["ocr_mean_conf"]))


class PdfTruncationTests(unittest.TestCase):
    """`max_tail_pages` is a user-editable Settings field, so 0 and
    absurdly-large values both reach this code."""

    MARKDOWN = "abcdefghij" * 6000  # 60 000 chars, over the 40 000 threshold

    def test_zero_tail_pages_does_not_append_the_whole_document(self):
        out = CONVERTD._truncate_pdf_markdown(self.MARKDOWN, 60, 10, 0)
        self.assertLess(len(out), len(self.MARKDOWN))
        self.assertTrue(out.endswith("[...]\n\n"))

    def test_tail_beyond_the_page_count_leaves_the_document_alone(self):
        out = CONVERTD._truncate_pdf_markdown(self.MARKDOWN, 12, 10, 99)
        self.assertEqual(out, self.MARKDOWN)

    def test_head_and_tail_slices_are_both_kept(self):
        out = CONVERTD._truncate_pdf_markdown(self.MARKDOWN, 60, 10, 3)
        self.assertLess(len(out), len(self.MARKDOWN))
        self.assertTrue(out.startswith(self.MARKDOWN[:1000]))
        self.assertTrue(out.endswith(self.MARKDOWN[-1000:]))

    def test_short_documents_are_never_truncated(self):
        self.assertEqual(CONVERTD._truncate_pdf_markdown("tiny", 1, 10, 3), "tiny")


class RenderScaleTests(unittest.TestCase):
    def test_ordinary_page_renders_at_the_requested_dpi(self):
        self.assertAlmostEqual(CONVERTD._render_scale(612, 792, 300), 300 / 72.0)

    def test_huge_page_is_clamped_to_the_pixel_budget(self):
        # ISO A0 (3370 x 2384 pt) at 600 DPI is ~1.4 gigapixels of RGB.
        scale = CONVERTD._render_scale(3370, 2384, 600)
        self.assertLess(scale, 600 / 72.0)
        self.assertLessEqual(3370 * 2384 * scale * scale, CONVERTD.MAX_RENDER_PIXELS + 1)


class MultiFrameImageTests(unittest.TestCase):
    """Multi-page TIFF is the standard fax/scan container here; reading frame
    0 only named a 10-page agreement from its cover sheet."""

    class _Frame:
        def __init__(self, index, size=(1700, 2200)):
            self.index = index
            self.size = size
            self.resized_to = None

        def resize(self, size):
            self.resized_to = size
            return MultiFrameImageTests._Frame(self.index, size)

        def convert(self, _mode):
            if self.size[0] * self.size[1] > CONVERTD.MAX_RENDER_PIXELS:
                raise AssertionError("converted a frame past the pixel budget")
            return f"frame-{self.index}"

    class _Image:
        def __init__(self, frames, expose_n_frames=True, size=(1700, 2200)):
            self.frames = [
                MultiFrameImageTests._Frame(i, size) for i in range(frames)
            ]
            if expose_n_frames:
                self.n_frames = frames

    def _select(self, image, head, tail):
        return _select_frames(image, head, tail)

    def test_head_and_tail_frames_are_read_from_a_multi_page_tiff(self):
        self.assertEqual(
            self._select(self._Image(10), 2, 1),
            ["frame-0", "frame-1", "frame-9"],
        )

    def test_every_frame_is_read_when_the_document_is_short(self):
        self.assertEqual(self._select(self._Image(3), 10, 3), ["frame-0", "frame-1", "frame-2"])

    def test_single_frame_image_without_n_frames_still_yields_its_page(self):
        self.assertEqual(
            self._select(self._Image(1, expose_n_frames=False), 10, 3),
            ["frame-0"],
        )


class ImagePixelBudgetTests(unittest.TestCase):
    """Route::Scanned takes arbitrary drops from the intake library. A PDF
    page is clamped to MAX_RENDER_PIXELS by _render_scale; a PNG went to
    np.array() at whatever size it claimed, ~510 MB per copy per frame."""

    class _Frame:
        def __init__(self, size, drafts_to=None):
            self.size = size
            self.drafts_to = drafts_to
            self.resized_to = None

        def draft(self, _mode, size):
            if self.drafts_to is not None:
                self.size = self.drafts_to(size)

        def resize(self, size):
            self.resized_to = size
            return ImagePixelBudgetTests._Frame(size)

    def test_a_frame_within_the_budget_is_handed_on_untouched(self):
        frame = self._Frame((1700, 2200))
        self.assertIs(CONVERTD._fit_pixel_budget(frame), frame)
        self.assertIsNone(frame.resized_to)

    def test_an_oversized_frame_is_downscaled_before_anything_expands_it(self):
        # ~170 MP: past Pillow's warn threshold, under its 2x error threshold,
        # so nothing else in the stack would have stopped it.
        frame = self._Frame((13000, 13000))
        fitted = CONVERTD._fit_pixel_budget(frame)
        self.assertIsNot(fitted, frame)
        self.assertLessEqual(
            fitted.size[0] * fitted.size[1], CONVERTD.MAX_RENDER_PIXELS
        )
        self.assertEqual(frame.resized_to, fitted.size)

    def test_a_jpeg_that_can_draft_itself_smaller_is_not_resized_again(self):
        frame = self._Frame((13000, 13000), drafts_to=lambda size: size)
        fitted = CONVERTD._fit_pixel_budget(frame)
        self.assertIs(fitted, frame)
        self.assertLessEqual(
            fitted.size[0] * fitted.size[1], CONVERTD.MAX_RENDER_PIXELS
        )
        self.assertIsNone(frame.resized_to)

    def test_the_frame_selector_applies_the_budget_to_every_frame(self):
        # _Frame.convert() raises if it is ever called past the budget, which
        # is exactly what op_ocr then feeds to np.array().
        image = MultiFrameImageTests._Image(3, size=(13000, 13000))
        self.assertEqual(
            _select_frames(image, 10, 3), ["frame-0", "frame-1", "frame-2"]
        )
        self.assertTrue(all(frame.resized_to for frame in image.frames))

    def test_both_branches_of_op_ocr_share_one_bound(self):
        scale = CONVERTD._render_scale(3370, 2384, 600)
        self.assertLessEqual(3370 * 2384 * scale * scale, CONVERTD.MAX_RENDER_PIXELS + 1)
        fitted = CONVERTD._fit_pixel_budget(self._Frame((13000, 13000)))
        self.assertLessEqual(
            fitted.size[0] * fitted.size[1], CONVERTD.MAX_RENDER_PIXELS
        )


def _compound_file(streams: list[str]) -> bytes:
    """A minimal but real v3 OLE compound file listing `streams`.

    Header, FAT and directory are all laid out properly so the reader under
    test has to walk them; nothing here is a byte pattern it could stumble on.
    """
    header = bytearray(b"\x00" * 512)
    header[0:8] = CONVERTD._OLE_CFB_MAGIC
    struct.pack_into("<H", header, 26, 3)  # major version
    struct.pack_into("<H", header, 28, 0xFFFE)  # little-endian
    struct.pack_into("<H", header, 30, 9)  # 512-byte sectors
    struct.pack_into("<H", header, 32, 6)  # 64-byte mini sectors
    struct.pack_into("<I", header, 44, 1)  # one FAT sector
    struct.pack_into("<I", header, 48, 1)  # directory starts at sector 1
    struct.pack_into("<I", header, 60, 0xFFFFFFFE)  # no mini FAT
    struct.pack_into("<I", header, 68, 0xFFFFFFFE)  # no extra DIFAT
    for index in range(109):
        struct.pack_into("<I", header, 76 + 4 * index, 0 if index == 0 else 0xFFFFFFFF)

    fat = bytearray(b"\xff" * 512)
    struct.pack_into("<I", fat, 0, 0xFFFFFFFD)  # sector 0 holds the FAT
    struct.pack_into("<I", fat, 4, 0xFFFFFFFE)  # the directory ends at sector 1

    names = ["Root Entry", *streams]
    directory = bytearray()
    for index, name in enumerate(names):
        entry = bytearray(b"\x00" * 128)
        encoded = name.encode("utf-16-le") + b"\x00\x00"
        entry[0 : len(encoded)] = encoded
        struct.pack_into("<H", entry, 64, len(encoded))
        entry[66] = 5 if index == 0 else 2  # root storage / stream
        entry[67] = 1  # black
        struct.pack_into("<I", entry, 68, 0xFFFFFFFF)  # no left sibling
        # The root points at the first stream; the streams chain right from
        # there, which is what a real CFB reader walks.
        right = index + 1 if 0 < index < len(names) - 1 else 0xFFFFFFFF
        struct.pack_into("<I", entry, 72, right)
        struct.pack_into("<I", entry, 76, 1 if index == 0 else 0xFFFFFFFF)
        struct.pack_into("<I", entry, 116, 0xFFFFFFFE)  # empty stream
        directory += entry
    directory += b"\x00" * (-len(directory) % 512)
    return bytes(header) + bytes(fat) + bytes(directory)


class EncryptedOfficeDetectionTests(unittest.TestCase):
    """A protected .docx raises BadZipFile('File is not a zip file'), which
    contains neither 'password' nor 'encrypt' -- but so does a legacy binary
    .doc somebody renamed .docx, and those two need opposite advice."""

    def _write(self, tmp, name, streams):
        path = Path(tmp) / name
        path.write_bytes(_compound_file(streams))
        return str(path)

    def test_an_encrypted_package_is_recognized_by_its_streams(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp, "contract.docx", ["EncryptionInfo", "EncryptedPackage"]
            )
            self.assertEqual(CONVERTD._ole_container_kind(path), "encrypted")
            self.assertTrue(CONVERTD._is_encrypted_ooxml(path))

    def test_a_renamed_legacy_word_file_is_not_called_encrypted(self):
        # Routine in an office backfill, and the old behaviour sent the user
        # off to find a password for a file that has none.
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp, "quarterly report.docx", ["WordDocument", "1Table", "SummaryInformation"]
            )
            self.assertEqual(CONVERTD._ole_container_kind(path), "legacy")
            self.assertFalse(CONVERTD._is_encrypted_ooxml(path))

    def test_a_renamed_legacy_workbook_is_not_called_encrypted(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "budget.xlsx", ["Workbook", "SummaryInformation"])
            self.assertEqual(CONVERTD._ole_container_kind(path), "legacy")

    def test_a_renamed_legacy_deck_is_not_called_encrypted(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp, "kickoff.pptx", ["PowerPoint Document", "Current User"]
            )
            self.assertEqual(CONVERTD._ole_container_kind(path), "legacy")

    def test_cfb_magic_alone_decides_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            truncated = Path(tmp) / "stub.docx"
            truncated.write_bytes(CONVERTD._OLE_CFB_MAGIC + b"\x00" * 512)
            self.assertIsNone(CONVERTD._ole_container_kind(str(truncated)))
            self.assertFalse(CONVERTD._is_encrypted_ooxml(str(truncated)))

    def test_a_real_docx_is_not_mistaken_for_an_encrypted_one(self):
        self.assertFalse(CONVERTD._is_encrypted_ooxml(str(FIXTURES / "sample_letter.docx")))

    def test_cfb_magic_outside_the_ooxml_family_is_ignored(self):
        with tempfile.TemporaryDirectory() as tmp:
            legacy = Path(tmp) / "old.pdf"
            legacy.write_bytes(_compound_file(["EncryptionInfo", "EncryptedPackage"]))
            self.assertIsNone(CONVERTD._ole_container_kind(str(legacy)))

    def test_op_convert_reports_encrypted_without_calling_markitdown(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(
                tmp, "contract.docx", ["EncryptionInfo", "EncryptedPackage"]
            )
            with mock.patch.object(
                CONVERTD, "_markitdown", side_effect=AssertionError("must not convert")
            ):
                result = CONVERTD.op_convert({"path": path})
        self.assertTrue(result["encrypted"])
        self.assertEqual(result["markdown"], "")
        self.assertEqual(result["doc_meta_dates"], [])

    def test_op_convert_still_offers_a_renamed_legacy_file_to_markitdown(self):
        # markitdown's XLS branch matches the sniffed mime type, not the
        # suffix, so a renamed .xls can still convert -- don't pre-empt it.
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "budget.xlsx", ["Workbook"])
            converter = mock.Mock()
            converter.convert.return_value = SimpleNamespace(text_content="| a | b |")
            with mock.patch.object(CONVERTD, "_markitdown", return_value=converter):
                result = CONVERTD.op_convert({"path": path})
        self.assertFalse(result["encrypted"])
        self.assertEqual(result["markdown"], "| a | b |")

    def test_a_failed_legacy_conversion_names_the_format_not_a_password(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write(tmp, "quarterly report.docx", ["WordDocument"])
            with mock.patch.object(
                CONVERTD,
                "_markitdown",
                side_effect=zipfile.BadZipFile("File is not a zip file"),
            ):
                with self.assertRaises(RuntimeError) as caught:
                    CONVERTD.op_convert({"path": path})
        message = str(caught.exception).lower()
        self.assertIn("legacy", message)
        # pipeline.rs's error_code() maps either word straight to ENCRYPTED.
        self.assertNotIn("password", message)
        self.assertNotIn("encrypt", message)

    def test_plain_zip_failure_is_not_reported_as_encrypted(self):
        self.assertFalse(CONVERTD._looks_encrypted(zipfile.BadZipFile("File is not a zip file")))

    def test_a_path_containing_the_word_password_is_not_evidence(self):
        # OSError's repr omits the filename; str() does not, which is exactly
        # why the check reads repr().
        error = FileNotFoundError(2, "No such file or directory", "/in/password-reset.pdf")
        self.assertFalse(CONVERTD._looks_encrypted(error))

    def test_a_wrapped_cause_counts_as_evidence(self):
        class PDFPasswordIncorrect(Exception):
            pass

        try:
            try:
                raise PDFPasswordIncorrect()
            except PDFPasswordIncorrect as cause:
                raise RuntimeError("conversion failed") from cause
        except RuntimeError as error:
            self.assertTrue(CONVERTD._looks_encrypted(error))

    def test_markitdown_conversion_attempts_are_walked(self):
        class FileConversionException(Exception):
            pass

        failure = FileConversionException("all attempts exhausted")
        failure.attempts = [
            SimpleNamespace(exc_info=(ValueError, ValueError("file is encrypted"), None))
        ]
        self.assertTrue(CONVERTD._looks_encrypted(failure))

    def test_op_convert_maps_an_encrypted_conversion_error_to_the_encrypted_flag(self):
        class EncryptedFileError(Exception):
            pass

        with mock.patch.object(
            CONVERTD, "_markitdown", side_effect=EncryptedFileError("nope")
        ):
            result = CONVERTD.op_convert({"path": "whatever.pdf"})
        self.assertTrue(result["encrypted"])

    def test_op_convert_still_raises_on_an_ordinary_failure(self):
        with mock.patch.object(CONVERTD, "_markitdown", side_effect=RuntimeError("corrupt")):
            with self.assertRaises(RuntimeError):
                CONVERTD.op_convert({"path": "whatever.pdf"})


class DocumentMetadataDateTests(unittest.TestCase):
    """The invariant is that no date ships unless it appears in the document
    text or file metadata. A modification timestamp is the weakest possible
    member of that set, so it must be tagged, not silently mixed in."""

    DOCX = FIXTURES / "sample_letter.docx"

    def test_entries_carry_the_property_each_date_came_from(self):
        self.assertEqual(
            CONVERTD._doc_meta_date_entries(str(self.DOCX)),
            [{"iso": "2019-03-04", "prop": "created"}, {"iso": "2026-03-14", "prop": "modified"}],
        )

    def test_the_flat_wire_list_carries_creation_dates_only(self):
        result = CONVERTD._conversion_result(str(self.DOCX), "body text")
        self.assertEqual(result["doc_meta_dates"], ["2019-03-04"])
        self.assertEqual(
            result["doc_meta_date_entries"],
            [{"iso": "2019-03-04", "prop": "created"}, {"iso": "2026-03-14", "prop": "modified"}],
        )

    def test_an_encrypted_result_reports_no_metadata_dates(self):
        result = CONVERTD._conversion_result(str(self.DOCX), "", encrypted=True)
        self.assertEqual(result["doc_meta_dates"], [])
        self.assertEqual(result["doc_meta_date_entries"], [])

    def test_unreadable_documents_yield_no_dates_rather_than_failing(self):
        self.assertEqual(CONVERTD._doc_meta_date_entries("no/such/file.docx"), [])


class ZipDecompressionBudgetTests(unittest.TestCase):
    """Anyone who can drop a file into the SharePoint intake library reaches
    this parser, and it runs three times per file across the retry ladder."""

    def _docx_with_core(self, tmp: str, payload: bytes) -> str:
        path = Path(tmp) / "bomb.docx"
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
            archive.writestr("docProps/core.xml", payload)
        return str(path)

    def _oversized_but_valid(self) -> bytes:
        """Well-formed core properties carrying a real date, past the budget.

        The date is the point: this payload parses, so an empty result can
        only come from the byte cap. Remove the cap and the assertions below
        fail loudly instead of passing on a ParseError.
        """
        padding = b"a" * (CONVERTD.MAX_ZIP_MEMBER_BYTES + 1024)
        return (
            b'<?xml version="1.0"?>'
            b'<cp:coreProperties xmlns:cp="urn:cp" xmlns:dcterms="urn:dcterms">'
            b"<dcterms:created>2019-03-04T09:12:00Z</dcterms:created>"
            b"<cp:keywords>" + padding + b"</cp:keywords>"
            b"</cp:coreProperties>"
        )

    def test_an_oversized_member_is_refused(self):
        payload = self._oversized_but_valid()
        # The payload is genuinely parseable and genuinely date-bearing.
        root = CONVERTD._parse_core_xml(payload)
        self.assertIsNotNone(root)
        self.assertIn(
            "2019-03-04", "".join(element.text or "" for element in root.iter())
        )
        with tempfile.TemporaryDirectory() as tmp:
            path = self._docx_with_core(tmp, payload)
            self.assertLess(Path(path).stat().st_size, 100_000)  # small on disk
            with zipfile.ZipFile(path) as archive:
                self.assertIsNone(
                    CONVERTD._read_zip_member(archive, "docProps/core.xml")
                )
            self.assertEqual(CONVERTD._doc_meta_date_entries(path), [])

    def test_a_small_member_with_an_absurd_compression_ratio_is_refused(self):
        payload = b"\x00" * (CONVERTD.MAX_ZIP_MEMBER_BYTES // 4)
        with tempfile.TemporaryDirectory() as tmp:
            path = self._docx_with_core(tmp, payload)
            with zipfile.ZipFile(path) as archive:
                info = archive.getinfo("docProps/core.xml")
                self.assertGreater(info.file_size / info.compress_size, CONVERTD.MAX_ZIP_RATIO)
                self.assertIsNone(CONVERTD._read_zip_member(archive, "docProps/core.xml"))

    def test_a_read_is_capped_even_when_the_header_lies_about_the_size(self):
        # ZipInfo is attacker-controlled metadata: understating file_size gets
        # a member past the budget check, so the read itself is bounded rather
        # than trusted to stop. A stand-in archive is the only way to see that
        # bound -- zipfile's own CRC check would mask it.
        class _LyingArchive:
            def getinfo(self, _name):
                return SimpleNamespace(file_size=32, compress_size=32)

            def open(self, _info):
                return io.BytesIO(b"x" * (CONVERTD.MAX_ZIP_MEMBER_BYTES * 4))

        self.assertIsNone(
            CONVERTD._read_zip_member(_LyingArchive(), "docProps/core.xml")
        )

    def test_a_lying_header_degrades_to_no_dates_rather_than_raising(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "liar.docx"
            with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
                archive.writestr("docProps/core.xml", b"x" * (CONVERTD.MAX_ZIP_MEMBER_BYTES + 8))
            raw = bytearray(path.read_bytes())
            honest = struct.pack("<I", CONVERTD.MAX_ZIP_MEMBER_BYTES + 8)
            self.assertIn(honest, raw)
            path.write_bytes(bytes(raw.replace(honest, struct.pack("<I", 32))))
            with zipfile.ZipFile(path) as archive:
                # zipfile stops at the declared size and then rejects the CRC;
                # what must never happen is more than the budget coming back.
                with self.assertRaises(zipfile.BadZipFile):
                    CONVERTD._read_zip_member(archive, "docProps/core.xml")
            # And that exception must not escape to flag the document.
            self.assertEqual(CONVERTD._doc_meta_date_entries(str(path)), [])

    def test_a_dtd_is_refused_whatever_the_document_is_encoded_in(self):
        # The memory bound is MAX_ZIP_MEMBER_BYTES, not this; refusing a DTD
        # is the separate guarantee that no entity is ever expanded and no
        # external reference ever followed. It has to be enforced by the
        # parser: a byte scan for b'<!DOCTYPE' sees nothing at all here.
        declaration = (
            '<?xml version="1.0" encoding="UTF-16"?>'
            '<!DOCTYPE p [<!ENTITY a "aaaaaaaaaa">]>'
            "<created>2019-03-04</created>"
        )
        utf16 = declaration.encode("utf-16")
        self.assertNotIn(b"<!DOCTYPE", utf16)
        self.assertNotIn(b"<!ENTITY", utf16)
        self.assertIsNone(CONVERTD._parse_core_xml(utf16))
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(
                CONVERTD._doc_meta_date_entries(self._docx_with_core(tmp, utf16)), []
            )

    def test_an_entity_expansion_bomb_is_refused(self):
        bomb = (
            b'<?xml version="1.0"?><!DOCTYPE p [<!ENTITY a "aaaaaaaaaa">'
            b'<!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">'
            b'<!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">]>'
            b"<created>&c;</created>"
        )
        self.assertIsNone(CONVERTD._parse_core_xml(bomb))
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(CONVERTD._doc_meta_date_entries(self._docx_with_core(tmp, bomb)), [])

    def test_ordinary_core_properties_still_parse(self):
        data = (
            b'<?xml version="1.0"?>'
            b"<cp><created>2019-03-04T09:12:00Z</created></cp>"
        )
        self.assertIsNotNone(CONVERTD._parse_core_xml(data))

    def test_a_utf16_document_without_a_dtd_still_yields_its_dates(self):
        core = (
            '<?xml version="1.0" encoding="UTF-16"?>'
            '<cp:coreProperties xmlns:cp="urn:cp" xmlns:dcterms="urn:dcterms">'
            "<dcterms:created>2019-03-04T09:12:00Z</dcterms:created>"
            "</cp:coreProperties>"
        ).encode("utf-16")
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(
                CONVERTD._doc_meta_date_entries(self._docx_with_core(tmp, core)),
                [{"iso": "2019-03-04", "prop": "created"}],
            )

    def test_an_ordinary_member_is_still_read(self):
        with zipfile.ZipFile(FIXTURES / "sample_letter.docx") as archive:
            self.assertIn(b"dcterms:created", CONVERTD._read_zip_member(archive, "docProps/core.xml"))

    def test_a_missing_member_is_not_an_error(self):
        with zipfile.ZipFile(FIXTURES / "sample_letter.docx") as archive:
            self.assertIsNone(CONVERTD._read_zip_member(archive, "docProps/app.xml"))


class RequirementsLockTests(unittest.TestCase):
    """Base markitdown ships no document parsers at all: without the extras
    every .docx/.pptx/.xlsx raises FileConversionException and quarantines."""

    LOCK = ROOT / "sidecar" / "requirements.lock"

    def _pinned(self) -> dict:
        pins = {}
        for raw in self.LOCK.read_text(encoding="utf-8").splitlines():
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            name, _, version = line.partition("==")
            # PEP 503: '.', '-' and '_' are interchangeable in project names.
            pins[name.strip().lower().replace(".", "-").replace("_", "-")] = version.strip()
        return pins

    def test_document_parsers_are_locked(self):
        pins = self._pinned()
        for package in ("pdfminer-six", "pdfplumber", "mammoth", "lxml", "python-pptx", "openpyxl"):
            self.assertIn(package, pins, f"{package} missing from requirements.lock")

    def test_the_legacy_xls_reader_is_locked(self):
        # routing.rs:29 puts application/vnd.ms-excel in NATIVE_TYPES, so every
        # .xls in the backfill reaches markitdown, whose XLS branch is
        # pd.read_excel(engine="xlrd"). Without xlrd that is a
        # MissingDependencyException and the file quarantines.
        self.assertIn("xlrd", self._pinned())

    def test_the_requirements_ask_for_the_parser_extras(self):
        for name in ("requirements.in", "requirements.txt"):
            text = (ROOT / "sidecar" / name).read_text(encoding="utf-8")
            self.assertIn("markitdown[pdf,docx,pptx,xls,xlsx]", text, name)

    def test_build_only_tooling_is_not_in_the_runtime_lock(self):
        pins = self._pinned()
        for package in ("pyinstaller", "pyinstaller-hooks-contrib", "altgraph", "pefile", "pywin32-ctypes"):
            self.assertNotIn(package, pins, f"{package} leaked into requirements.lock from the build venv")

    def test_every_pin_is_exact(self):
        for raw in self.LOCK.read_text(encoding="utf-8").splitlines():
            line = raw.strip()
            if line and not line.startswith("#"):
                self.assertIn("==", line, line)


@unittest.skipUnless(
    _installed("markitdown"),
    "markitdown is not installed in this interpreter; the frozen-binary gate in "
    "scripts/build-sidecar.ps1 drives the same fixtures",
)
class FixtureConversionTests(unittest.TestCase):
    """Real documents through the real ops. `ping` returns {} and every heavy
    component sits behind a lazy factory, so nothing else in this suite would
    notice a missing parser dependency."""

    def test_docx_converts_to_non_empty_markdown(self):
        result = CONVERTD.op_convert({"path": str(FIXTURES / "sample_letter.docx")})
        self.assertFalse(result["encrypted"])
        self.assertIn("Northgate", result["markdown"])

    def test_pdf_converts_to_non_empty_markdown(self):
        result = CONVERTD.op_convert({"path": str(FIXTURES / "sample_text.pdf")})
        self.assertFalse(result["encrypted"])
        self.assertTrue(result["markdown"].strip())

    @unittest.skipUnless(
        _installed("rapidocr", "numpy", "PIL"),
        "the OCR lane's dependencies are not installed in this interpreter",
    )
    def test_scanned_png_ocrs_to_non_empty_markdown(self):
        result = CONVERTD.op_ocr({"path": str(FIXTURES / "sample_scan.png"), "dpi": 300})
        self.assertTrue(result["ocr_used"])
        self.assertEqual(result["page_count"], 1)
        self.assertTrue(result["markdown"].strip())
        self.assertEqual(result["pages_with_text"], 1)


class FixtureIntegrityTests(unittest.TestCase):
    """These run everywhere, so a corrupted fixture is caught here rather than
    showing up as a mysterious skip on the release machine."""

    def test_the_three_fixtures_exist_and_are_the_formats_they_claim(self):
        docx = FIXTURES / "sample_letter.docx"
        pdf = FIXTURES / "sample_text.pdf"
        png = FIXTURES / "sample_scan.png"
        with zipfile.ZipFile(docx) as archive:
            self.assertIsNone(archive.testzip())
            self.assertIn("word/document.xml", archive.namelist())
        self.assertTrue(pdf.read_bytes().startswith(b"%PDF-"))
        self.assertTrue(pdf.read_bytes().rstrip().endswith(b"%%EOF"))
        self.assertTrue(png.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"))

    def test_the_scan_fixture_is_a_decodable_greyscale_image_with_ink_on_it(self):
        data = (FIXTURES / "sample_scan.png").read_bytes()
        position, chunks = 8, {}
        while position < len(data):
            length = struct.unpack(">I", data[position : position + 4])[0]
            tag = data[position + 4 : position + 8]
            chunks[tag] = chunks.get(tag, b"") + data[position + 8 : position + 8 + length]
            position += 12 + length
        width, height, depth, colour = struct.unpack(">IIBB", chunks[b"IHDR"][:10])
        self.assertEqual((depth, colour), (8, 0))
        raw = zlib.decompress(chunks[b"IDAT"])
        self.assertEqual(len(raw), height * (width + 1))
        self.assertIn(0, raw)  # black ink, not a blank page


if __name__ == "__main__":
    unittest.main()
