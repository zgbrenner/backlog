// Proves the dev-stub marker contract holds, on a Linux runner, without
// PowerShell.
//
// The marker is the only thing standing between "a developer stubbed the
// sidecars to get tests green" and "the installer shipped with placeholders
// inside it". It is duplicated across three scripts because two of them are
// PowerShell and one is bash; a silent drift between them turns
// verify-binaries.ps1 into a no-op that still prints green, which is exactly
// the failure mode it exists to prevent.
//
//   node .github/scripts/check-stub-marker.mjs
//
// Run AFTER scripts/dev-stubs.sh, so the staged files are checked too.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const MARKER = "BACKLOG-DEV-STUB-DO-NOT-SHIP";
const failures = [];

// 1. All three scripts must name the same marker.
for (const rel of [
  "scripts/dev-stubs.sh",
  "scripts/dev-stubs.ps1",
  "scripts/verify-binaries.ps1",
]) {
  const source = readFileSync(path.join(ROOT, rel), "utf8");
  if (!source.includes(`"${MARKER}"`)) {
    failures.push(`${rel} does not define the marker ${MARKER}`);
  }
}

// 2. verify-binaries.ps1 must actually reject what dev-stubs.sh produces:
//    non-empty, marker-carrying, and not a PE image.
//
// Every staged file must be provably one thing or the other — a marked stub or
// a real PE image. Both states are legitimate and both occur in practice: a
// fresh checkout has stubs, and a release machine has the real llama.cpp and
// convertd builds, which `dev-stubs.sh` deliberately leaves alone. What must
// never exist is a third state: a file that is neither, i.e. a truncated
// download or a half-written build that would sail through a stub check and
// then fail at runtime.
//
// This distinction is the whole reason the gate needed fixing. An earlier
// version failed on any non-stub, with a comment conceding that "a real binary
// here is fine and expected on a release machine" — and since Actions never
// runs for this repository (docs/KNOWN_ISSUES.md item 11), the release machine
// is the *only* place this script ever executes. It could not pass anywhere it
// actually ran.
const binDir = path.join(ROOT, "src-tauri/binaries");
let staged;
try {
  staged = readdirSync(binDir);
} catch {
  failures.push("src-tauri/binaries is missing; run scripts/dev-stubs.sh first");
  staged = [];
}
if (staged.length && !staged.some((name) => name.startsWith("convertd-"))) {
  failures.push("no convertd-<triple> binary staged; run scripts/dev-stubs.sh first");
}
let stubs = 0;
let real = 0;
for (const name of staged) {
  const file = path.join(binDir, name);
  if (!statSync(file).isFile()) continue;
  const bytes = readFileSync(file);
  if (bytes.length === 0) {
    failures.push(`${name} is zero bytes; dev-stubs.sh must write the marker instead`);
    continue;
  }
  const head = bytes.subarray(0, MARKER.length).toString("latin1");
  const isPe = bytes[0] === 0x4d && bytes[1] === 0x5a;
  if (head === MARKER) {
    stubs += 1;
    // The marker is chosen so a stub also fails a plain PE-magic test; a stub
    // that somehow passed one would defeat every check downstream of it.
    if (isPe) {
      failures.push(`${name} carries the marker but starts with the MZ PE magic; a stub must fail a PE test`);
    }
  } else if (isPe) {
    real += 1;
  } else {
    failures.push(
      `${name} is neither a marked stub nor a PE image (starts ${JSON.stringify(head)}) — ` +
        `a truncated or half-written file would pass a stub check and fail at runtime`,
    );
  }
}

if (failures.length) {
  console.error("dev-stub marker contract broken:");
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(
  `Dev-stub marker contract holds across 3 scripts and ${staged.length} staged file(s): ` +
    `${stubs} marked stub(s), ${real} real binary(ies).`,
);
if (real) {
  console.log(
    "Real binaries are staged, so this run did not exercise the stub path. " +
      "scripts/verify-binaries.ps1 is what refuses stubs before a release build.",
  );
}
