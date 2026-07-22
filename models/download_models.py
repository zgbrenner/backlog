#!/usr/bin/env python3
"""
One-time model fetch for BackLog. Run this ON A MACHINE WITH INTERNET, then
copy the models/ directory to the deployment machine. The app itself never
downloads anything at runtime.

Records SHA-256 of every file into models.lock.json on first download and
VERIFIES against the lockfile on every subsequent run. Commit models.lock.json
to the repo once generated; do not hand-edit it.

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

    lock = json.loads(LOCK.read_text()) if LOCK.exists() else {}
    items = CORE + (VL if args.vl else [])

    for repo, filename, target in items:
        dest = HERE / target
        print(f"[{repo}]")
        if filename:
            if not dest.exists():
                got = hf_hub_download(repo_id=repo, filename=filename, local_dir=HERE)
                got = Path(got)
                if got != dest:
                    got.replace(dest)
            record(lock, target, dest)
        else:
            if not dest.exists():
                snapshot_download(repo_id=repo, local_dir=dest)
            # lock every file in the snapshot
            for f in sorted(dest.rglob("*")):
                if f.is_file() and not f.name.startswith("."):
                    record(lock, str(f.relative_to(HERE)), f)

    ft = HERE / "lid.176.ftz"
    print("[fasttext lid.176]")
    if not ft.exists():
        urllib.request.urlretrieve(FASTTEXT_URL, ft)
    record(lock, "lid.176.ftz", ft)

    LOCK.write_text(json.dumps(lock, indent=2, sort_keys=True))
    print(f"\nWrote {LOCK} ({len(lock)} entries). Commit this file.")
    print("Copy the whole models/ directory to the deployment machine.")


if __name__ == "__main__":
    main()
