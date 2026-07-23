#!/usr/bin/env python3
"""Download and verify BackLog's fully local runtime model bundle.

The application never downloads at runtime. Run this script once on a connected
staging machine, commit ``models.lock.json``, and copy the verified ``models/``
directory to the deployment machine.

Usage:
  python download_models.py
  python download_models.py --verify-only

``--verify-only`` imports no Hub client and performs no network access. Ettin is
a separate training input and is deliberately not part of this runtime bundle.

This is the slim, torch-free sidecar profile's bundle: just the two Apache-2.0
Qwen3 GGUFs. gliclass (doc-type classification) and granite (salience
embeddings) are not fetched here -- they're torch-only naming enhancements
that ``sidecar/convertd.py``'s ``_gliclass``/``_granite`` loaders degrade to
deterministic fallbacks for when their libraries or snapshots are absent, and
the slim sidecar never installs those libraries. See
``docs/DEPENDENCY_COMPATIBILITY.md``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOCK = HERE / "models.lock.json"


class ModelSpec:
    def __init__(self, repo_id: str, filename: str | None, target: str):
        self.repo_id = repo_id
        self.filename = filename
        self.target = target


MODEL_SPECS = [
    # Apache-2.0 Qwen GGUFs served by llama.cpp. This is the whole bundle on
    # the slim, torch-free sidecar profile -- gliclass (doc-type
    # classification) and granite (salience embeddings) are deliberately
    # absent; see the module docstring above.
    ModelSpec(
        "Qwen/Qwen3-0.6B-GGUF",
        "Qwen3-0.6B-Q8_0.gguf",
        "Qwen3-0.6B-Q8_0.gguf",
    ),
    ModelSpec(
        "Qwen/Qwen3-1.7B-GGUF",
        "Qwen3-1.7B-Q8_0.gguf",
        "Qwen3-1.7B-Q8_0.gguf",
    ),
]

_TOOL_FILES = {"download_models.py", "models.lock.json"}
_IGNORED_DIRS = {".cache", ".git", "__pycache__", "grammar", "tests"}
_PAYLOAD_SUFFIXES = {
    ".gguf",
    ".safetensors",
    ".bin",
    ".onnx",
    ".model",
    ".pt",
    ".pth",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _safe_relative_path(value: str) -> Path | None:
    path = Path(value)
    if path.is_absolute() or not value or ".." in path.parts:
        return None
    return path


def _is_ignored(relative: Path) -> bool:
    if relative.name in _TOOL_FILES or relative.suffix in {".py", ".pyc"}:
        return True
    return any(part in _IGNORED_DIRS or part.startswith(".") for part in relative.parts)


def iter_payload_files(root: Path):
    """Yield model-like files while excluding repository tooling and caches."""
    if not root.exists():
        return
    directory_targets = {spec.target for spec in MODEL_SPECS if spec.filename is None}
    file_targets = {spec.target for spec in MODEL_SPECS if spec.filename is not None}
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(root)
        if _is_ignored(relative):
            continue
        first = relative.parts[0]
        if (
            first in directory_targets
            or relative.as_posix() in file_targets
            or relative.suffix.lower() in _PAYLOAD_SUFFIXES
        ):
            yield path


def verify_lock(root: Path, lock: dict[str, str]) -> list[str]:
    """Return every lock integrity problem without mutating the model bundle."""
    errors: list[str] = []
    normalized_lock: dict[str, str] = {}

    if not isinstance(lock, dict) or not lock:
        errors.append("model lock is empty or invalid")
        lock = {}

    for relative_text, expected in sorted(lock.items()):
        relative = _safe_relative_path(relative_text)
        if relative is None:
            errors.append(f"unsafe lock path: {relative_text!r}")
            continue
        normalized = relative.as_posix()
        normalized_lock[normalized] = str(expected).lower()
        path = root / relative
        if not path.is_file():
            errors.append(f"missing locked file: {normalized}")
            continue
        actual = sha256_file(path)
        if actual != str(expected).lower():
            errors.append(
                f"hash mismatch for {normalized}: locked {expected}, computed {actual}"
            )

    for path in iter_payload_files(root):
        relative = path.relative_to(root).as_posix()
        if relative not in normalized_lock:
            errors.append(f"untracked model file: {relative}")

    return errors


def _expected_payload_files(root: Path):
    for spec in MODEL_SPECS:
        target = root / spec.target
        if spec.filename is not None:
            if target.is_file():
                yield target
            continue
        if target.is_dir():
            for path in sorted(target.rglob("*")):
                if path.is_file() and not _is_ignored(path.relative_to(root)):
                    yield path


def _write_lock(root: Path) -> dict[str, str]:
    lock = {
        path.relative_to(root).as_posix(): sha256_file(path)
        for path in _expected_payload_files(root)
    }
    if not lock:
        raise RuntimeError("download completed without any model payload files")
    LOCK.write_text(
        json.dumps(lock, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    return lock


def _load_lock() -> dict[str, str]:
    if not LOCK.exists():
        return {}
    value = json.loads(LOCK.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError("models.lock.json must contain a JSON object")
    return {str(key): str(digest).lower() for key, digest in value.items()}


def _download_bundle() -> None:
    try:
        from huggingface_hub import hf_hub_download, snapshot_download
    except ImportError as error:
        raise RuntimeError(
            "install the staging dependency: pip install huggingface_hub"
        ) from error

    existing_lock = _load_lock()
    # A changed locked file is never overwritten. Missing files may be restored
    # from the same declared source, then the complete bundle is re-verified.
    for relative, expected in existing_lock.items():
        safe = _safe_relative_path(relative)
        if safe is None:
            raise RuntimeError(f"unsafe path in existing lock: {relative!r}")
        path = HERE / safe
        if path.is_file() and sha256_file(path) != expected:
            raise RuntimeError(
                f"refusing to overwrite changed locked file {relative}; "
                "delete it only after investigation"
            )

    for spec in MODEL_SPECS:
        destination = HERE / spec.target
        print(f"[{spec.repo_id}]")
        if spec.filename is not None:
            if not destination.exists():
                downloaded = Path(
                    hf_hub_download(
                        repo_id=spec.repo_id,
                        filename=spec.filename,
                        local_dir=HERE,
                    )
                )
                if downloaded.resolve() != destination.resolve():
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(downloaded, destination)
        else:
            snapshot_download(repo_id=spec.repo_id, local_dir=destination)

    allowed = {path.resolve() for path in _expected_payload_files(HERE)}
    unexpected = [
        path.relative_to(HERE).as_posix()
        for path in iter_payload_files(HERE)
        if path.resolve() not in allowed
    ]
    if unexpected:
        joined = "\n  - ".join(unexpected)
        raise RuntimeError(
            "obsolete or untracked model files remain; remove them before locking:\n  - "
            + joined
        )

    lock = _write_lock(HERE)
    errors = verify_lock(HERE, lock)
    if errors:
        raise RuntimeError(
            "post-download verification failed:\n  - " + "\n  - ".join(errors)
        )
    print(f"\nWrote {LOCK} ({len(lock)} entries). Commit this lockfile.")
    print("Copy the complete models/ directory to the deployment machine.")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--verify-only",
        action="store_true",
        help="verify models.lock.json without importing Hub clients or using the network",
    )
    args = parser.parse_args()

    try:
        if args.verify_only:
            errors = verify_lock(HERE, _load_lock())
            if errors:
                print("Model verification failed:", file=sys.stderr)
                for error in errors:
                    print(f"  - {error}", file=sys.stderr)
                return 2
            print("Model bundle verified successfully.")
            return 0
        _download_bundle()
        return 0
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print(f"ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
