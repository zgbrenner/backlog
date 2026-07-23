import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace


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


if __name__ == "__main__":
    unittest.main()
