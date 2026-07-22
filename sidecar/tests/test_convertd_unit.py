import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "sidecar" / "convertd.py"
SPEC = importlib.util.spec_from_file_location("backlog_convertd", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
CONVERTD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONVERTD)


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


if __name__ == "__main__":
    unittest.main()
