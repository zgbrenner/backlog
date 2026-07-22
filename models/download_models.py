#!/usr/bin/env python3
"""
One-time model fetch for BackLog. Run this ON A MACHINE WITH INTERNET, then
copy the models/ directory to the deployment machine. The app itself never
downloads anything at runtime.

Records SHA-256 of every file into models.lock.json on first download and
VERIFIES against the lockfile on every subsequent run. Commit models.lock.json
to the repo once generated (it MUST be committed for reproducible verification);
do not hand-edit it.

Windows: Developer Mode / admin is no longer required. HF downloads are forced
to copy files instead of creating symlinks (local_dir_use_symlinks=False), which
avoids the WinError 1314 "a required privilege is not held" failure on large
model snapshots.

Usage:
  python download_models.py            # core models
  python download_models.py --vl       # also fetch the VL-Extract fallback
"""

import argparse
import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
LOCK = HERE / "models.lock.json"

# repo_id, filename (None = whole snapshot into a directory), local target
CORE = [
    # SLM primary + escalation (GGUF, llama.cpp)
    ("LiquidAI/LFM2.5-350M-GGUF", "LFM2.5-350M-Q8_0.gguf", "LFM2.5-350M-Q8_0.gguf"),
    ("LiquidAI/LFM2.5-1.2B-Instruct-GGUF", "LFM2.5-1.2B-Instruct-Q4_K_M.gguf", "LFM2.5-1.2B-Instruct-Q4_K_M.gguf"),
    # Zero-shot doc-type classifier
    ("knowledgator/gliclass-base-v3.0", None, "gliclass-base-v3.0"),
    # Salience embeddings
    ("ibm-granite/granite-embedding-small-english-r2", None, "granite-embedding-small-english-r2"),
    # Ettin base encoder (fine-tuned later by training/train_ettin.py)
    ("jhu-clsp/ettin-encoder-32m", None, "ettin-encoder-32m"),
]

VL = [
    ("LiquidAI/LFM2.5-VL-450M-Extract", None, "LFM2.5-VL-450M-Extract"),
]

FASTTEXT_URL = "https://dl.fbaipublicfiles.com/fasttext/supervised-models/lid.176.ftz"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def record(lock: dict, rel: str, path: Path):
    digest = sha256_file(path)
    prev = lock.get(rel)
    if prev and prev != digest:
        print(f"HASH MISMATCH for {rel}:\n  locked   {prev}\n  computed {digest}", file=sys.stderr)
        print("Refusing to continue. Delete the file and re-run, or investigate.", file=sys.stderr)
        sys.exit(2)
    lock[rel] = digest
    print(f"  {rel}  sha256={digest[:16]}...")


def _locked_rels(lock: dict, target: str):
    """Lockfile keys belonging to `target` (a single file key, or a dir prefix)."""
    if target in lock:
        return [target]
    prefix = target + "/"
    return [rel for rel in lock if rel.startswith(prefix)]


def is_complete(lock: dict, target: str, dest: Path) -> bool:
    """A download is complete only if `dest` exists AND every file the committed
    lock records for it is present. A bare-directory existence check treats an
    interrupted (partial) download as done and skips it forever; verifying
    against the lock re-fetches when any locked file is missing. With no lock
    entries yet (first run) we fall back to plain existence."""
    if not dest.exists():
        return False
    rels = _locked_rels(lock, target)
    if not rels:
        return True
    return all((HERE / rel).exists() for rel in rels)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--vl", action="store_true", help="also fetch LFM2.5-VL-450M-Extract")
    args = ap.parse_args()

    try:
        from huggingface_hub import hf_hub_download, snapshot_download
    except ImportError:
        print("pip install huggingface_hub", file=sys.stderr)
        sys.exit(1)
    import urllib.request

    lock = json.loads(LOCK.read_text(encoding="utf-8")) if LOCK.exists() else {}
    items = CORE + (VL if args.vl else [])

    for repo, filename, target in items:
        dest = HERE / target
        print(f"[{repo}]")
        if filename:
            if not is_complete(lock, target, dest):
                # local_dir_use_symlinks=False -> copy, not symlink, so Windows
                # downloads work without Developer Mode/admin (avoids WinError 1314).
                got = hf_hub_download(repo_id=repo, filename=filename, local_dir=HERE,
                                      local_dir_use_symlinks=False)
                got = Path(got)
                if got != dest:
                    got.replace(dest)
            record(lock, target, dest)
        else:
            if not is_complete(lock, target, dest):
                # local_dir_use_symlinks=False -> copy, not symlink, so Windows
                # downloads work without Developer Mode/admin (avoids WinError 1314).
                # snapshot_download resumes, completing a partial/interrupted dir.
                snapshot_download(repo_id=repo, local_dir=dest,
                                  local_dir_use_symlinks=False)
            # lock every file in the snapshot. Use as_posix() so a lock generated
            # on Windows (backslashes) still verifies on macOS/Linux and vice versa.
            for f in sorted(dest.rglob("*")):
                if f.is_file() and not f.name.startswith("."):
                    record(lock, f.relative_to(HERE).as_posix(), f)

    ft = HERE / "lid.176.ftz"
    print("[fasttext lid.176]")
    if not ft.exists():
        # timeout so a stalled connection fails fast instead of hanging forever
        # (urlretrieve has no timeout).
        with urllib.request.urlopen(FASTTEXT_URL, timeout=60) as resp:
            ft.write_bytes(resp.read())
    record(lock, "lid.176.ftz", ft)

    LOCK.write_text(json.dumps(lock, indent=2, sort_keys=True), encoding="utf-8")
    print(f"\nWrote {LOCK} ({len(lock)} entries). Commit this file.")
    print("Copy the whole models/ directory to the deployment machine.")


if __name__ == "__main__":
    main()
