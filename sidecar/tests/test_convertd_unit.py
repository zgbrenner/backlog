import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "sidecar" / "convertd.py"
SPEC = importlib.util.spec_from_file_location("backlog_convertd", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
CONVERTD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONVERTD)


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


if __name__ == "__main__":
    unittest.main()
