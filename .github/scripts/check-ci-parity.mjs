#!/usr/bin/env node
// Fail if .github/workflows/ci.yml and scripts/ci-local.sh have drifted apart.
//
// Hosted CI is authoritative for pull requests, while ci-local.sh lets
// contributors run the same core gates before they push. Two definitions of
// the gates can drift, so this makes divergence a build failure instead of a
// surprise.
//
// It deliberately compares the two things that drift in practice — the set of
// jobs, and the set of gate scripts each one invokes — rather than trying to
// diff shell against YAML token by token, which would be brittle enough that
// people would start ignoring it.

import { readFileSync } from "node:fs";

const WORKFLOW = ".github/workflows/ci.yml";
const LOCAL = "scripts/ci-local.sh";

const workflow = readFileSync(WORKFLOW, "utf8");
const local = readFileSync(LOCAL, "utf8");

const problems = [];

// --- jobs -------------------------------------------------------------------
// ci.yml job keys are the only two-space-indented `key:` lines under `jobs:`.
const jobsBlock = workflow.slice(workflow.indexOf("\njobs:"));
const workflowJobs = new Set(
  [...jobsBlock.matchAll(/^ {2}([a-z][a-z0-9-]*):$/gm)].map((m) => m[1]),
);

// ci-local.sh names its job on every `step <job> "label" ...` call.
const localJobs = new Set(
  [...local.matchAll(/^step +([a-z][a-z0-9-]*) /gm)].map((m) => m[1]),
);

for (const job of workflowJobs) {
  if (!localJobs.has(job)) {
    problems.push(
      `job "${job}" runs in ${WORKFLOW} but has no \`step ${job} ...\` in ${LOCAL} — ` +
        `a contributor running the local gates would never execute it`,
    );
  }
}
for (const job of localJobs) {
  if (!workflowJobs.has(job)) {
    problems.push(
      `job "${job}" runs in ${LOCAL} but is not a job in ${WORKFLOW} — ` +
        `it would be skipped once Actions is enabled`,
    );
  }
}

// --- gate scripts -----------------------------------------------------------
// Every node gate under .github/scripts must be invoked by both, or one side
// is enforcing a rule the other does not.
//
// The character class includes `.` because gate names are dotted: a gate's own
// self-test is `<gate>.test.mjs`. Without the dot, `[a-z0-9-]+` stops at the
// first `.` and the whole filename matches nothing at all — so a `.test.mjs`
// gate present in only one of the two files was invisible here, and this check
// reported "parity holds" over precisely the drift it exists to catch.
const gateScripts = (text) =>
  new Set(
    [...text.matchAll(/\.github\/scripts\/([a-z0-9.-]+\.mjs)/g)].map((m) => m[1]),
  );

const workflowGates = gateScripts(workflow);
const localGates = gateScripts(local);

for (const gate of workflowGates) {
  if (!localGates.has(gate)) {
    problems.push(`${gate} is run by ${WORKFLOW} but not by ${LOCAL}`);
  }
}
for (const gate of localGates) {
  if (!workflowGates.has(gate)) {
    problems.push(`${gate} is run by ${LOCAL} but not by ${WORKFLOW}`);
  }
}

// --- anchor commands --------------------------------------------------------
// The load-bearing checks, by the substring that identifies each. Kept short
// and specific on purpose: this catches "someone dropped -D warnings from one
// of the two files", which is the realistic silent regression.
const ANCHORS = [
  ["cargo test", "-p backlog-core"],
  ["cargo clippy", "-D warnings"],
  ["cargo fmt", "--check"],
  ["workspace tests", "--workspace"],
  ["workspace resource staging", "bash scripts/dev-stubs.sh"],
  ["npm typecheck+build", "npm run check"],
  ["frontend dependency audit", "npm audit --audit-level=high"],
  ["UI harness", "harness:shots"],
  ["python tests", "pytest"],
  ["Power Automate contract", "validate_examples.py"],
  ["locked dependency resolution", "--locked"],
  // Redundant with the gate-script set above, on purpose: this is the one
  // entry that survives someone narrowing the filename pattern back to
  // `[a-z0-9-]+`, which is how the self-test came to run in only one file.
  ["coverage gate self-test", "coverage.test.mjs"],
];

for (const [label, needle] of ANCHORS) {
  const inWorkflow = workflow.includes(needle);
  const inLocal = local.includes(needle);
  if (inWorkflow !== inLocal) {
    problems.push(
      `${label}: "${needle}" appears in ${inWorkflow ? WORKFLOW : LOCAL} ` +
        `but not in ${inWorkflow ? LOCAL : WORKFLOW}`,
    );
  }
}

if (problems.length > 0) {
  console.error(`${WORKFLOW} and ${LOCAL} have drifted:\n`);
  for (const problem of problems) console.error(`  - ${problem}`);
  console.error(
    "\nBoth must describe the same gates so local checks predict hosted CI.",
  );
  process.exit(1);
}

console.log(
  `CI parity holds: ${workflowJobs.size} jobs and ${workflowGates.size} gate ` +
    `script(s) match between ${WORKFLOW} and ${LOCAL}.`,
);
