// A document that tells the user to press a button must call it what the
// button calls itself.
//
// "Press Create folder if the app offers it" sat in docs/TROUBLESHOOTING.md
// through two reviews. The button says **Create this folder for me**. A
// non-technical user reading that row scans the readiness panel for "Create
// folder", does not find it, and concludes the app does not offer the fix the
// troubleshooting page just promised — on the one screen that exists because
// they are already stuck.
//
//   node .github/scripts/check-button-labels.mjs
//
// It failed a hand sweep because the sweep ran the wrong way round: it diffed
// `**bolded**` phrases against button labels, and the wrong name was buried
// inside a whole bolded sentence, so it classified as prose and matched
// nothing. This runs label-first instead — take each label the frontend can
// render and go looking for it in the docs.
//
// The near-miss test is a *skeleton* match, not a prefix match. Strip the
// function words from the label ("Create this folder for me" -> "create
// folder") and look for what is left. A prefix test would not have caught the
// original, because "create folder" is not a prefix of "create this folder for
// me" — it is an abbreviation of it, which is what people actually write.

import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const frontend = readFileSync(path.join(ROOT, "src/main.ts"), "utf8");

// Buttons are minted three ways in main.ts: inline in a template literal, via
// `el(\`<button>…\`)`, and by relabelling an existing button (the two-step
// confirm on the destructive actions). Miss any one and a label goes unchecked.
const labels = new Set();
for (const m of frontend.matchAll(/<button[^>]*>([^<>{}]+)<\/button>/g)) labels.add(m[1].trim());
for (const m of frontend.matchAll(/el\(`<button[^>]*>([^<`]+)<\/button>`\)/g)) labels.add(m[1].trim());
for (const m of frontend.matchAll(/button\.textContent\s*=\s*"([^"]+)"/g)) labels.add(m[1].trim());

if (labels.size < 20) {
  console.error(
    `extracted only ${labels.size} button labels; the extraction has stopped matching. ` +
      "Fix it rather than lowering this floor."
  );
  process.exit(2);
}

const norm = (s) =>
  s
    .toLowerCase()
    .replace(/[‘’]/g, "'")
    .replace(/[^a-z0-9' ]+/g, " ")
    .replace(/\s+/g, " ")
    .trim();

// Words that carry no identity, so dropping them is how a person shortens a
// label in prose. Keep this list small: every addition makes a near-miss
// harder to see.
const FUNCTION_WORDS = new Set([
  "a", "an", "the", "this", "that", "these", "those",
  "for", "me", "my", "to", "of", "in", "on", "it", "is", "and", "or",
]);
const skeleton = (label) =>
  norm(label)
    .split(" ")
    .filter((w) => !FUNCTION_WORDS.has(w))
    .join(" ");

const docs = [];
const walk = (dir) => {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full);
    else if (entry.name.endsWith(".md")) docs.push(full);
  }
};
walk(path.join(ROOT, "docs"));
walk(path.join(ROOT, "power-automate"));
docs.push(path.join(ROOT, "README.md"));

const problems = [];
for (const label of [...labels].sort()) {
  const full = norm(label);
  const bones = skeleton(label);
  // Nothing was stripped, or one content word is left: either way the
  // skeleton is not a distinctive abbreviation and matching it is noise.
  if (bones === full || bones.split(" ").length < 2) continue;
  for (const doc of docs) {
    const text = norm(readFileSync(doc, "utf8"));
    if (text.includes(full)) continue; // named correctly somewhere in this doc
    if (text.includes(bones)) {
      problems.push(
        `${path.relative(ROOT, doc)} says "${bones}" but the button is "${label}"`
      );
    }
  }
}

if (problems.length) {
  console.error("documents that name a button by a name it does not have:\n");
  for (const problem of problems) console.error(`  - ${problem}`);
  console.error(
    "\nUse the exact label. If the phrase genuinely means something else " +
      "(a Power Automate connector action, say), reword it so it cannot be " +
      "read as a BackLog button."
  );
  process.exit(1);
}

console.log(
  `All ${labels.size} button labels are named correctly across ${docs.length} documents.`
);
