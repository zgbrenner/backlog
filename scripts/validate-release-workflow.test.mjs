import assert from "node:assert/strict";
import test from "node:test";

import {
  validateBuildDependencyLock,
  validateReleaseWorkflow,
  validateRustToolchain,
} from "./validate-release-workflow.mjs";

const validWorkflow = `
name: Release
on:
  workflow_run:
    workflows: [CI]
    types: [completed]
    branches: [main]
permissions:
  contents: read
env:
  RELEASE_SHA: \${{ github.event.workflow_run.head_sha }}
jobs:
  release-check:
    if: github.event.workflow_run.conclusion == 'success'
    runs-on: ubuntu-24.04
    outputs:
      should-release: \${{ steps.release-gate.outputs.release }}
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
        with:
          ref: \${{ env.RELEASE_SHA }}
      - id: release-gate
        run: node scripts/release-contract.mjs gate
  release:
    needs: release-check
    if: needs.release-check.outputs.should-release == 'true'
    runs-on: windows-2022
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
        with:
          ref: \${{ env.RELEASE_SHA }}
      - uses: actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020
        with:
          node-version: 22
      - uses: actions/setup-python@a26af69be951a213d495a4c3e4e4022e16d87065
        with:
          python-version: "3.11"
      - name: Install pinned Rust
        run: rustup show
      - name: Install locked dependencies
        run: npm ci
      - name: Validate frontend
        run: npm run check
      - name: Validate Power Automate
        run: python power-automate/validate_examples.py
      - name: Stage verified release inputs
        run: pwsh scripts/stage-release-inputs.ps1
      - name: Build sidecar
        run: pwsh scripts/build-sidecar.ps1 -Clean
      - name: Verify release binaries
        run: pwsh scripts/verify-binaries.ps1
      - id: release-mode
        env:
          TAURI_SIGNING_PRIVATE_KEY: \${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
        run: node scripts/release-contract.mjs mode
      - name: Build signed installer
        if: steps.release-mode.outputs.signed == 'true'
        env:
          TAURI_SIGNING_PRIVATE_KEY: \${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: \${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
        run: npm run tauri build
      - name: Build unsigned installer
        if: steps.release-mode.outputs.signed == 'false'
        run: npm run tauri build -- --config scripts/tauri.unsigned.conf.json
      - name: Create signed updater manifest
        if: steps.release-mode.outputs.signed == 'true'
        run: node scripts/release-contract.mjs manifest --out latest.json
      - name: Verify updater signature against embedded public key
        if: steps.release-mode.outputs.signed == 'true'
        run: |
          $config = Get-Content src-tauri/tauri.conf.json | ConvertFrom-Json
          [Convert]::FromBase64String($config.plugins.updater.pubkey)
          $encodedSignature = Get-Content $env:SIGNATURE
          [Convert]::FromBase64String($encodedSignature)
          cargo run --locked --manifest-path src-tauri/Cargo.toml --example verify_updater_signature -- "$installer" "$signature" "$pubkey"
      - name: Publish signed stable release
        if: steps.release-mode.outputs.signed == 'true'
        run: |
          $release = gh release view "$tag" --json isDraft,name | ConvertFrom-Json
          if ($release.name -ne "BackLog v0.5.0") { throw "different release mode" }
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release upload "$tag" "$installer"
          gh release create "$tag" "BackLog_0.5.0_x64-setup.exe" "BackLog_0.5.0_x64-setup.exe.sig" "latest.json" --target "$env:RELEASE_SHA" --draft
          gh release view "$tag" --json assets
          Compare-Object $expected $actual
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release edit "$tag" --draft=false --latest
      - name: Publish unsigned prerelease
        if: steps.release-mode.outputs.signed == 'false'
        run: |
          $release = gh release view "$tag" --json isDraft,name | ConvertFrom-Json
          if ($release.name -ne "BackLog v0.5.0 (unsigned prerelease)") { throw "different release mode" }
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release upload "$tag" "$installer"
          gh release create "$tag" "$installer" --target "$env:RELEASE_SHA" --draft --notes "Unsigned installer; v0.4.4 remains the stable updater"
          gh release view "$tag" --json assets
          Compare-Object $expected $actual
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release edit "$tag" --draft=false --prerelease
`;

const validStageScript = `
$StubMarker = "BACKLOG-DEV-STUB-DO-NOT-SHIP"
$PrimaryModelSha256 = "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031"
$LlamaArchiveSha256 = "b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee"
if ((Get-FileHash $model -Algorithm SHA256).Hash.ToLowerInvariant() -ne $PrimaryModelSha256) {
  throw "Primary model hash mismatch"
}
`;

test("the guarded Windows release structure is accepted", () => {
  assert.deepEqual(validateReleaseWorkflow(validWorkflow, validStageScript), []);
});

test("a workflow that does not wait for successful main CI is rejected", () => {
  const manualOnly = validWorkflow.replace(
    "  workflow_run:\n    workflows: [CI]\n    types: [completed]\n    branches: [main]\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(manualOnly, validStageScript).join("\n"),
    /successful CI on main/,
  );
});

test("a failed CI run cannot allocate the release build", () => {
  const unguarded = validWorkflow.replace(
    "    if: github.event.workflow_run.conclusion == 'success'\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(unguarded, validStageScript).join("\n"),
    /successful CI conclusion/,
  );
});

test("the release must check out and tag the CI-tested commit", () => {
  const drifting = validWorkflow.replace(
    "${{ github.event.workflow_run.head_sha }}",
    "${{ github.sha }}",
  );
  assert.match(
    validateReleaseWorkflow(drifting, validStageScript).join("\n"),
    /CI-tested commit/,
  );
});

test("a manual dispatch path that bypasses exact successful CI provenance is rejected", () => {
  const manual = validWorkflow
    .replace("  workflow_run:\n", "  workflow_dispatch:\n  workflow_run:\n")
    .replace(
      "    if: github.event.workflow_run.conclusion == 'success'\n",
      "    if: github.event_name == 'workflow_dispatch' || github.event.workflow_run.conclusion == 'success'\n",
    );
  assert.match(
    validateReleaseWorkflow(manual, validStageScript).join("\n"),
    /must not expose a manual CI bypass/,
  );
});

test("signed publication without cryptographic verification against the embedded key is rejected", () => {
  const unverified = validWorkflow.replace(
    "--example verify_updater_signature",
    "--example does_not_verify",
  );
  assert.match(
    validateReleaseWorkflow(unverified, validStageScript).join("\n"),
    /embedded updater public key/,
  );
});

test("Tauri's base64-wrapped signature must be decoded before Minisign verification", () => {
  const unwrapped = validWorkflow.replace(
    "          $encodedSignature = Get-Content $env:SIGNATURE\n" +
      "          [Convert]::FromBase64String($encodedSignature)\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(unwrapped, validStageScript).join("\n"),
    /base64-wrapped Tauri signature/,
  );
});

test("release publication must stay draft until every asset upload succeeds", () => {
  const eager = validWorkflow
    .replaceAll(" --draft", "")
    .replaceAll("gh release edit \"$tag\" --draft=false --latest\n", "")
    .replaceAll("gh release edit \"$tag\" --draft=false --prerelease\n", "");
  assert.match(
    validateReleaseWorkflow(eager, validStageScript).join("\n"),
    /draft until every artifact is uploaded/,
  );
});

test("a draft cannot publish until its remote assets exactly match its release mode", () => {
  const unchecked = validWorkflow.replaceAll(
    "          gh release view \"$tag\" --json assets\n" +
      "          Compare-Object $expected $actual\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(unchecked, validStageScript).join("\n"),
    /exact remote draft asset set/,
  );
});

test("an interrupted draft must retain its original signed or unsigned mode", () => {
  const modeBlind = validWorkflow
    .replace('          if ($release.name -ne "BackLog v0.5.0") { throw "different release mode" }\n', "")
    .replace('          if ($release.name -ne "BackLog v0.5.0 (unsigned prerelease)") { throw "different release mode" }\n', "");
  assert.match(
    validateReleaseWorkflow(modeBlind, validStageScript).join("\n"),
    /durable signed or unsigned mode/,
  );
});

test("the tag target must be rechecked before retry mutation and publication", () => {
  const unchecked = validWorkflow.replaceAll(
    '          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"\n',
    "",
  );
  assert.match(
    validateReleaseWorkflow(unchecked, validStageScript).join("\n"),
    /recheck the release tag target/,
  );
});

test("branch-local tag checks cannot replace the final pre-publication check", () => {
  const noFinalCheck = validWorkflow.replaceAll(
    '          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"\n' +
      "          gh release edit",
    "          gh release edit",
  );
  assert.match(
    validateReleaseWorkflow(noFinalCheck, validStageScript).join("\n"),
    /recheck the release tag target/,
  );
});

test("a Windows build that is not gated by the absent-tag check is rejected", () => {
  const ungated = validWorkflow
    .replace("    needs: release-check\n", "")
    .replace("    if: needs.release-check.outputs.should-release == 'true'\n", "");
  assert.match(
    validateReleaseWorkflow(ungated, validStageScript).join("\n"),
    /absent-tag preflight/,
  );
});

test("an unsigned publication that uploads updater metadata is rejected", () => {
  const unsafe = validWorkflow.replace(
    'gh release create "$tag" "$installer" --target',
    'gh release create "$tag" "$installer" "$manifest" --target',
  );
  assert.match(
    validateReleaseWorkflow(unsafe, validStageScript).join("\n"),
    /unsigned publish step must not reference updater metadata/,
  );
});

test("a floating Windows runner is rejected", () => {
  const floating = validWorkflow.replace("windows-2022", "windows-latest");
  assert.match(
    validateReleaseWorkflow(floating, validStageScript).join("\n"),
    /windows-2022/,
  );
});

test("mutable release action tags are rejected", () => {
  const floating = validWorkflow.replace(
    "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020",
    "actions/setup-node@v4",
  );
  assert.match(
    validateReleaseWorkflow(floating, validStageScript).join("\n"),
    /pinned to full commit SHAs/,
  );
});

test("a changed primary model hash is rejected", () => {
  const changed = validStageScript.replace("9465e63a", "00000000");
  assert.match(
    validateReleaseWorkflow(validWorkflow, changed).join("\n"),
    /primary model SHA-256/,
  );
});

const validBuildScript = `
$buildLock = Join-Path $SidecarDir "build-requirements.lock"
python -m pip install -r $buildLock
`;
const validBuildLock = `
PyInstaller==6.21.0
pyinstaller-hooks-contrib==2026.6
altgraph==0.17.5
packaging==26.2
pefile==2024.8.26
pywin32-ctypes==0.2.3
setuptools==83.0.0
`;

test("the sidecar freezer and every build helper are exact", () => {
  assert.deepEqual(
    validateBuildDependencyLock(validBuildScript, validBuildLock),
    [],
  );
});

test("a floating PyInstaller install is rejected", () => {
  const floating = `${validBuildScript}\npython -m pip install "pyinstaller>=6,<7"`;
  assert.match(
    validateBuildDependencyLock(floating, validBuildLock).join("\n"),
    /floating PyInstaller/,
  );
});

test("a missing or non-exact build helper is rejected", () => {
  const incomplete = validBuildLock
    .replace("pefile==2024.8.26\n", "")
    .replace("setuptools==83.0.0", "setuptools>=83");
  const problems = validateBuildDependencyLock(validBuildScript, incomplete).join("\n");
  assert.match(problems, /missing pefile/);
  assert.match(problems, /not exactly pinned: setuptools>=83/);
  assert.match(problems, /missing setuptools/);
});

test("the Rust compiler is pinned through the patch version", () => {
  assert.deepEqual(validateRustToolchain('channel = "1.94.1"'), []);
  assert.match(
    validateRustToolchain('channel = "1.94"').join("\n"),
    /major\.minor\.patch/,
  );
});
