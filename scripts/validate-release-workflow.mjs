#!/usr/bin/env node

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { parse } from "yaml";

const PRIMARY_SHA256 =
  "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031";
const LLAMA_SHA256 =
  "b2d991bdd37258bb51309f50e9fb7a52a16fe662ba71b2cbbbbb9303b47b5dee";

const stepRun = (step) => String(step?.run ?? "");
const normalizedIf = (step) => String(step?.if ?? "").replaceAll(/\s+/g, "");

export function validateReleaseWorkflow(workflowSource, stageSource) {
  const problems = [];
  let workflow;
  try {
    workflow = parse(workflowSource);
  } catch (error) {
    return [`release.yml is not valid YAML: ${error.message}`];
  }

  if (Object.prototype.hasOwnProperty.call(workflow?.on ?? {}, "workflow_dispatch")) {
    problems.push("release workflow must not expose a manual CI bypass");
  }
  const workflowRun = workflow?.on?.workflow_run;
  if (
    !Array.isArray(workflowRun?.workflows) ||
    !workflowRun.workflows.includes("CI") ||
    !Array.isArray(workflowRun?.types) ||
    !workflowRun.types.includes("completed") ||
    !Array.isArray(workflowRun?.branches) ||
    !workflowRun.branches.includes("main")
  ) {
    problems.push("release workflow must run after successful CI on main");
  }
  if (workflow?.permissions?.contents !== "read") {
    problems.push("release workflow default permissions must be read-only");
  }
  if (
    String(workflow?.env?.RELEASE_SHA ?? "") !==
    "${{ github.event.workflow_run.head_sha }}"
  ) {
    problems.push("release workflow must identify the exact CI-tested commit");
  }

  const preflight = workflow?.jobs?.["release-check"];
  if (!preflight) {
    problems.push("release workflow must define an absent-tag preflight job");
  } else {
    const preflightCondition = normalizedIf(preflight);
    if (preflightCondition !== "github.event.workflow_run.conclusion=='success'") {
      problems.push("release preflight must require a successful CI conclusion");
    }
    const preflightSteps = Array.isArray(preflight.steps) ? preflight.steps : [];
    const preflightCheckout = preflightSteps.find((step) =>
      String(step?.uses ?? "").startsWith("actions/checkout@"),
    );
    if (String(preflightCheckout?.with?.ref ?? "") !== "${{ env.RELEASE_SHA }}") {
      problems.push("release preflight must check out the exact CI-tested commit");
    }
    const gate = preflightSteps.find((step) => step?.id === "release-gate");
    if (!stepRun(gate).includes("release-contract.mjs gate")) {
      problems.push("absent-tag preflight must run release-contract.mjs gate");
    }
    if (
      String(preflight?.outputs?.["should-release"] ?? "") !==
      "${{ steps.release-gate.outputs.release }}"
    ) {
      problems.push("absent-tag preflight must expose its release decision");
    }
  }

  const job = workflow?.jobs?.release;
  if (!job) return [...problems, 'release workflow must define a "release" job'];
  if (
    job.needs !== "release-check" ||
    String(job.if ?? "").replaceAll(/\s+/g, "") !==
      "needs.release-check.outputs.should-release=='true'"
  ) {
    problems.push("Windows release build must require the absent-tag preflight");
  }
  if (job["runs-on"] !== "windows-2022") {
    problems.push("release job must use the reproducible windows-2022 runner");
  }
  if (job?.permissions?.contents !== "write") {
    problems.push("only the Windows publication job may receive contents: write");
  }
  const steps = Array.isArray(job.steps) ? job.steps : [];
  const findNamed = (name) => steps.find((step) => step?.name === name);
  const findUsing = (prefix) =>
    steps.find((step) => String(step?.uses ?? "").startsWith(prefix));
  if (String(findUsing("actions/checkout@")?.with?.ref ?? "") !== "${{ env.RELEASE_SHA }}") {
    problems.push("Windows release must check out the exact CI-tested commit");
  }
  const allActionUses = [
    ...(Array.isArray(preflight?.steps) ? preflight.steps : []),
    ...steps,
  ]
    .map((step) => String(step?.uses ?? ""))
    .filter(Boolean);
  if (
    allActionUses.some((uses) =>
      /^(actions\/checkout|actions\/setup-node|actions\/setup-python)@/.test(uses) &&
      !/@[0-9a-f]{40}$/i.test(uses)
    )
  ) {
    problems.push("release workflow actions must be pinned to full commit SHAs");
  }

  if (findUsing("actions/setup-node@")?.with?.["node-version"] !== 22) {
    problems.push("release workflow must use Node 22");
  }
  if (String(findUsing("actions/setup-python@")?.with?.["python-version"]) !== "3.11") {
    problems.push("release workflow must use Python 3.11");
  }
  if (!steps.some((step) => /rustup/.test(stepRun(step)))) {
    problems.push("release workflow must install the rust-toolchain.toml toolchain");
  }

  const allRuns = steps.map(stepRun).join("\n");
  for (const [label, command] of [
    ["locked npm install", "npm ci"],
    ["frontend validation", "npm run check"],
    ["Power Automate validation", "power-automate/validate_examples.py"],
    ["release input staging", "scripts/stage-release-inputs.ps1"],
    ["sidecar build", "scripts/build-sidecar.ps1"],
    ["binary release gate", "scripts/verify-binaries.ps1"],
  ]) {
    if (!allRuns.includes(command)) problems.push(`release workflow is missing ${label}`);
  }

  const mode = steps.find((step) => step?.id === "release-mode");
  if (!stepRun(mode).includes("release-contract.mjs mode")) {
    problems.push("release mode must be selected by release-contract.mjs");
  }
  if (
    String(mode?.env?.TAURI_SIGNING_PRIVATE_KEY ?? "") !==
    "${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}"
  ) {
    problems.push("release mode must consume TAURI_SIGNING_PRIVATE_KEY from secrets");
  }

  const signedBuild = findNamed("Build signed installer");
  const unsignedBuild = findNamed("Build unsigned installer");
  const signedCondition = "steps.release-mode.outputs.signed=='true'";
  const unsignedCondition = "steps.release-mode.outputs.signed=='false'";
  if (normalizedIf(signedBuild) !== signedCondition) {
    problems.push("signed build must run only when the updater key is present");
  }
  if (normalizedIf(unsignedBuild) !== unsignedCondition) {
    problems.push("unsigned build must run only when the updater key is absent");
  }
  if (!stepRun(signedBuild).includes("npm run tauri build")) {
    problems.push("signed release must build the Tauri installer");
  }
  if (
    String(signedBuild?.env?.TAURI_SIGNING_PRIVATE_KEY ?? "") !==
      "${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}" ||
    String(signedBuild?.env?.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "") !==
      "${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}"
  ) {
    problems.push("signed build must receive the updater key and optional password");
  }
  if (
    !stepRun(unsignedBuild).includes("npm run tauri build") ||
    !stepRun(unsignedBuild).includes("tauri.unsigned.conf.json")
  ) {
    problems.push("unsigned build must explicitly disable updater artifacts");
  }

  const manifest = findNamed("Create signed updater manifest");
  if (
    normalizedIf(manifest) !== signedCondition ||
    !stepRun(manifest).includes("release-contract.mjs manifest")
  ) {
    problems.push("latest.json must be created only by the signed release branch");
  }

  const signatureVerification = findNamed(
    "Verify updater signature against embedded public key",
  );
  const signatureRun = stepRun(signatureVerification);
  if (
    normalizedIf(signatureVerification) !== signedCondition ||
    !signatureRun.includes("src-tauri/tauri.conf.json") ||
    !signatureRun.includes("--example verify_updater_signature") ||
    !signatureRun.includes("--locked")
  ) {
    problems.push(
      "signed publication must cryptographically verify against the embedded updater public key",
    );
  }
  if (
    (signatureRun.match(/FromBase64String/g) ?? []).length < 2 ||
    !signatureRun.includes("Get-Content $env:SIGNATURE")
  ) {
    problems.push(
      "signature verification must decode the base64-wrapped Tauri signature",
    );
  }

  const signedPublish = findNamed("Publish signed stable release");
  const signedRun = stepRun(signedPublish);
  if (normalizedIf(signedPublish) !== signedCondition) {
    problems.push("stable release publication must require the updater key");
  }
  for (const [label, alternatives] of [
    [".exe", [".exe", "$env:INSTALLER"]],
    [".sig", [".sig", "$env:SIGNATURE"]],
    ["latest.json", ["latest.json", "$env:MANIFEST"]],
    ["--target", ["--target"]],
    ["RELEASE_SHA", ["RELEASE_SHA"]],
    ["--draft", ["--draft"]],
    ["gh release edit", ["gh release edit"]],
    ["--draft=false", ["--draft=false"]],
    ["--latest", ["--latest"]],
  ]) {
    if (!alternatives.some((required) => signedRun.includes(required))) {
      problems.push(`signed stable publish step must include ${label}`);
    }
  }

  const unsignedPublish = findNamed("Publish unsigned prerelease");
  const unsignedRun = stepRun(unsignedPublish);
  if (normalizedIf(unsignedPublish) !== unsignedCondition) {
    problems.push("unsigned publication must run only when the updater key is absent");
  }
  if (!unsignedRun.includes("--prerelease")) {
    problems.push("unsigned publication must be a prerelease");
  }
  for (const required of [
    "--target",
    "RELEASE_SHA",
    "--draft",
    "gh release edit",
    "--draft=false",
  ]) {
    if (!unsignedRun.includes(required)) {
      problems.push(
        "release publication must remain a draft until every artifact is uploaded",
      );
      break;
    }
  }
  if (
    unsignedRun.includes(".sig") ||
    unsignedRun.includes("latest.json") ||
    unsignedRun.includes("$signature") ||
    unsignedRun.includes("$manifest")
  ) {
    problems.push("unsigned publish step must not reference updater metadata");
  }
  if (!unsignedRun.includes("v0.4.4 remains the stable updater")) {
    problems.push("unsigned prerelease note must say v0.4.4 remains the stable updater");
  }

  if (
    !signedRun.includes("--draft") ||
    !signedRun.includes("gh release edit") ||
    !signedRun.includes("--draft=false")
  ) {
    problems.push("release publication must remain a draft until every artifact is uploaded");
  }
  if (
    !signedRun.includes("--json assets") ||
    !signedRun.includes("Compare-Object") ||
    !unsignedRun.includes("--json assets") ||
    !unsignedRun.includes("Compare-Object")
  ) {
    problems.push("each release mode must prove its exact remote draft asset set");
  }
  const assertRetryState = (run, expectedName) => {
    const modeCheck = run.indexOf(`$release.name -ne "${expectedName}"`);
    const upload = run.indexOf("gh release upload");
    const finalAssetCheck = run.lastIndexOf("Compare-Object");
    const edit = run.indexOf("gh release edit");
    const tagChecks = [...run.matchAll(/assert-release-tag\.ps1/g)].map(
      (match) => match.index,
    );
    if (
      !run.includes("--json isDraft,name") ||
      modeCheck < 0 ||
      upload < 0 ||
      modeCheck > upload
    ) {
      problems.push(
        `interrupted ${expectedName} drafts must retain their durable signed or unsigned mode`,
      );
    }
    if (
      tagChecks.length < 2 ||
      tagChecks[0] > upload ||
      tagChecks.at(-1) < finalAssetCheck ||
      tagChecks.at(-1) > edit
    ) {
      problems.push(
        `${expectedName} retries must recheck the release tag target before mutation and publication`,
      );
    }
  };
  assertRetryState(signedRun, "BackLog v0.5.0");
  assertRetryState(unsignedRun, "BackLog v0.5.0 (unsigned prerelease)");
  const signatureIndex = steps.indexOf(signatureVerification);
  const signedPublishIndex = steps.indexOf(signedPublish);
  if (
    signatureIndex < 0 ||
    signedPublishIndex < 0 ||
    signatureIndex >= signedPublishIndex
  ) {
    problems.push("updater signature verification must finish before stable publication");
  }

  if (!stageSource.includes(`"${PRIMARY_SHA256}"`)) {
    problems.push("stage script must pin the primary model SHA-256");
  }
  if (!stageSource.includes(`"${LLAMA_SHA256}"`)) {
    problems.push("stage script must pin the llama.cpp archive SHA-256");
  }
  if (!stageSource.includes("Primary model hash mismatch")) {
    problems.push("stage script must fail closed on a primary model hash mismatch");
  }
  if (!stageSource.includes('"BACKLOG-DEV-STUB-DO-NOT-SHIP"')) {
    problems.push("stage script must explicitly reject the development stub marker");
  }

  return problems;
}

export function validateBuildDependencyLock(buildScriptSource, buildLockSource) {
  const problems = [];
  if (!buildScriptSource.includes('-r $buildLock')) {
    problems.push("sidecar build must install its build dependency lock");
  }
  if (/pyinstaller\s*[><~!]=?/i.test(buildScriptSource)) {
    problems.push("sidecar build must not install a floating PyInstaller range");
  }
  const required = [
    "PyInstaller",
    "pyinstaller-hooks-contrib",
    "altgraph",
    "packaging",
    "pefile",
    "pywin32-ctypes",
    "setuptools",
  ];
  const locked = new Map();
  for (const rawLine of buildLockSource.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const match = /^([A-Za-z0-9_.-]+)==([A-Za-z0-9_.+-]+)$/.exec(line);
    if (!match) {
      problems.push(`build dependency is not exactly pinned: ${line}`);
      continue;
    }
    locked.set(match[1].toLowerCase(), match[2]);
  }
  for (const name of required) {
    if (!locked.has(name.toLowerCase())) {
      problems.push(`build dependency lock is missing ${name}`);
    }
  }
  return problems;
}

export function validateRustToolchain(toolchainSource) {
  const channel = /^\s*channel\s*=\s*"([^"]+)"\s*$/m.exec(toolchainSource)?.[1];
  if (!channel) return ["rust-toolchain.toml must declare a channel"];
  if (!/^\d+\.\d+\.\d+$/.test(channel)) {
    return ["Rust must be pinned to an exact major.minor.patch version"];
  }
  return [];
}

function runCli() {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
  const workflow = readFileSync(path.join(root, ".github/workflows/release.yml"), "utf8");
  const stage = readFileSync(path.join(root, "scripts/stage-release-inputs.ps1"), "utf8");
  const buildScript = readFileSync(path.join(root, "scripts/build-sidecar.ps1"), "utf8");
  const buildLock = readFileSync(
    path.join(root, "sidecar/build-requirements.lock"),
    "utf8",
  );
  const toolchain = readFileSync(path.join(root, "rust-toolchain.toml"), "utf8");
  const problems = [
    ...validateReleaseWorkflow(workflow, stage),
    ...validateBuildDependencyLock(buildScript, buildLock),
    ...validateRustToolchain(toolchain),
  ];
  if (problems.length) {
    console.error("Release workflow contract is broken:");
    for (const problem of problems) console.error(`  - ${problem}`);
    process.exitCode = 1;
    return;
  }
  console.log(
    "Release workflow contract holds: Windows 2022, pinned inputs, and guarded updater publication.",
  );
}

const isMain = process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) runCli();
