import importlib.util
import math
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "sidecar" / "semantic.py"
SPEC = importlib.util.spec_from_file_location("backlog_semantic", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
SEMANTIC = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SEMANTIC
SPEC.loader.exec_module(SEMANTIC)


class KeywordEmbedder:
    """Small deterministic stand-in for an embedding model.

    Dimensions intentionally represent semantic concepts rather than exact
    tokens so tests exercise ranking/extraction logic without NumPy, ONNX, or
    downloading model weights.
    """

    model_id = "test/keyword-embedder"

    def __init__(self):
        self.calls = []

    @staticmethod
    def _vector(text: str) -> list[float]:
        lowered = text.lower()
        concepts = [
            ("termination", "terminate", "ended", "last day"),
            ("date", "effective", "july", "2026", "march"),
            ("person", "employee", "jane", "john", "signer"),
            (
                "organization",
                "company",
                "llc",
                "inc",
                "corporation",
                "party",
                "parties",
                "employer",
            ),
            ("invoice", "amount", "$", "total", "number"),
            ("subject", "regarding", "matter", "title", "agreement"),
            ("boilerplate", "confidentiality", "miscellaneous", "governing law"),
        ]
        vector = [
            float(sum(lowered.count(term) for term in terms)) for terms in concepts
        ]
        norm = math.sqrt(sum(value * value for value in vector)) or 1.0
        return [value / norm for value in vector]

    def encode(self, texts):
        texts = list(texts)
        self.calls.append(texts)
        return [self._vector(text) for text in texts]


class ParagraphRankingTests(unittest.TestCase):
    def test_ranks_relevant_exact_paragraphs_and_preserves_provenance(self):
        embedder = KeywordEmbedder()
        paragraphs = [
            {
                "index": 0,
                "text": "This agreement contains miscellaneous confidentiality boilerplate.",
                "start_char": 0,
                "end_char": 69,
            },
            {
                "index": 1,
                "text": "Jane Doe's employment terminates effective July 31, 2026.",
                "start_char": 71,
                "end_char": 130,
            },
            {
                "index": 2,
                "text": "The governing law provision applies to the company.",
                "start_char": 132,
                "end_char": 183,
            },
        ]

        result = SEMANTIC.rank_paragraphs(
            embedder,
            paragraphs,
            ["effective date of termination", "employee being terminated"],
            top_k=2,
            min_score=0.05,
        )

        self.assertTrue(result["available"])
        self.assertEqual(result["model"], embedder.model_id)
        self.assertEqual(result["results"][0]["index"], 1)
        self.assertEqual(result["results"][0]["text"], paragraphs[1]["text"])
        self.assertEqual(result["results"][0]["start_char"], 71)
        self.assertIn("termination", result["results"][0]["probe"].lower())
        self.assertEqual(result["source_chars"], sum(len(p["text"]) for p in paragraphs))
        self.assertEqual(
            result["selected_chars"],
            sum(len(item["text"]) for item in result["results"]),
        )

    def test_mmr_does_not_fill_the_lane_with_near_duplicates(self):
        embedder = KeywordEmbedder()
        paragraphs = [
            {"index": 0, "text": "Employment terminates July 31, 2026.", "start_char": 0, "end_char": 38},
            {"index": 1, "text": "The employee termination date is July 31, 2026.", "start_char": 40, "end_char": 88},
            {"index": 2, "text": "Acme LLC is the employer and party to the agreement.", "start_char": 90, "end_char": 143},
        ]

        result = SEMANTIC.rank_paragraphs(
            embedder,
            paragraphs,
            ["termination date", "parties to the document"],
            top_k=2,
            min_score=0.01,
            diversity=0.45,
        )

        indices = {item["index"] for item in result["results"]}
        self.assertIn(2, indices, "a diverse party paragraph should survive duplicate termination text")
        self.assertEqual(len(indices), 2)

    def test_empty_input_short_circuits_without_using_the_embedder(self):
        embedder = KeywordEmbedder()
        result = SEMANTIC.rank_paragraphs(embedder, [], ["date"], top_k=4)
        self.assertEqual(result["results"], [])
        self.assertEqual(embedder.calls, [])


class CachedLabelEntityTests(unittest.TestCase):
    def setUp(self):
        SEMANTIC.clear_label_cache()

    def test_extracts_exact_spans_from_the_complete_document(self):
        embedder = KeywordEmbedder()
        paragraphs = [
            {
                "index": index,
                "text": f"Routine paragraph {index} with no relevant facts.",
                "start_char": index * 60,
                "end_char": index * 60 + 47,
            }
            for index in range(20)
        ]
        paragraphs.append(
            {
                "index": 20,
                "text": "Notice: Acme Holdings LLC terminates Jane Doe effective July 31, 2026.",
                "start_char": 1200,
                "end_char": 1273,
            }
        )

        result = SEMANTIC.extract_entities(
            embedder,
            paragraphs,
            SEMANTIC.DEFAULT_ENTITY_LABELS,
            threshold=0.05,
            max_per_label=6,
        )

        self.assertTrue(result["available"])
        spans = result["spans"]
        self.assertTrue(any(span["paragraph_index"] == 20 for span in spans))
        date = next(span for span in spans if span.get("iso") == "2026-07-31")
        self.assertEqual(date["text"], "July 31, 2026")
        self.assertEqual(
            paragraphs[20]["text"][date["start_char"] : date["end_char"]],
            date["text"],
        )
        self.assertTrue(any(span["text"] == "Acme Holdings LLC" for span in spans))
        self.assertTrue(any(span["text"] == "Jane Doe" for span in spans))

    def test_label_embeddings_are_cached_by_normalized_label_set(self):
        embedder = KeywordEmbedder()
        paragraphs = [
            {
                "index": 0,
                "text": "Acme LLC sent the notice to Jane Doe on July 31, 2026.",
                "start_char": 0,
                "end_char": 57,
            }
        ]
        labels = [
            {"label": "PERSON", "description": "a human person's full name"},
            {"label": "ORGANIZATION", "description": "a company or organization"},
            {"label": "DOCUMENT_DATE", "description": "the date of the document"},
        ]

        first = SEMANTIC.extract_entities(embedder, paragraphs, labels, threshold=0.01)
        calls_after_first = len(embedder.calls)
        second = SEMANTIC.extract_entities(embedder, paragraphs, list(reversed(labels)), threshold=0.01)

        self.assertFalse(first["label_embeddings_reused"])
        self.assertTrue(second["label_embeddings_reused"])
        self.assertEqual(
            len(embedder.calls),
            calls_after_first + 1,
            "the second call should encode candidates but not labels again",
        )
        self.assertEqual(first["label_cache_key"], second["label_cache_key"])

    def test_invalid_or_overlapping_candidates_are_deduplicated(self):
        embedder = KeywordEmbedder()
        paragraphs = [
            {
                "index": 0,
                "text": "Invoice Number: INV-2026-0042. Invoice Number: INV-2026-0042.",
                "start_char": 0,
                "end_char": 63,
            }
        ]
        result = SEMANTIC.extract_entities(
            embedder,
            paragraphs,
            SEMANTIC.DEFAULT_ENTITY_LABELS,
            threshold=0.01,
            max_per_label=8,
        )
        invoice_ids = [
            span for span in result["spans"] if span["label"] == "INVOICE_NUMBER"
        ]
        self.assertLessEqual(len(invoice_ids), 2)
        self.assertTrue(all(span["text"] == "INV-2026-0042" for span in invoice_ids))


class WordPieceTokenizerTests(unittest.TestCase):
    def test_uncased_wordpiece_ids_include_special_tokens_and_padding(self):
        vocab = {
            "[PAD]": 0,
            "[UNK]": 100,
            "[CLS]": 101,
            "[SEP]": 102,
            "jane": 200,
            "doe": 201,
            "terminate": 202,
            "##s": 203,
            ".": 204,
        }
        tokenizer = SEMANTIC.WordPieceTokenizer(vocab)
        ids, mask, token_types = tokenizer.encode("Jane Doe terminates.", max_length=10)
        self.assertEqual(ids[:7], [101, 200, 201, 202, 203, 204, 102])
        self.assertEqual(mask[:7], [1] * 7)
        self.assertEqual(ids[7:], [0, 0, 0])
        self.assertEqual(token_types, [0] * 10)


if __name__ == "__main__":
    unittest.main()
