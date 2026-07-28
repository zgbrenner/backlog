// Proves the coverage gate actually fails, and fails on the codes that matter.
//
// The gate passed for a whole review cycle while `DISMISSED` was undocumented,
// because its source list did not include the file that mints it — a green
// gate that structurally could not see the one code that was missing. So this
// test does not check that the gate passes on the real page (CI already does
// that); it checks that the gate *fails* when a code is taken out of the page,
// once per source of the vocabulary.
//
//   node .github/scripts/check-troubleshooting-coverage.test.mjs

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../..");
const GATE = path.join(HERE, "check-troubleshooting-coverage.mjs");
const PAGE = readFileSync(path.join(ROOT, "docs/TROUBLESHOOTING.md"), "utf8");
const scratch = mkdtempSync(path.join(tmpdir(), "backlog-coverage-"));

const run = (docPath) => spawnSync(process.execPath, [GATE, "--doc", docPath], { encoding: "utf8" });

let failures = 0;
const check = (label, condition, detail) => {
  if (condition) return;
  failures++;
  console.error(`FAIL  ${label}\n      ${detail}`);
};

// The real page must pass, or every negative case below proves nothing.
const clean = run(path.join(ROOT, "docs/TROUBLESHOOTING.md"));
check("the committed page passes", clean.status === 0, (clean.stderr || clean.stdout).trim());

// One code per emitter, so a source silently dropping out of the list is
// caught rather than showing up as a still-green gate.
const perSource = {
  "checker.rs": "DATE_NOT_IN_EVIDENCE",
  "pipeline.rs": "CRASH_LOOP",
  "lib.rs": "DISMISSED",
  "preflight.rs": "llama_server_probe_failed",
  "main.ts SOFT_FLAG_COPY": "SUBJECT_UNGROUNDED",
};

for (const [source, code] of Object.entries(perSource)) {
  const doctored = path.join(scratch, `without-${code}.md`);
  // Global, because a code appears in the table and often in the prose too;
  // leaving one occurrence behind would make the gate pass and the test lie.
  writeFileSync(doctored, PAGE.split(code).join("XX_REMOVED_XX"));
  const result = run(doctored);
  check(
    `${code} removed (${source}) is reported`,
    result.status === 1 && result.stderr.includes(code),
    `exit ${result.status}; stderr: ${result.stderr.trim() || "(empty)"}`
  );
}

if (failures) {
  console.error(`\n${failures} check(s) failed.`);
  process.exit(1);
}
console.log(`Coverage gate fails as intended on ${Object.keys(perSource).length} removed codes.`);
