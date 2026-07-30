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
// convertd ships as a bundle.resources directory tree (PyInstaller --onedir
// output), not a triple-suffixed externalBin exe, so it no longer appears as
// a flat "convertd-<triple>" entry in this listing -- it is a "convertd"
// subdirectory instead, checked separately below.
if (staged.length && !staged.includes("convertd")) {
  failures.push("no convertd/ directory staged; run scripts/dev-stubs.sh first");
}
let stubs = 0;
let real = 0;

// Checks one file for the marker-vs-PE contract and folds it into the
// running stub/real counts, exactly like the flat-file loop below.
function checkFile(label, file) {
  const bytes = readFileSync(file);
  if (bytes.length === 0) {
    failures.push(`${label} is zero bytes; dev-stubs.sh must write the marker instead`);
    return;
  }
  const head = bytes.subarray(0, MARKER.length).toString("latin1");
  const isPe = bytes[0] === 0x4d && bytes[1] === 0x5a;
  if (head === MARKER) {
    stubs += 1;
    // The marker is chosen so a stub also fails a plain PE-magic test; a stub
    // that somehow passed one would defeat every check downstream of it.
    if (isPe) {
      failures.push(`${label} carries the marker but starts with the MZ PE magic; a stub must fail a PE test`);
    }
  } else if (isPe) {
    real += 1;
  } else {
    failures.push(
      `${label} is neither a marked stub nor a PE image (starts ${JSON.stringify(head)}) — ` +
        `a truncated or half-written file would pass a stub check and fail at runtime`,
    );
  }
}

for (const name of staged) {
  const file = path.join(binDir, name);
  if (!statSync(file).isFile()) continue;
  checkFile(name, file);
}

// convertd/convertd.exe is checked the same way as the flat-file entries
// above. Everything else under convertd/_internal/ is deliberately not
// walked here: most of those files are legitimately not PE images (ONNX
// models, fonts, .dat/.json metadata PyInstaller drops there), so a blanket
// marker-or-PE check across that tree would fail on real, correct builds --
// scripts/verify-binaries.ps1 only asserts _internal/ is present and
// non-empty, for the same reason.
const convertdExe = path.join(binDir, "convertd", "convertd.exe");
try {
  if (statSync(convertdExe).isFile()) {
    checkFile("convertd/convertd.exe", convertdExe);
  } else {
    failures.push("convertd/convertd.exe is missing or not a file; run scripts/dev-stubs.sh first");
  }
} catch {
  failures.push("convertd/convertd.exe is missing; run scripts/dev-stubs.sh first");
}

if (failures.length) {
  console.error("dev-stub marker contract broken:");
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(
  `Dev-stub marker contract holds across 3 scripts and ${stubs + real} checked file(s): ` +
    `${stubs} marked stub(s), ${real} real binary(ies).`,
);
if (real) {
  console.log(
    "Real binaries are staged, so this run did not exercise the stub path. " +
      "scripts/verify-binaries.ps1 is what refuses stubs before a release build.",
  );
}
