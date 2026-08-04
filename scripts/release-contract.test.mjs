import assert from "node:assert/strict";
import { mkdtemp, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  buildUpdaterManifest,
  ciGateStatus,
  releasePlan,
  shouldStartRelease,
  verifyReleaseArtifacts,
} from "./release-contract.mjs";

test("the release CI gate accepts only a successful main push for the exact SHA", () => {
  const sha = "a".repeat(40);
  assert.equal(
    ciGateStatus(
      [{
        headSha: sha,
        headBranch: "main",
        event: "push",
        status: "in_progress",
        conclusion: "",
        createdAt: "2026-08-01T08:00:00Z",
      }],
      sha,
    ),
    "pending",
  );
  assert.equal(
    ciGateStatus(
      [{
        headSha: sha,
        headBranch: "main",
        event: "push",
        status: "completed",
        conclusion: "success",
        createdAt: "2026-08-01T08:01:00Z",
      }],
      sha,
    ),
    "success",
  );
  assert.equal(
    ciGateStatus(
      [{
        headSha: sha,
        headBranch: "main",
        event: "pull_request",
        status: "completed",
        conclusion: "success",
        createdAt: "2026-08-01T08:02:00Z",
      }],
      sha,
    ),
    "pending",
  );
});

test("the release starts only for main when the version tag is absent", () => {
  assert.equal(
    shouldStartRelease({
      ref: "refs/heads/main",
      tagTarget: null,
      releaseState: "missing",
      releaseSha: "a".repeat(40),
    }),
    true,
  );
  assert.equal(
    shouldStartRelease({
      ref: "refs/heads/main",
      tagTarget: "a".repeat(40),
      releaseState: "published",
      releaseSha: "a".repeat(40),
    }),
    false,
  );
  assert.equal(
    shouldStartRelease({
      ref: "refs/heads/main",
      tagTarget: "b".repeat(40),
      releaseState: "published",
      releaseSha: "a".repeat(40),
    }),
    false,
  );
  assert.equal(
    shouldStartRelease({
      ref: "refs/heads/feature",
      tagTarget: null,
      releaseState: "missing",
      releaseSha: "a".repeat(40),
    }),
    false,
  );
});

test("a matching interrupted draft can be retried without moving its tag", () => {
  assert.equal(
    shouldStartRelease({
      ref: "refs/heads/main",
      tagTarget: "a".repeat(40),
      releaseState: "draft",
      releaseSha: "a".repeat(40),
    }),
    true,
  );
  assert.throws(
    () =>
      shouldStartRelease({
        ref: "refs/heads/main",
        tagTarget: "b".repeat(40),
        releaseState: "draft",
        releaseSha: "a".repeat(40),
      }),
    /points at a different commit/,
  );
});

test("an absent updater key selects an unsigned prerelease", () => {
  assert.deepEqual(releasePlan(""), {
    signed: false,
    prerelease: true,
    publishUpdater: false,
  });
  assert.deepEqual(releasePlan(" \r\n\t"), {
    signed: false,
    prerelease: true,
    publishUpdater: false,
  });
});

test("a present updater key selects a stable signed release", () => {
  assert.deepEqual(releasePlan("untrusted comment: encrypted secret key\npayload"), {
    signed: true,
    prerelease: false,
    publishUpdater: true,
  });
});

test("the updater manifest points at the signed installer for the exact tag", () => {
  assert.deepEqual(
    buildUpdaterManifest({
      version: "0.6.0",
      repository: "zgbrenner/backlog",
      installerName: "BackLog_0.6.0_x64-setup.exe",
      signature: "literal-signature",
      pubDate: "2026-07-30T12:00:00Z",
      notes: "BackLog 0.6.0",
    }),
    {
      version: "0.6.0",
      notes: "BackLog 0.6.0",
      pub_date: "2026-07-30T12:00:00Z",
      platforms: {
        "windows-x86_64": {
          signature: "literal-signature",
          url:
            "https://github.com/zgbrenner/backlog/releases/download/" +
            "v0.6.0/BackLog_0.6.0_x64-setup.exe",
        },
      },
    },
  );
});

test("a blank signature cannot produce updater metadata", () => {
  assert.throws(
    () =>
      buildUpdaterManifest({
        version: "0.6.0",
        repository: "zgbrenner/backlog",
        installerName: "BackLog_0.6.0_x64-setup.exe",
        signature: " \n",
        pubDate: "2026-07-30T12:00:00Z",
        notes: "",
      }),
    /signature is empty/,
  );
});

test("an unsigned artifact set is rejected if updater files exist", async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "backlog-release-"));
  const installer = path.join(dir, "BackLog_0.6.0_x64-setup.exe");
  const signature = `${installer}.sig`;
  const manifest = path.join(dir, "latest.json");
  await writeFile(installer, "installer");

  await assert.doesNotReject(() =>
    verifyReleaseArtifacts({ signed: false, installer, signature, manifest }),
  );

  await writeFile(manifest, "{}");
  await assert.rejects(
    () => verifyReleaseArtifacts({ signed: false, installer, signature, manifest }),
    /unsigned release must not contain latest\.json/,
  );
});

test("a signed artifact set requires a nonempty signature matching latest.json", async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "backlog-release-"));
  const installer = path.join(dir, "BackLog_0.6.0_x64-setup.exe");
  const signature = `${installer}.sig`;
  const manifest = path.join(dir, "latest.json");
  await writeFile(installer, "installer");
  await writeFile(signature, "literal-signature\n");
  await writeFile(
    manifest,
    JSON.stringify(
      buildUpdaterManifest({
        version: "0.6.0",
        repository: "zgbrenner/backlog",
        installerName: path.basename(installer),
        signature: "literal-signature",
        pubDate: "2026-07-30T12:00:00Z",
        notes: "",
      }),
    ),
  );

  await assert.doesNotReject(() =>
    verifyReleaseArtifacts({ signed: true, installer, signature, manifest }),
  );

  await writeFile(signature, "different-signature");
  await assert.rejects(
    () => verifyReleaseArtifacts({ signed: true, installer, signature, manifest }),
    /latest\.json signature does not match/,
  );
});
