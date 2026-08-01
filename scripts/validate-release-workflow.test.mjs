import assert from "node:assert/strict";
import test from "node:test";

import {
  validateBuildDependencyLock,
  validateReleaseDraftRetarget,
  validateReleaseWorkflow,
  validateReleaseTargetGuard,
  validateRustToolchain,
  validateWebviewRuntimeSource,
} from "./validate-release-workflow.mjs";

const validWorkflow = `
name: Release
on:
  push:
    branches: [main]
permissions:
  contents: read
concurrency:
  group: release-\${{ github.sha }}
  cancel-in-progress: true
env:
  RELEASE_SHA: \${{ github.sha }}
jobs:
  release-check:
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-24.04
    timeout-minutes: 20
    outputs:
      should-release: \${{ steps.release-gate.outputs.release }}
      version: \${{ steps.release-metadata.outputs.version }}
      tag: \${{ steps.release-metadata.outputs.tag }}
      portable: \${{ steps.release-metadata.outputs.portable }}
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
        with:
          ref: \${{ env.RELEASE_SHA }}
      - name: Wait for exact successful main CI
        id: ci-gate
        run: node scripts/release-contract.mjs ci-gate --sha "$RELEASE_SHA"
      - id: release-metadata
        run: node scripts/release-contract.mjs metadata
      - id: release-gate
        env:
          TAG: \${{ steps.release-metadata.outputs.tag }}
        run: node scripts/release-contract.mjs gate
  release:
    needs: release-check
    if: needs.release-check.outputs.should-release == 'true'
    runs-on: windows-2022
    permissions:
      contents: write
    env:
      VERSION: \${{ needs.release-check.outputs.version }}
      TAG: \${{ needs.release-check.outputs.tag }}
      INSTALLER: "src-tauri/target/release/bundle/nsis/BackLog_\${{ needs.release-check.outputs.version }}_x64-setup.exe"
      PORTABLE: "src-tauri/target/release/\${{ needs.release-check.outputs.portable }}"
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
        with:
          ref: \${{ env.RELEASE_SHA }}
      - name: Set runner-local release paths
        shell: pwsh
        run: |
          $runtimeDir = Join-Path $env:RUNNER_TEMP "backlog-webview2-fixed"
          "WEBVIEW2_RUNTIME_DIR=$runtimeDir" >> $env:GITHUB_ENV
      - uses: actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020
        with:
          node-version: 22
      - uses: actions/setup-python@a26af69be951a213d495a4c3e4e4022e16d87065
        with:
          python-version: "3.11"
      - name: Install pinned Rust
        run: rustup show
      - name: Select native Windows build tools
        run: |
          $nasmVersion = "2.16.03"
          $nasmHash = "3ee4782247bcb874378d02f7eab4e294a84d3d15f3f6ee2de2f47a46aa7226e6"
          Invoke-WebRequest -Uri "https://www.nasm.us/pub/nasm/releasebuilds/$nasmVersion/win64/nasm.zip"
          Get-FileHash nasm.zip
          Expand-Archive nasm.zip
          $actualPerl = perl --version
          $perlExit = $LASTEXITCODE
          $perlText = $actualPerl -join "\`n"
          if ($perlExit -ne 0 -or $perlText -notmatch "built for MSWin32-x64") { throw "native Perl is unavailable" }
          $actualPerl | Select-Object -First 2
      - name: Install locked dependencies
        run: npm ci
      - name: Validate frontend
        run: npm run check
      - name: Validate Power Automate
        run: python power-automate/validate_examples.py
      - name: Stage verified release inputs
        run: pwsh scripts/stage-release-inputs.ps1
      - name: Stage pinned fixed WebView2 runtime
        run: pwsh scripts/stage-webview2-runtime.ps1 -Destination $env:WEBVIEW2_RUNTIME_DIR -Clean
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
      - name: Build installer-free portable ZIP
        run: pwsh scripts/package-portable.ps1 -Version $env:VERSION -Output $env:PORTABLE -WebView2RuntimeDir $env:WEBVIEW2_RUNTIME_DIR
      - name: Verify installer-free portable ZIP
        run: pwsh scripts/validate-portable-package.ps1 -Archive $env:PORTABLE -Version $env:VERSION
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
           if ($release.name -ne "BackLog v$env:VERSION") { throw "different release mode" }
           ./scripts/retarget-release-draft.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA" -ExpectedName "BackLog v$env:VERSION"
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
           gh release upload "$tag" "$installer" "$env:PORTABLE"
            gh release create "$tag" "$env:INSTALLER" "$env:PORTABLE" "$env:SIGNATURE" "latest.json" --target "$env:RELEASE_SHA" --draft
          gh release view "$tag" --json assets
          Compare-Object $expected $actual
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release edit "$tag" --draft=false --latest
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
      - name: Publish unsigned prerelease
        if: steps.release-mode.outputs.signed == 'false'
        run: |
          $release = gh release view "$tag" --json isDraft,name | ConvertFrom-Json
           if ($release.name -ne "BackLog v$env:VERSION (unsigned prerelease)") { throw "different release mode" }
           ./scripts/retarget-release-draft.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA" -ExpectedName "BackLog v$env:VERSION (unsigned prerelease)"
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
           gh release upload "$tag" "$installer" "$env:PORTABLE"
           gh release create "$tag" "$installer" "$env:PORTABLE" --target "$env:RELEASE_SHA" --draft --notes "Unsigned installer; v0.4.4 remains the stable updater"
          gh release view "$tag" --json assets
          Compare-Object $expected $actual
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
          gh release edit "$tag" --draft=false --prerelease
          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"
`;

const validStageScript = `
$StubMarker = "BACKLOG-DEV-STUB-DO-NOT-SHIP"
$PrimaryModelSha256 = "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031"
$LlamaArchiveSha256 = "b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee"
if ((Get-FileHash $model -Algorithm SHA256).Hash.ToLowerInvariant() -ne $PrimaryModelSha256) {
  throw "Primary model hash mismatch"
}
`;

const validWebviewRuntimeSource = `
$WebView2Version = "151.0.4129.59"
$WebView2Url = "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/3cb717d2-b86d-4160-a13e-f3860141dc7f/Microsoft.WebView2.FixedVersionRuntime.151.0.4129.59.x64.cab"
$WebView2Sha256 = "056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc"
$WebView2CabSize = [int64]304114944
$WebView2FileCount = 256
$ChunkSize = [int64](16MB)
[System.Net.Http.HttpClient]::new()
[System.Net.Http.Headers.RangeHeaderValue]::new($start, $end)
[System.Net.Http.HttpCompletionOption]::ResponseHeadersRead
if ([int]$response.StatusCode -ne 206) { throw "not a range" }
$response.Content.ReadAsStreamAsync()
Get-FileHash -LiteralPath $cab -Algorithm SHA256
7za.exe x $cab "-o$destination" -y
expand.exe -F:* $cab $destination
msedgewebview2.exe
runtime-manifest.json
webview2-fixed
backlog-webview2-fixed-download
destination and download directory must be different paths
`;

test("the guarded Windows release structure is accepted", () => {
  assert.deepEqual(validateReleaseWorkflow(validWorkflow, validStageScript), []);
});

test("the portable build pins and stages a fixed WebView2 runtime", () => {
  assert.deepEqual(validateWebviewRuntimeSource(validWebviewRuntimeSource), []);
});

test("a changed fixed WebView2 CAB hash is rejected", () => {
  const changed = validWebviewRuntimeSource.replace("056858a0", "00000000");
  assert.match(
    validateWebviewRuntimeSource(changed).join("\n"),
    /WebView2 staging script is missing.*056858/,
  );
});

test("a workflow that does not wait for successful main CI is rejected", () => {
  const manualOnly = validWorkflow.replace(
    "  push:\n    branches: [main]\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(manualOnly, validStageScript).join("\n"),
    /main pushes/,
  );
});

test("a failed CI run cannot allocate the release build", () => {
  const unguarded = validWorkflow.replace(
    "      - name: Wait for exact successful main CI\n" +
      "        id: ci-gate\n" +
      "        run: node scripts/release-contract.mjs ci-gate --sha \"$RELEASE_SHA\"\n",
    "",
  );
  assert.match(
    validateReleaseWorkflow(unguarded, validStageScript).join("\n"),
    /wait for successful CI on the exact main SHA/,
  );
});

test("the release must check out and tag the CI-tested commit", () => {
  const drifting = validWorkflow.replace(
    "          ref: ${{ env.RELEASE_SHA }}",
    "          ref: ${{ github.sha }}",
  );
  assert.match(
    validateReleaseWorkflow(drifting, validStageScript).join("\n"),
    /exact pushed commit/,
  );
});

test("a manual dispatch path that bypasses exact successful CI provenance is rejected", () => {
  const manual = validWorkflow
    .replace("  push:\n", "  workflow_dispatch:\n  push:\n")
    .replace(
      "    if: github.ref == 'refs/heads/main'\n",
      "    if: github.event_name == 'workflow_dispatch' || github.ref == 'refs/heads/main'\n",
    );
  assert.match(
    validateReleaseWorkflow(manual, validStageScript).join("\n"),
    /must not expose a manual CI bypass/,
  );
});

test("a newer tested main commit cancels superseded release packaging", () => {
  const wasteful = validWorkflow.replace(
    "  cancel-in-progress: true",
    "  cancel-in-progress: false",
  );
  assert.match(
    validateReleaseWorkflow(wasteful, validStageScript).join("\n"),
    /cancel superseded packaging work/,
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
    .replace('          if ($release.name -ne "BackLog v$env:VERSION") { throw "different release mode" }\n', "")
    .replace('          if ($release.name -ne "BackLog v$env:VERSION (unsigned prerelease)") { throw "different release mode" }\n', "");
  assert.match(
    validateReleaseWorkflow(modeBlind, validStageScript).join("\n"),
    /durable signed or unsigned mode/,
  );
});

test("an interrupted draft must be ancestry-checked before its asset is replaced", () => {
  const unsafeRetry = validWorkflow.replaceAll(
    /^\s+\.\/scripts\/retarget-release-draft\.ps1.*\n/gm,
    "",
  );
  assert.match(
    validateReleaseWorkflow(unsafeRetry, validStageScript).join("\n"),
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
    /recheck the release target/,
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
    /recheck the release target/,
  );
});

test("the published tag must be checked after the draft becomes visible", () => {
  const noPublishedTagCheck = validWorkflow.replaceAll(
    "          gh release edit \"$tag\" --draft=false --latest\n" +
      '          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"\n',
    "          gh release edit \"$tag\" --draft=false --latest\n",
  ).replaceAll(
    "          gh release edit \"$tag\" --draft=false --prerelease\n" +
      '          ./scripts/assert-release-tag.ps1 -Tag "$tag" -ExpectedSha "$env:RELEASE_SHA"\n',
    "          gh release edit \"$tag\" --draft=false --prerelease\n",
  );
  assert.match(
    validateReleaseWorkflow(noPublishedTagCheck, validStageScript).join("\n"),
    /after publication/,
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
    'gh release create "$tag" "$installer" "$env:PORTABLE" --target',
    'gh release create "$tag" "$installer" "$env:PORTABLE" "$manifest" --target',
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

test("a changed NASM hash is rejected", () => {
  const changed = validWorkflow.replace("3ee47822", "00000000");
  assert.match(
    validateReleaseWorkflow(changed, validStageScript).join("\n"),
    /exact NASM archive/,
  );
});

test("Perl version output must be captured before it is shortened for display", () => {
  const brokenPipe = validWorkflow
    .replace("          $actualPerl = perl --version\n", "")
    .replace("          $perlExit = $LASTEXITCODE\n", "")
    .replace(
      "          $actualPerl | Select-Object -First 2\n",
      "          perl --version | Select-Object -First 2\n",
    );
  assert.match(
    validateReleaseWorkflow(brokenPipe, validStageScript).join("\n"),
    /without truncating the live process output/,
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

const validReleaseTargetGuard = `
$rows = @(& git ls-remote --exit-code --tags origin $baseRef $peeledRef)
$tagExit = $LASTEXITCODE
if ($tagExit -eq 0) {
  if ($resolved.Sha.ToLowerInvariant() -ne $ExpectedSha.ToLowerInvariant()) { throw "wrong tag" }
  return
}
if ($tagExit -ne 2) { throw "tag lookup failed" }
$draftExit = 1
for ($attempt = 1; $attempt -le 8; $attempt++) {
  $draftJson = & gh release view $Tag --json isDraft,tagName,targetCommitish
  $draftExit = $LASTEXITCODE
  if ($draftExit -eq 0) { break }
  Start-Sleep -Seconds 3
}
$draft = $draftJson | ConvertFrom-Json
if (-not $draft.isDraft) { throw "not a draft" }
if ($draft.tagName -ne $Tag) { throw "wrong tag name" }
if ($draft.targetCommitish.ToLowerInvariant() -ne $ExpectedSha.ToLowerInvariant()) { throw "wrong target" }
`;

test("the release target guard accepts the real tag or an exact-SHA draft", () => {
  assert.deepEqual(validateReleaseTargetGuard(validReleaseTargetGuard), []);
});

test("a draft target that is not compared with the tested SHA is rejected", () => {
  const driftingDraft = validReleaseTargetGuard.replace(
    "$draft.targetCommitish.ToLowerInvariant()",
    "$draft.name",
  );
  assert.match(
    validateReleaseTargetGuard(driftingDraft).join("\n"),
    /real tag or an exact-SHA GitHub draft/,
  );
});

test("draft lookup retries are required after a GitHub target mutation", () => {
  const noConsistencyRetry = validReleaseTargetGuard
    .replace("for ($attempt = 1; $attempt -le 8; $attempt++) {", "if ($true) {")
    .replace("  Start-Sleep -Seconds 3\n", "");
  assert.match(
    validateReleaseTargetGuard(noConsistencyRetry).join("\n"),
    /real tag or an exact-SHA GitHub draft/,
  );
});

const validDraftRetarget = `
gh release view $Tag --json databaseId,isDraft,name,tagName,targetCommitish
if ($release.targetCommitish -notmatch '^[0-9a-fA-F]{40}$') { throw "not immutable" }
git ls-remote --exit-code --tags origin "refs/tags/$Tag"
if ($tagExit -ne 2) { throw "tag exists or lookup failed" }
gh api "repos/$env:GITHUB_REPOSITORY/compare/$oldSha...$newSha"
if ($comparison.status -ne "ahead") { throw "not a descendant" }
gh api --method PATCH "repos/$env:GITHUB_REPOSITORY/releases/$id" -f "target_commitish=$ExpectedSha"
./scripts/assert-release-tag.ps1
`;

test("an untagged draft may advance only to a tested descendant", () => {
  assert.deepEqual(validateReleaseDraftRetarget(validDraftRetarget), []);
});

test("draft retargeting without a remote ancestry check is rejected", () => {
  const unchecked = validDraftRetarget.replace(
    'if ($comparison.status -ne "ahead") { throw "not a descendant" }\n',
    "",
  );
  assert.match(
    validateReleaseDraftRetarget(unchecked).join("\n"),
    /only an untagged draft to advance to a tested descendant/,
  );
});
