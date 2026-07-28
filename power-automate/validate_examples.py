#!/usr/bin/env python3
"""Validate BackLog manifest examples and contract invariants."""

from __future__ import annotations

import copy
import json
import re
import sys
from pathlib import Path
from typing import Any

from jsonschema import Draft6Validator, Draft202012Validator, FormatChecker

ROOT = Path(__file__).resolve().parent
STRICT_SCHEMA_PATH = ROOT / "manifest.schema.json"
PARSE_JSON_SCHEMA_PATH = ROOT / "manifest.parse-json.schema.json"
EXAMPLES_DIR = ROOT / "examples"
# The emitter is the authority on the schema number; these files describe the
# same contract from the SharePoint side. They drifted apart once already --
# Rust moved to v3 for `dismissed` while the schemas, the examples and
# FLOW2-commit.md all still said 2, which would have made every real manifest
# fail the Flow 2 contract condition in a tenant nobody could debug from here.
MANIFEST_RS = ROOT.parent / "src-tauri" / "src" / "manifest.rs"


def rust_schema_version() -> int:
    match = re.search(
        r"MANIFEST_SCHEMA_VERSION:\s*u32\s*=\s*(\d+)",
        MANIFEST_RS.read_text(encoding="utf-8"),
    )
    if not match:
        raise RuntimeError(f"could not read MANIFEST_SCHEMA_VERSION from {MANIFEST_RS}")
    return int(match.group(1))


# Keep the Parse JSON schema deliberately conservative. The strict source
# contract can use modern JSON Schema features, while this schema is limited to
# constructs that the Power Automate action has historically handled reliably.
PARSE_JSON_FORBIDDEN_KEYS = {
    "$schema",
    "$id",
    "$ref",
    "allOf",
    "anyOf",
    "const",
    "contains",
    "dependentSchemas",
    "else",
    "format",
    "if",
    "not",
    "oneOf",
    "pattern",
    "patternProperties",
    "propertyNames",
    "then",
    "unevaluatedProperties",
}


def load_json(path: Path) -> object:
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


def describe(error: object) -> str:
    path = ".".join(str(part) for part in getattr(error, "absolute_path", ()))
    message = getattr(error, "message", str(error))
    return f"{path or '<root>'}: {message}"


def find_parse_schema_compatibility_issues(
    node: Any,
    path: tuple[str, ...] = (),
) -> list[str]:
    issues: list[str] = []
    if isinstance(node, bool):
        issues.append(f"{'.'.join(path) or '<root>'}: boolean schemas are not allowed")
        return issues
    if isinstance(node, dict):
        for key, value in node.items():
            child_path = (*path, key)
            if key in PARSE_JSON_FORBIDDEN_KEYS:
                issues.append(
                    f"{'.'.join(child_path)}: keyword is not allowed in the Parse JSON schema"
                )
            issues.extend(find_parse_schema_compatibility_issues(value, child_path))
    elif isinstance(node, list):
        for index, value in enumerate(node):
            issues.extend(
                find_parse_schema_compatibility_issues(value, (*path, str(index)))
            )
    return issues


def main() -> int:
    strict_schema = load_json(STRICT_SCHEMA_PATH)
    parse_json_schema = load_json(PARSE_JSON_SCHEMA_PATH)
    Draft202012Validator.check_schema(strict_schema)
    Draft6Validator.check_schema(parse_json_schema)

    schema_version = rust_schema_version()
    declared = strict_schema.get("properties", {}).get("schema", {}).get("const")

    strict_validator = Draft202012Validator(
        strict_schema,
        format_checker=FormatChecker(),
    )
    parse_json_validator = Draft6Validator(parse_json_schema)

    failures = find_parse_schema_compatibility_issues(parse_json_schema)
    if declared != schema_version:
        failures.append(
            f"manifest.schema.json pins schema {declared!r}, but "
            f"src-tauri/src/manifest.rs emits {schema_version}"
        )
    examples: dict[str, dict[str, object]] = {}
    for path in sorted(EXAMPLES_DIR.glob("manifest-*.json")):
        payload = load_json(path)
        if not isinstance(payload, dict):
            failures.append(f"{path.name}: root must be an object")
            continue
        examples[path.name] = payload
        for error in sorted(
            strict_validator.iter_errors(payload),
            key=lambda item: list(item.path),
        ):
            failures.append(f"{path.name} strict schema: {describe(error)}")
        for error in sorted(
            parse_json_validator.iter_errors(payload),
            key=lambda item: list(item.path),
        ):
            failures.append(f"{path.name} Parse JSON schema: {describe(error)}")

    required_examples = {
        "manifest-ok.json",
        "manifest-duplicate.json",
        "manifest-flagged.json",
        "manifest-dismissed.json",
    }
    missing = required_examples.difference(examples)
    failures.extend(f"missing required example: {name}" for name in sorted(missing))

    if not missing:
        ok = examples["manifest-ok.json"]
        duplicate = examples["manifest-duplicate.json"]
        flagged = examples["manifest-flagged.json"]
        dismissed = examples["manifest-dismissed.json"]

        # Every example must declare the schema version the Rust emitter
        # writes. Drift here is invisible until a real manifest hits a Flow 2
        # contract condition that still tests for the old number.
        for name, payload in examples.items():
            if payload.get("schema") != schema_version:
                failures.append(
                    f"{name}: schema is {payload.get('schema')!r}, expected {schema_version}"
                )

        if ok.get("sha256") != duplicate.get("sha256"):
            failures.append("duplicate example must reuse the original true sha256")
        if ok.get("manifest_id") == duplicate.get("manifest_id"):
            failures.append("duplicate example must have a distinct manifest_id")
        if duplicate.get("duplicate_of") != duplicate.get("sha256"):
            failures.append("duplicate_of must equal the duplicate content sha256")
        if flagged.get("status") != "flagged":
            failures.append("flagged example must use status=flagged")
        if dismissed.get("status") != "dismissed":
            failures.append("dismissed example must use status=dismissed")

        negative_cases: list[tuple[str, dict[str, object]]] = []
        for field in ("new_filename", "description", "date", "date_source"):
            invalid = copy.deepcopy(ok)
            invalid.pop(field, None)
            negative_cases.append((f"ok missing {field}", invalid))
        invalid = copy.deepcopy(ok)
        invalid["flag_reason"] = "not allowed"
        negative_cases.append(("ok with flag_reason", invalid))
        # manifest.rs refuses an `ok` manifest with no model provenance: the
        # name it ships would not be reproducible and nothing would say so.
        invalid = copy.deepcopy(ok)
        invalid["model_versions"] = {}
        negative_cases.append(("ok with empty model_versions", invalid))
        for label, base in (("flagged", flagged), ("dismissed", dismissed)):
            invalid = copy.deepcopy(base)
            invalid.pop("flag_reason", None)
            negative_cases.append((f"{label} missing flag_reason", invalid))
            invalid = copy.deepcopy(base)
            invalid["new_filename"] = "not-allowed.pdf"
            negative_cases.append((f"{label} with new_filename", invalid))
        invalid = copy.deepcopy(ok)
        invalid["manifest_id"] = "../unsafe"
        negative_cases.append(("unsafe manifest_id", invalid))
        invalid = copy.deepcopy(ok)
        invalid["date"] = "2026-99-99"
        negative_cases.append(("invalid ISO date", invalid))
        invalid = copy.deepcopy(ok)
        invalid["status"] = "reviewed"
        negative_cases.append(("unknown status", invalid))
        invalid = copy.deepcopy(ok)
        invalid["schema"] = schema_version - 1
        negative_cases.append(("previous schema version", invalid))

        for label, payload in negative_cases:
            if not list(strict_validator.iter_errors(payload)):
                failures.append(
                    f"strict schema incorrectly accepted negative case: {label}"
                )

    if failures:
        print("Manifest contract validation failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    print(
        f"Validated {len(examples)} manifest examples against the strict and "
        "Power Automate Parse JSON schemas."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
