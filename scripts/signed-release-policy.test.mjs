import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { releasePlan } from "./release-contract.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function read(relativePath) {
  return readFileSync(path.join(root, relativePath), "utf8");
}

test("a missing updater private key fails closed", () => {
  assert.throws(() => releasePlan(""), /TAURI_SIGNING_PRIVATE_KEY is required/);
  assert.throws(() => releasePlan(" \r\n\t"), /TAURI_SIGNING_PRIVATE_KEY is required/);
});

test("the normal release workflow cannot publish an unsigned fallback", () => {
  const workflow = read(".github/workflows/release.yml");
  assert.doesNotMatch(workflow, /Build unsigned installer/);
  assert.doesNotMatch(workflow, /Publish unsigned prerelease/);
  assert.doesNotMatch(workflow, /tauri\.unsigned\.conf\.json/);
  assert.match(workflow, /TAURI_SIGNING_PRIVATE_KEY/);
  assert.match(workflow, /Verify updater signature against embedded public key/);
});

test("v0.8.0 has a one-time exact-tag signed repair workflow", () => {
  const relativePath = ".github/workflows/repair-v0.8.0.yml";
  assert.equal(existsSync(path.join(root, relativePath)), true, `${relativePath} must exist`);
  const workflow = read(relativePath);

  assert.match(workflow, /REPAIR_TAG:\s*v0\.8\.0/);
  assert.match(workflow, /REPAIR_SHA:\s*74e31fbd2b31ad99ceaf5390bb27fb197fc706a7/);
  assert.match(workflow, /ref:\s*\$\{\{ env\.REPAIR_TAG \}\}/);
  assert.match(workflow, /TAURI_SIGNING_PRIVATE_KEY:\s*\$\{\{ secrets\.TAURI_SIGNING_PRIVATE_KEY \}\}/);
  assert.match(workflow, /refusing to rotate or replace the updater key/i);
  assert.match(workflow, /assert-release-tag\.ps1/);
  assert.match(workflow, /verify_updater_signature/);
  assert.match(workflow, /gh release upload.*--clobber/s);
  assert.match(workflow, /gh release edit.*--prerelease=false.*--latest/s);
  assert.doesNotMatch(workflow, /git tag\s+-f|update-ref.*refs\/tags|gh release delete/);
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
