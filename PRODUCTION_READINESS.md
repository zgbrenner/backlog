# PRODUCTION_READINESS.md is retired

This document was a rolling strikethrough log of one hardening pass. It
accumulated an inline "update" note saying items 1-3 were done while all three
remained verbatim under "What still needs attention"; it stated "No auto-updater"
about an app that ships a signed one; and its verification table quoted a test
count that no run has produced in months. A document that contradicts itself
teaches readers to distrust the parts of it that are accurate.

It has been split into three files that each have one job, and each of which is
wrong in a way you can check:

| If you want | Read |
|---|---|
| What changed, per version | [`CHANGELOG.md`](CHANGELOG.md) |
| What is genuinely still open | [`docs/KNOWN_ISSUES.md`](docs/KNOWN_ISSUES.md) |
| Why the load-bearing choices are what they are | [`docs/DECISIONS.md`](docs/DECISIONS.md) |
| Whether this is safe to deploy | [`docs/RELEASE_CHECKLIST.md`](docs/RELEASE_CHECKLIST.md) and [`docs/PILOT_RUNBOOK.md`](docs/PILOT_RUNBOOK.md) |
| What it does with documents | [`docs/PRIVACY.md`](docs/PRIVACY.md), [`docs/SECURITY.md`](docs/SECURITY.md) |

## Current verification, freshly run

Not a claim carried forward. These are the commands, and the numbers they
produce on this tree. `./scripts/ci-local.sh` runs all of them in one pass and
is what any of these numbers should be re-derived from.

`.github/workflows/ci.yml` describes the same five jobs but **has never
executed** — every run dies in seconds with no runner assigned
(`docs/KNOWN_ISSUES.md` item 11). Do not read a green badge into this table;
read it as "someone ran the gates on a developer machine and these were the
results".

| Check | Command | Result |
|---|---|---|
| Trust core | `cargo test -p backlog-core` | **49 passed** |
| Full workspace | `cargo test --workspace --all-targets` | **199 passed** (150 app + 49 core) |
| Lints | `cargo clippy --workspace --all-targets -- -D warnings` | 0 warnings |
| Formatting | `cargo fmt --all -- --check` | clean |
| Frontend | `npm run check` (`tsc --noEmit` + `vite build`) | passes |
| UI harness | `npm run harness:shots` | every scenario, both themes, 0 console errors |
| Python | `python -m pytest sidecar/tests models/tests` | 96 passed, 3 skipped |
| Manifest contract | `python power-automate/validate_examples.py` | passes |

What cannot be verified anywhere but on Windows, and is therefore gated by
`docs/RELEASE_CHECKLIST.md` rather than by CI: the DPAPI key path, the NSIS
bundle, install/repair/upgrade/uninstall, and the end-to-end run with real
sidecar binaries and model weights.
