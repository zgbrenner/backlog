import importlib.util
import hashlib
import json
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
MODULE_PATH = ROOT / "models" / "download_models.py"
LOCK_PATH = ROOT / "models" / "models.lock.json"
SPEC = importlib.util.spec_from_file_location("backlog_download_models", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
DOWNLOAD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DOWNLOAD)

SEMANTIC_REVISION = "751bff37182d3f1213fa05d7196b954e230abad9"
SEMANTIC_MODEL_TARGET = "semantic/all-MiniLM-L6-v2/model.onnx"
SEMANTIC_VOCAB_TARGET = "semantic/all-MiniLM-L6-v2/vocab.txt"
SEMANTIC_MODEL_SHA256 = "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1"
SEMANTIC_VOCAB_SHA256 = "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3"


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

    def test_every_tier_a_machine_can_be_assigned_is_staged(self):
        # One staged models/ directory has to provision every deployment
        # target. A machine at or below 9 GiB of RAM collapses its escalation
        # tier onto the primary and never loads the 1.7B, but a staging run
        # that dropped it could not provision anything above that, and the Rust
        # downloader (src-tauri/src/model_download.rs) pins both against this
        # same lock.
        targets = {spec.target for spec in DOWNLOAD.MODEL_SPECS}
        self.assertEqual(
            {target for target in targets if target.endswith(".gguf")},
            {
                "Qwen3-0.6B-Q8_0.gguf",
                "Qwen3-1.7B-Q8_0.gguf",
            },
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

    def test_torch_only_naming_enhancements_are_absent_from_the_slim_bundle(self):
        # gliclass (doc-type classification) and granite (salience
        # embeddings) are torch-only naming enhancements that convertd.py
        # degrades gracefully without; the slim sidecar bundle never fetches
        # them. See docs/DEPENDENCY_COMPATIBILITY.md.
        serialized = json.dumps(
            [spec.__dict__ for spec in DOWNLOAD.MODEL_SPECS]
        ).lower()
        self.assertNotIn("gliclass", serialized)
        self.assertNotIn("granite", serialized)
        self.assertEqual(len(DOWNLOAD.MODEL_SPECS), 4)

    def test_semantic_model_assets_are_revision_pinned_and_staged_under_runtime_layout(self):
        files = {
            (spec.repo_id, spec.filename, spec.target, spec.revision)
            for spec in DOWNLOAD.MODEL_SPECS
        }
        self.assertIn(
            (
                "Xenova/all-MiniLM-L6-v2",
                "onnx/model_quantized.onnx",
                SEMANTIC_MODEL_TARGET,
                SEMANTIC_REVISION,
            ),
            files,
        )
        self.assertIn(
            (
                "Xenova/all-MiniLM-L6-v2",
                "vocab.txt",
                SEMANTIC_VOCAB_TARGET,
                SEMANTIC_REVISION,
            ),
            files,
        )

    def test_committed_lock_contains_every_declared_target_and_no_extra_payloads(self):
        lock = json.loads(LOCK_PATH.read_text(encoding="utf-8"))
        declared = {spec.target for spec in DOWNLOAD.MODEL_SPECS}
        self.assertEqual(set(lock), declared)
        self.assertEqual(lock[SEMANTIC_MODEL_TARGET], SEMANTIC_MODEL_SHA256)
        self.assertEqual(lock[SEMANTIC_VOCAB_TARGET], SEMANTIC_VOCAB_SHA256)

    def test_tauri_resources_include_the_nested_semantic_model_directory(self):
        config = json.loads((ROOT / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))
        resources = config["bundle"]["resources"]
        self.assertEqual(
            resources["resources/models/semantic/all-MiniLM-L6-v2/*"],
            "resources/models/semantic/all-MiniLM-L6-v2/",
        )

    def test_release_scripts_gate_semantic_assets_and_live_frozen_semantic_ops(self):
        stage = (ROOT / "scripts" / "stage-release-inputs.ps1").read_text(encoding="utf-8")
        build = (ROOT / "scripts" / "build-sidecar.ps1").read_text(encoding="utf-8")
        verify = (ROOT / "scripts" / "verify-binaries.ps1").read_text(encoding="utf-8")
        for text in (stage, build, verify):
            self.assertIn(SEMANTIC_MODEL_TARGET.replace("/", "\\"), text)
            self.assertIn(SEMANTIC_MODEL_SHA256, text)
            self.assertIn(SEMANTIC_VOCAB_TARGET.replace("/", "\\"), text)
            self.assertIn(SEMANTIC_VOCAB_SHA256, text)
        self.assertIn('"op" = "rank_paragraphs"', build)
        self.assertIn('"op" = "extract_entities"', build)
        self.assertIn("available: true", build)


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


class ExistingLockDownloadTests(unittest.TestCase):
    def _run_single_file_download(self, root: Path, payload: bytes, expected: bytes):
        spec = DOWNLOAD.ModelSpec("Qwen/test", "model.gguf", "model.gguf")
        lock_path = root / "models.lock.json"
        lock_path.write_text(
            json.dumps({spec.target: hashlib.sha256(expected).hexdigest()}),
            encoding="utf-8",
        )
        source = root / ".cache" / "hub-download.bin"
        source.parent.mkdir()

        def fake_hf_hub_download(**_kwargs):
            source.write_bytes(payload)
            return str(source)

        fake_hub = types.SimpleNamespace(
            hf_hub_download=fake_hf_hub_download,
            snapshot_download=mock.Mock(side_effect=AssertionError("unexpected snapshot")),
        )
        patches = (
            mock.patch.object(DOWNLOAD, "HERE", root),
            mock.patch.object(DOWNLOAD, "LOCK", lock_path),
            mock.patch.object(DOWNLOAD, "MODEL_SPECS", [spec]),
            mock.patch.dict(sys.modules, {"huggingface_hub": fake_hub}),
        )
        with patches[0], patches[1], patches[2], patches[3]:
            DOWNLOAD._download_bundle()
        return spec, lock_path

    def test_mutated_download_cannot_replace_a_missing_locked_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            expected = b"committed model bytes"
            with self.assertRaisesRegex(RuntimeError, "does not match existing lock"):
                self._run_single_file_download(root, b"mutable main bytes", expected)
            self.assertFalse((root / "model.gguf").exists())
            self.assertEqual(
                json.loads((root / "models.lock.json").read_text(encoding="utf-8")),
                {"model.gguf": hashlib.sha256(expected).hexdigest()},
            )

    def test_matching_download_preserves_the_committed_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            expected = b"committed model bytes"
            spec, lock_path = self._run_single_file_download(root, expected, expected)
            self.assertEqual((root / spec.target).read_bytes(), expected)
            self.assertEqual(
                json.loads(lock_path.read_text(encoding="utf-8")),
                {spec.target: hashlib.sha256(expected).hexdigest()},
            )


if __name__ == "__main__":
    unittest.main()
