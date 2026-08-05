import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

import { releasePlan } from "./release-contract.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function read(relativePath) {
  return readFileSync(path.join(root, relativePath), "utf8");
}

function namedSteps(job) {
  return new Map(
    (Array.isArray(job?.steps) ? job.steps : [])
      .filter((step) => step?.name)
      .map((step) => [step.name, step]),
  );
}

test("a missing updater private key fails closed", () => {
  assert.throws(() => releasePlan(""), /TAURI_SIGNING_PRIVATE_KEY is required/);
  assert.throws(() => releasePlan(" \r\n\t"), /TAURI_SIGNING_PRIVATE_KEY is required/);
});

test("the normal release workflow is structurally signed-only", () => {
  const source = read(".github/workflows/release.yml");
  const workflow = parse(source);
  const job = workflow?.jobs?.release;
  assert.ok(job, "release job must exist");
  assert.equal(job["runs-on"], "windows-2022");
  assert.equal(job?.permissions?.contents, "write");

  const steps = Array.isArray(job.steps) ? job.steps : [];
  const byName = namedSteps(job);
  const mode = byName.get("Select guarded release mode");
  const signedBuild = byName.get("Build signed installer");
  const portable = byName.get("Build installer-free portable ZIP");
  const signatureCheck = byName.get(
    "Verify updater signature against embedded public key",
  );
  const publish = byName.get("Publish signed stable release");

  for (const [label, step] of [
    ["release mode", mode],
    ["signed installer", signedBuild],
    ["portable ZIP", portable],
    ["signature verification", signatureCheck],
    ["stable publication", publish],
  ]) {
    assert.ok(step, `${label} step must exist`);
  }

  assert.equal(
    mode.env.TAURI_SIGNING_PRIVATE_KEY,
    "${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}",
  );
  assert.equal(
    signedBuild.env.TAURI_SIGNING_PRIVATE_KEY,
    "${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}",
  );
  assert.equal(
    signedBuild.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD,
    "${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}",
  );
  assert.ok(steps.indexOf(portable) > steps.indexOf(signedBuild));
  assert.ok(steps.indexOf(signatureCheck) > steps.indexOf(portable));
  assert.ok(steps.indexOf(publish) > steps.indexOf(signatureCheck));

  assert.doesNotMatch(source, /Build unsigned installer/);
  assert.doesNotMatch(source, /Publish unsigned prerelease/);
  assert.doesNotMatch(source, /tauri\.unsigned\.conf\.json/);
  assert.match(String(publish.run), /\$env:PORTABLE/);
  assert.match(String(publish.run), /\$env:SIGNATURE/);
  assert.match(String(publish.run), /\$env:MANIFEST/);
  assert.match(String(publish.run), /Compare-Object/);
  assert.match(String(publish.run), /--latest/);
});

test("the retired v0.8.0 repair workflow stays retired", () => {
  // The repair workflow rebuilt the immutable v0.8.0 tag and verified its
  // updater signature against the public key EMBEDDED IN THAT TAG'S BUILD
  // (F6BB6D5A6C3954C6). The private half of that key was lost and the
  // updater key was rotated for 0.8.1 (199849F4EB9588F7), so the repair can
  // never verify again -- it would only burn a Windows runner on every main
  // push and end red. Existing v0.8.0 installations upgrade by one manual
  // download, as CHANGELOG.md's 0.8.1 Security note records.
  const relativePath = ".github/workflows/repair-v0.8.0.yml";
  assert.equal(
    existsSync(path.join(root, relativePath)),
    false,
    `${relativePath} is retired; a repair against a lost key cannot succeed`,
  );
});

test("the release checklist requires all four signed downloadable assets", () => {
  const checklist = read("docs/RELEASE_CHECKLIST.md");
  const publication = checklist.slice(checklist.indexOf("## Publication guard"));
  for (const asset of [
    "BackLog_0.8.0_x64-setup.exe",
    "BackLog_0.8.0_x64-portable.zip",
    "BackLog_0.8.0_x64-setup.exe.sig",
    "latest.json",
  ]) {
    assert.match(publication, new RegExp(asset.replaceAll(".", "\\.")));
  }
});
