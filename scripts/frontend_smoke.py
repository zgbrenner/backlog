#!/usr/bin/env python3
"""Static smoke test for the Tauri command and critical DOM contracts.

This intentionally avoids a browser dependency. It catches drift between the
frontend command names and Rust's invoke handler, plus the accessibility,
instance-review, and state-management regressions that previously made the UI
misleading.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAIN = (ROOT / "src" / "main.ts").read_text(encoding="utf-8")
LIB = (ROOT / "src-tauri" / "src" / "lib.rs").read_text(encoding="utf-8")
PREFLIGHT = (ROOT / "src-tauri" / "src" / "preflight.rs").read_text(encoding="utf-8")
REVIEW = (ROOT / "src-tauri" / "src" / "review.rs").read_text(encoding="utf-8")
STYLES = (ROOT / "src" / "styles.css").read_text(encoding="utf-8")


def fail(message: str) -> None:
    print(f"frontend smoke failure: {message}", file=sys.stderr)
    raise SystemExit(1)


def require(source: str, needle: str, description: str) -> None:
    if needle not in source:
        fail(f"missing {description}: {needle}")


def extract_invokes(source: str) -> set[str]:
    return set(
        re.findall(
            r'invoke(?:<[^>]+>)?\(\s*["\']([a-zA-Z0-9_]+)["\']',
            source,
        )
    )


def extract_handlers(source: str) -> set[str]:
    match = re.search(
        r"tauri::generate_handler!\s*\[([^\]]+)\]",
        source,
        flags=re.DOTALL,
    )
    if not match:
        fail("Rust invoke handler list could not be found")
    return {
        name.strip()
        for name in match.group(1).split(",")
        if name.strip()
    }


def main() -> int:
    invokes = extract_invokes(MAIN)
    handlers = extract_handlers(LIB)
    missing_handlers = invokes - handlers
    if missing_handlers:
        fail(
            "frontend invokes commands absent from Rust: "
            + ", ".join(sorted(missing_handlers))
        )

    required_commands = {
        "get_config",
        "set_config",
        "get_runtime_status",
        "run_preflight",
        "start_pipeline",
        "set_paused",
        "list_jobs",
        "list_flagged",
        "get_stats",
        "get_evidence",
        "resubmit",
    }
    missing_commands = required_commands - invokes
    if missing_commands:
        fail(
            "critical frontend commands are not exercised: "
            + ", ".join(sorted(missing_commands))
        )

    if re.search(r"\blet\s+(running|paused)\s*=", MAIN):
        fail("frontend reintroduced local-only running or paused state")
    require(
        MAIN,
        "const runDisabled = !runtime.running && !runtime.configured;",
        "hard Start gate",
    )
    require(
        MAIN,
        'activeCheck ? "run_preflight" : "get_runtime_status"',
        "backend runtime refresh",
    )
    require(
        LIB,
        "if !status.configured",
        "backend preflight enforcement before Start",
    )

    for field in (
        "processing_dir_ready",
        "outbox_writable",
        "quarantine_writable",
        "cache_writable",
        "sidecar_found",
        "sidecar_ok",
        "llama_server_found",
        "grammar_found",
        "primary_model_found",
        "escalation_model_found",
        "offline_runtime",
    ):
        require(MAIN, field, f"frontend readiness field {field}")
        require(PREFLIGHT, field, f"backend readiness field {field}")

    require(MAIN, "type ReviewItem = {", "physical review item type")
    require(MAIN, "instanceId: item.instance_id", "instance-aware resubmit argument")
    require(MAIN, "item.instance_id.slice(0, 12)", "visible review InstanceId")
    require(LIB, "review::list_review_items", "instance-aware review query")
    require(LIB, "resubmit_instance(&instance_id", "instance-aware correction command")
    require(REVIEW, "WHERE fi.state = 'flagged'", "flagged instance SQL filter")
    require(REVIEW, "manifest_id: instance.instance_id.clone()", "stable corrected ManifestId")

    for control in ("date", "subject", "description"):
        require(MAIN, f'for="${{id}}-{control}"', f"review label for {control}")
        require(MAIN, f'id="${{id}}-{control}"', f"review input id for {control}")
    require(MAIN, 'role="alert" aria-live="polite"', "accessible inline error region")
    require(MAIN, "submitButton.disabled = true;", "pending review lock")
    require(MAIN, "submitButton.disabled = false;", "review retry recovery")
    require(MAIN, 'card.removeAttribute("aria-busy")', "review busy-state cleanup")
    if "window.alert(" in MAIN or "alert(" in MAIN:
        fail("blocking alert dialog reintroduced")

    for selector in (
        "button:focus-visible",
        ".runtime-chip",
        ".check-list",
        ".problem-box",
        ".settings-layout",
    ):
        require(STYLES, selector, f"critical UI style {selector}")

    print(
        f"Frontend smoke passed: {len(invokes)} commands are wired, runtime state "
        "is backend-owned, and flagged physical instances remain distinct."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
