// Proves the coverage gate actually fails, and fails once per source of its
// vocabulary.
//
// The gate passed for a whole review cycle while `DISMISSED` was undocumented,
// because its source list did not include the file that mints it — a green
// gate that structurally could not see the one code that was missing. So this
// test does not check that the gate passes on the real page (CI already does
// that); it checks that the gate *fails* when a code is taken out of the page.
//
// The first version of this test only asserted `exit 1` and that the code was
// named. That was not enough: the gate unions two passes, so every code has at
// least two providers, and both `["lib.rs", ...]` and the entire second pass
// could be deleted with this test still green — it was protecting nothing.
// Hence `--source`: each case below runs the gate against exactly one provider,
// so removing that provider turns the case red (exit 2, "no source labelled").
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

const run = (...args) => spawnSync(process.execPath, [GATE, ...args], { encoding: "utf8" });

let failures = 0;
const check = (label, condition, detail) => {
  if (condition) return;
  failures++;
  console.error(`FAIL  ${label}\n      ${detail}`);
};

/** A copy of the page with every occurrence of `code` gone. Global, because a
 *  code appears in the table and often in the prose too; leaving one behind
 *  would make the gate pass and this test lie. */
const pageWithout = (code) => {
  const doctored = path.join(scratch, `without-${code}.md`);
  writeFileSync(doctored, PAGE.split(code).join("XX_REMOVED_XX"));
  return doctored;
};

// The real page must pass, or every negative case below proves nothing.
const clean = run("--doc", path.join(ROOT, "docs/TROUBLESHOOTING.md"));
check("the committed page passes", clean.status === 0, (clean.stderr || clean.stdout).trim());

// One code per source, driven through `--source` so the case depends on that
// source still being in the list and still yielding codes. Under the default
// union run the same code would be attributed to whichever source reached it
// first, which is why the attribution assertion below needs the flag.
const perSource = {
  "checker.rs": "DATE_NOT_IN_EVIDENCE",
  "pipeline.rs": "CRASH_LOOP",
  "routing.rs": "CORRUPT",
  "lib.rs": "DISMISSED",
  "preflight.rs": "llama_server_probe_failed",
  "main.ts REASON_COPY": "TOO_LONG",
  "main.ts SOFT_FLAG_COPY": "SUBJECT_UNGROUNDED",
};

for (const [source, code] of Object.entries(perSource)) {
  const result = run("--source", source, "--doc", pageWithout(code));
  const detail = `exit ${result.status}; stderr: ${result.stderr.trim() || "(empty)"}`;
  // Exit 1 is "a documented code went missing". Exit 2 is "the extraction
  // broke" — which is what a deleted source looks like, and must not be
  // mistaken for a successful negative case.
  check(`${source} still supplies ${code}`, result.status === 1, detail);
  // Naming the reporting source is the point: without it, a case passes on
  // any other source that happens to supply the same code.
  check(`${code} is reported as coming from ${source}`, result.stderr.includes(`(${source})`), detail);
}

// The default invocation — the one ci.yml and ci-local.sh actually run — must
// fail too. `--source` narrowing is a test affordance, not the shipped gate.
const union = run("--doc", pageWithout("SLM_FAIL"));
check(
  "the unnarrowed gate fails on a removed code",
  union.status === 1 && union.stderr.includes("SLM_FAIL"),
  `exit ${union.status}; stderr: ${union.stderr.trim() || "(empty)"}`
);

// A label that no longer exists must be exit 2, not a silent pass on an empty
// vocabulary. This is the assertion that makes deleting a source detectable.
const gone = run("--source", "checker.rs.deleted", "--doc", path.join(ROOT, "docs/TROUBLESHOOTING.md"));
check(
  "an unknown source is a hard error",
  gone.status === 2 && gone.stderr.includes("no source labelled"),
  `exit ${gone.status}; stderr: ${gone.stderr.trim() || "(empty)"}`
);

if (failures) {
  console.error(`\n${failures} check(s) failed.`);
  process.exit(1);
}
console.log(
  `Coverage gate fails as intended: ${Object.keys(perSource).length} sources checked in ` +
    "isolation, plus the unnarrowed run and the unknown-source guard."
);
