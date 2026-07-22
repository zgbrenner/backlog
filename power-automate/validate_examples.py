#!/usr/bin/env python3
"""Validate BackLog manifest examples and contract invariants."""

from __future__ import annotations

import copy
import json
import sys
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker

ROOT = Path(__file__).resolve().parent
SCHEMA_PATH = ROOT / "manifest.schema.json"
EXAMPLES_DIR = ROOT / "examples"


def load_json(path: Path) -> object:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def describe(error: object) -> str:
    path = ".".join(str(part) for part in getattr(error, "absolute_path", ()))
    message = getattr(error, "message", str(error))
    return f"{path or '<root>'}: {message}"


def main() -> int:
    schema = load_json(SCHEMA_PATH)
    Draft202012Validator.check_schema(schema)
    validator = Draft202012Validator(schema, format_checker=FormatChecker())

    failures: list[str] = []
    examples: dict[str, dict[str, object]] = {}
    for path in sorted(EXAMPLES_DIR.glob("manifest-*.json")):
        payload = load_json(path)
        if not isinstance(payload, dict):
            failures.append(f"{path.name}: root must be an object")
            continue
        examples[path.name] = payload
        for error in sorted(validator.iter_errors(payload), key=lambda item: list(item.path)):
            failures.append(f"{path.name}: {describe(error)}")

    required_examples = {
        "manifest-ok.json",
        "manifest-duplicate.json",
        "manifest-flagged.json",
    }
    missing = required_examples.difference(examples)
    failures.extend(f"missing required example: {name}" for name in sorted(missing))

    if not missing:
        ok = examples["manifest-ok.json"]
        duplicate = examples["manifest-duplicate.json"]
        flagged = examples["manifest-flagged.json"]

        if ok.get("sha256") != duplicate.get("sha256"):
            failures.append("duplicate example must reuse the original true sha256")
        if ok.get("manifest_id") == duplicate.get("manifest_id"):
            failures.append("duplicate example must have a distinct manifest_id")
        if duplicate.get("duplicate_of") != duplicate.get("sha256"):
            failures.append("duplicate_of must equal the duplicate content sha256")
        if flagged.get("status") != "flagged":
            failures.append("flagged example must use status=flagged")

        negative_cases: list[tuple[str, dict[str, object]]] = []
        for field in ("new_filename", "description", "date", "date_source"):
            invalid = copy.deepcopy(ok)
            invalid.pop(field, None)
            negative_cases.append((f"ok missing {field}", invalid))
        invalid = copy.deepcopy(ok)
        invalid["flag_reason"] = "not allowed"
        negative_cases.append(("ok with flag_reason", invalid))
        invalid = copy.deepcopy(flagged)
        invalid.pop("flag_reason", None)
        negative_cases.append(("flagged missing flag_reason", invalid))
        invalid = copy.deepcopy(flagged)
        invalid["new_filename"] = "not-allowed.pdf"
        negative_cases.append(("flagged with new_filename", invalid))
        invalid = copy.deepcopy(ok)
        invalid["manifest_id"] = "../unsafe"
        negative_cases.append(("unsafe manifest_id", invalid))

        for label, payload in negative_cases:
            if not list(validator.iter_errors(payload)):
                failures.append(f"schema incorrectly accepted negative case: {label}")

    if failures:
        print("Manifest contract validation failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(f"Validated {len(examples)} manifest examples and contract invariants.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
