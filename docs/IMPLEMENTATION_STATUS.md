# BackLog implementation status

**Updated:** 2026-08-04

> **Status (v0.8 candidate).** The source implements the default **Power
> Automate / SharePoint** handoff and native **Local folder** delivery. Local
> delivery writes final renamed documents and durable receipts under the
> operator-selected Local Output root; it does not create a Power Automate
> manifest or a SharePoint handoff. The five hosted CI jobs remain enabled in
> `.github/workflows/ci.yml` and are mirrored by `./scripts/ci-local.sh`.
>
> The local evidence below supports this candidate, but it is not a statement
> that the exact v0.8 commit has passed hosted CI, Windows packaging, target
> tenant Power Automate testing, signing, or publication. A prior
> complete-workspace hosted CI run passed for this repository lineage; that is
> historical evidence only, not v0.8 certification.

## Implemented in source

- additive output modes: the legacy/default **Power Automate / SharePoint**
  handoff remains available, while **Local folder** commits renamed documents
  directly to Local Output and keeps a per-delivery receipt under
  `.backlog/receipts`;
- mode-aware configuration, preflight/readiness, first-run guidance, folder
  picking, review wording, and Start gating. The required safe roots are
  Processing + Quarantine + Outbox (Power Automate) or Local Output (Local);
  all must be distinct and non-nested;
- separate content, physical-delivery, and manifest identities;
- transactional, case-insensitive filename reservations;
- manifest schema v3 (adds the `dismissed` status and a non-empty
  `model_versions` requirement on `ok`) with strict and Power
  Automate-compatible schemas;
- checkpointed Flow 2 instructions and replay-safe Flow 1 instructions;
- Unicode-safe harvesting and deterministic date-evidence validation;
- pause-safe watcher behavior and bounded sidecar communication;
- durable terminal metadata before source removal: manifest-before-quarantine
  ordering for Power Automate and receipt-backed transactions for Local Output;
- runtime configuration validation and live preflight;
- backend-owned running and paused state in the desktop UI;
- accessible review controls and inline deterministic validation errors;
- Qwen3-0.6B primary and Qwen3-1.7B escalation through llama.cpp chat
  completions with JSON Schema output and thinking disabled;
- RapidOCR 3 compatibility, enhanced 600-DPI final retry, and offline Lingua
  language identification;
- removal of Liquid model weights, fastText `lid.176`, shell permission, opener
  permission, and the corresponding unused Rust plugins from the reviewed
  distributable runtime;
- model download and verification tooling that detects missing, changed,
  untracked, and unsafe paths;
- locked npm inputs, a pinned toolchain (`rust-toolchain.toml`), build
  diagnostics, and a Windows NSIS packaging procedure (`RELEASING.md`) whose
  binaries are gated by `scripts/verify-binaries.ps1`;
- in-app, hash-verified downloader (and the equivalent `models/download_models.py`
  staging script) for the two Qwen3 GGUFs and `models.lock.json`, landing under
  the installed app's data directory (the slim, torch-free sidecar profile
  fetches no GLiClass/Granite snapshots -- see `docs/DEPENDENCY_COMPATIBILITY.md`);
- documentation for each audience it has: `docs/USER_GUIDE.md` and
  `docs/TROUBLESHOOTING.md` (the operator), `docs/PRIVACY.md` (the person whose
  documents these are), `docs/SECURITY.md` and `NOTICE.md` (IT and legal),
  `RELEASING.md` + `docs/RELEASE_CHECKLIST.md` (the releaser),
  `docs/DECISIONS.md` and `docs/KNOWN_ISSUES.md` (the next engineer).

## Automated validation

`./scripts/ci-local.sh` mirrors the five hosted CI jobs. Its workspace job and
the hosted workflow stage deterministic development resource stubs so the app
crate can compile and test without shipping model weights; the guarded Windows
release workflow replaces those stubs with real, hash-verified release inputs.

### Current local evidence (v0.8 candidate, 2026-08-04)

These are recorded local gate results. They are not a substitute for a fresh
single-run hosted CI result on the exact candidate commit.

| Check | Result |
|---|---|
| Rust app suite | 247 passed, 3 ignored; updater coverage: 1 passed |
| `cargo test -p backlog-core --locked` | 65 passed |
| `npm run check` (`tsc --noEmit` + `vite build`) | passed |
| `npm audit --audit-level=high` | passed |
| `npm run harness:shots` | 30 scenarios, light and dark themes, passed with no harness console errors |
| `python -m unittest discover -s sidecar/tests -t sidecar/tests` | 110 passed, 3 skipped |
| `python -m unittest discover -s models/tests -t models/tests` | 13 passed |
| `python power-automate/validate_examples.py` | 4 examples validated |
| `npm run check:release` | 47 release/workflow/portable contract tests passed |
| Version, documentation, and CI-mirror gates | versions agree at 0.8.0; troubleshooting coverage (and self-test), button labels, CI parity, and dev-stub marker contracts passed |

### Hosted CI history and pending v0.8 evidence

A prior complete-workspace hosted CI run passed for this repository lineage.
That result demonstrates that the enabled five-job workflow can complete with
its deterministic development stubs; it does **not** certify the current v0.8
candidate. Before describing v0.8 as CI-green, hosted CI must pass for its
exact PR or main commit. The guarded Windows release workflow must then pass
its exact-CI provenance check and real-artifact packaging gates before any
release is published.

## Remaining validation before release

### Delivery-mode boundary

Local folder behavior is suitable for local, bounded verification: configure
separate roots, run preflight, process files, and reconcile renamed outputs,
receipts, and Quarantine as described in `docs/PILOT_RUNBOOK.md`. That is not
target-tenant or release certification. Power Automate / SharePoint still needs
the real tenant's Flow 1/Flow 2, connector permissions, indexing, archive, and
recovery behavior exercised end to end.

Everything the local gates cannot reach, because it needs Windows and real
artifacts: the DPAPI key path, the NSIS bundle,
install/repair/upgrade/uninstall, and an end-to-end run with the real sidecars
and model weights. The guarded GitHub release workflow builds the package on a
clean Windows runner after successful CI, verifies its staged inputs, and
publishes through a draft only after the artifact gates pass. See
`RELEASING.md` and `docs/RELEASE_CHECKLIST.md`.

## Release gates that cannot be completed in source alone

- produce and review `models/models.lock.json` from the final downloaded bundle;
- generate a hash-pinned `sidecar/requirements.lock` for any signed release;
- build, install, repair, upgrade, and uninstall the Windows NSIS package;
- confirm the installed app resolves its app-data model paths, and that the
  in-app downloader works from an empty models directory;
- observe runtime network traffic with networking disabled and with a connection
  monitor;
- execute the bounded Local folder acceptance path (ordinary file, duplicate
  physical copies, unrelated collision, restart/fault recovery, correction,
  dismissal, and full source/output/receipt/Quarantine reconciliation);
- build both Power Automate flows in the target tenant and force checkpoint
  failures. Local verification does not certify the manifest handoff,
  SharePoint index, cloud archive, or tenant recovery behavior;
- execute the 50, 200, and 500-document staged pilot; and
- obtain security, legal, operational, and pilot-owner approval.

Until those gates are complete, do not represent v0.8 as hosted-CI certified,
released, or signed. Any generated installer remains an unsigned internal pilot
candidate unless its applicable signing and publication gates have passed.
