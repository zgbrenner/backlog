// Asserts the app's three independently-edited version numbers agree.
//
// They drift silently and the symptom is remote: `tauri.conf.json`'s version is
// what lands in `latest.json`, `package.json`'s is what a release note quotes,
// and `Cargo.toml`'s is what `get_diagnostics` reports back from a user's
// machine. A release cut with two of the three bumped ships an updater manifest
// that either never fires or fires forever, and the only place the mismatch is
// visible is a support call.
//
//   node .github/scripts/check-versions.mjs
//
// `src-tauri/core/Cargo.toml` is deliberately NOT checked: backlog-core is a
// separately versioned library whose whole point is that it moves on its own.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");
const read = (rel) => readFileSync(path.join(ROOT, rel), "utf8");

const sources = [
  ["package.json", JSON.parse(read("package.json")).version],
  ["src-tauri/tauri.conf.json", JSON.parse(read("src-tauri/tauri.conf.json")).version],
  // Deliberately naive: the first `version = "..."` in the file, which is the
  // `[package]` one because `[workspace]` and `[dependencies]` follow it. A
  // TOML parser would be a dependency this check exists to avoid needing.
  ["src-tauri/Cargo.toml", read("src-tauri/Cargo.toml").match(/^version\s*=\s*"([^"]+)"/m)?.[1]],
];

const missing = sources.filter(([, version]) => !version);
if (missing.length) {
  console.error("could not read a version from:");
  for (const [file] of missing) console.error(`  - ${file}`);
  process.exit(2);
}

const distinct = new Set(sources.map(([, version]) => version));
if (distinct.size !== 1) {
  console.error("version drift between release inputs:");
  for (const [file, version] of sources) console.error(`  ${version.padEnd(12)} ${file}`);
  console.error("\nBump all three together (see RELEASING.md, 'Cutting a release', step 1).");
  process.exit(1);
}

console.log(`Versions agree: ${[...distinct][0]}`);
