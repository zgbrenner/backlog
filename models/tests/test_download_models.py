import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "models" / "download_models.py"
SPEC = importlib.util.spec_from_file_location("backlog_download_models", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
DOWNLOAD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DOWNLOAD)


class ModelSpecTests(unittest.TestCase):
    def test_core_models_use_apache_qwen_ggufs(self):
        files = {
            (spec.repo_id, spec.filename, spec.target)
            for spec in DOWNLOAD.MODEL_SPECS
        }
        self.assertIn(
            (
                "Qwen/Qwen3-0.6B-GGUF",
                "Qwen3-0.6B-Q8_0.gguf",
                "Qwen3-0.6B-Q8_0.gguf",
            ),
            files,
        )
        self.assertIn(
            (
                "Qwen/Qwen3-1.7B-GGUF",
                "Qwen3-1.7B-Q8_0.gguf",
                "Qwen3-1.7B-Q8_0.gguf",
            ),
            files,
        )

    def test_restricted_and_training_only_assets_are_absent(self):
        serialized = json.dumps(
            [spec.__dict__ for spec in DOWNLOAD.MODEL_SPECS]
        ).lower()
        self.assertNotIn("liquidai", serialized)
        self.assertNotIn("lfm2", serialized)
        self.assertNotIn("fasttext", serialized)
        self.assertNotIn("lid.176", serialized)
        self.assertNotIn("ettin", serialized)


class LockVerificationTests(unittest.TestCase):
    def test_valid_lock_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            model = root / "Qwen3-0.6B-Q8_0.gguf"
            model.write_bytes(b"model")
            lock = {model.name: DOWNLOAD.sha256_file(model)}
            self.assertEqual(DOWNLOAD.verify_lock(root, lock), [])

    def test_missing_changed_and_untracked_files_are_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            changed = root / "changed.gguf"
            changed.write_bytes(b"new")
            untracked = root / "untracked.safetensors"
            untracked.write_bytes(b"extra")
            lock = {
                "missing.gguf": "0" * 64,
                "changed.gguf": "1" * 64,
            }
            errors = DOWNLOAD.verify_lock(root, lock)
            self.assertTrue(
                any("missing locked file" in error for error in errors)
            )
            self.assertTrue(any("hash mismatch" in error for error in errors))
            self.assertTrue(
                any("untracked model file" in error for error in errors)
            )

    def test_tooling_and_cache_files_are_not_treated_as_model_payloads(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "download_models.py").write_text("print('tool')")
            (root / "models.lock.json").write_text("{}")
            (root / ".cache").mkdir()
            (root / ".cache" / "temp.bin").write_bytes(b"cache")
            (root / "grammar").mkdir()
            (root / "grammar" / "name.gbnf").write_text("root ::= 'x'")
            self.assertEqual(list(DOWNLOAD.iter_payload_files(root)), [])

    def test_unsafe_lock_paths_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            errors = DOWNLOAD.verify_lock(root, {"../escape.gguf": "0" * 64})
            self.assertTrue(any("unsafe lock path" in error for error in errors))


if __name__ == "__main__":
    unittest.main()
