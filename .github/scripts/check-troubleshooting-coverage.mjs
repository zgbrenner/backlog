// Every code the app can put in front of a human must be explained somewhere
// a human can read.
//
// Failure in this product is not an edge case — it is the designed workflow.
// A scan that came out badly, a document with no date on it, a packet of five
// letters in one PDF: all of those are expected, and every one of them
// terminates at a person reading a code in the review pane or the readiness
// list. A code with no entry in docs/TROUBLESHOOTING.md is a dead end for
// someone who must never open a terminal, and the way that happens is nobody
// noticing when a new reason string lands.
//
//   node .github/scripts/check-troubleshooting-coverage.mjs
//
// Extraction is deliberately over-broad — every SHOUTY_CASE and snake_case
// string literal in the files that produce user-visible codes — with an
// explicit list of the few that are not codes. Over-collecting costs a
// documentation row; under-collecting costs a support call nobody can answer.
//
// Two independent passes, because either one alone has a blind spot:
//
//   1. the Rust emitters, which is where a code is minted; and
//   2. `src/main.ts`'s `REASON_COPY` / `SOFT_FLAG_COPY` keys, which is the set
//      of codes the app has written a sentence for.
//
// Pass 1 alone missed `DISMISSED` for a whole review cycle: it is minted in
// `lib.rs` (`dismissed_manifest`), not in the pipeline, so a source list built
// around the pipeline could not see it — while the review pane rendered "You
// set this one aside" for it and Flow 2 wrote it into the NeedsReview row.
// Pass 2 catches exactly that class: if a human reads it in the app, it is in
// the vocabulary regardless of which file produced it.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const read = (rel) => readFileSync(path.join(ROOT, rel), "utf8");

/** Source before `#[cfg(test)]`. Test fixtures invent strings that look like
 *  codes ("SUBJECT: Reçu…") and are not part of the vocabulary. */
const withoutTests = (source) => source.split(/^#\[cfg\(test\)\]/m)[0];

// Not codes. Each is here for a stated reason; if a new literal shows up in
// this check, document it rather than adding it to this list by reflex.
const NOT_CODES = new Set([
  "DATE", // harvest span label (filter.rs/pipeline.rs), never shown to a user
  "snake_case", // serde rename attribute
  // RuntimeProblem.field values — which settings row a problem attaches to,
  // not a code a user is ever told to quote.
  "cache_dir",
  "outbox_dir",
  "processing_dir",
  "quarantine_dir",
  "install_dir",
  "llama_port",
  "llama_server",
  "CARGO_PKG_VERSION", // env!() lookup in get_diagnostics, not a code
]);

// SHOUTY_CASE flag reasons, checker codes and soft flags; the trailing `:...`
// detail is stripped because the vocabulary is keyed on the prefix.
const SHOUTY = /"([A-Z][A-Z0-9_]{3,})(?::[^"]*)?"/g;
// snake_case preflight problem codes.
const SNAKE = /"([a-z][a-z0-9]*(?:_[a-z0-9]+)+)"/g;

const sources = [
  ["checker.rs", "src-tauri/core/src/checker.rs", SHOUTY],
  ["pipeline.rs", "src-tauri/src/pipeline.rs", SHOUTY],
  ["routing.rs", "src-tauri/src/routing.rs", SHOUTY],
  // `DISMISSED:` is minted here and nowhere else — see the header note.
  ["lib.rs", "src-tauri/src/lib.rs", SHOUTY],
  ["preflight.rs", "src-tauri/src/preflight.rs", SNAKE],
];

const vocabulary = new Map(); // code -> file it comes from
for (const [label, rel, pattern] of sources) {
  const source = withoutTests(read(rel));
  for (const match of source.matchAll(pattern)) {
    const code = match[1];
    if (!NOT_CODES.has(code)) vocabulary.set(code, label);
  }
}

/** Top-level keys of a `const <name>: ... = { ... }` object literal in TS.
 *  Brace-matched rather than regexed to the closing brace, because the values
 *  are objects and a lazy `[^}]*` would stop at the first nested one. */
function objectKeys(source, name) {
  const declaration = source.indexOf(`const ${name}`);
  if (declaration < 0) return [];
  const open = source.indexOf("{", source.indexOf("=", declaration));
  let depth = 0;
  let close = open;
  for (; close < source.length; close++) {
    if (source[close] === "{") depth++;
    else if (source[close] === "}" && --depth === 0) break;
  }
  return [...source.slice(open, close).matchAll(/^ {2}([A-Z][A-Z0-9_]{2,}):/gm)].map((m) => m[1]);
}

// Pass 2: the codes the app has already written a user-facing sentence for.
// These are what a person actually reads, so parity with the doc is the point.
const frontend = read("src/main.ts");
const copyKeys = [
  ["main.ts REASON_COPY", objectKeys(frontend, "REASON_COPY")],
  ["main.ts SOFT_FLAG_COPY", objectKeys(frontend, "SOFT_FLAG_COPY")],
];
for (const [label, keys] of copyKeys) {
  if (keys.length < 10) {
    console.error(
      `extracted only ${keys.length} keys from ${label}; the extraction has stopped matching. ` +
        "Fix it rather than lowering this floor."
    );
    process.exit(2);
  }
  for (const code of keys) if (!vocabulary.has(code)) vocabulary.set(code, label);
}

// A floor, not a target: if the regexes stop matching because a file changed
// shape, this check must fail rather than quietly pass on nothing.
if (vocabulary.size < 35) {
  console.error(
    `extracted only ${vocabulary.size} codes; the regexes have stopped matching. ` +
      "Fix the extraction rather than lowering this floor."
  );
  process.exit(2);
}

const doc = read("docs/TROUBLESHOOTING.md");
const missing = [...vocabulary].filter(([code]) => !doc.includes(code));
if (missing.length) {
  console.error("codes with no entry in docs/TROUBLESHOOTING.md:");
  for (const [code, label] of missing) console.error(`  - ${code}   (${label})`);
  console.error(
    "\nEvery one of these can appear in front of a non-technical user. " +
      "Add a row saying what it means and what to do next."
  );
  process.exit(1);
}

console.log(`All ${vocabulary.size} user-visible codes are documented in docs/TROUBLESHOOTING.md.`);
