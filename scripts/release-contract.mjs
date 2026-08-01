#!/usr/bin/env node

import {
  access,
  appendFile,
  readFile,
  stat,
  writeFile,
} from "node:fs/promises";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const VERSION_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const REPOSITORY_RE = /^[0-9A-Za-z_.-]+\/[0-9A-Za-z_.-]+$/;
const TAG_RE = /^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/**
 * Read and cross-check the version authorities from the exact checked-out
 * release commit. Keeping this in the release contract means a workflow can
 * derive every artifact name from the same metadata it validates, instead of
 * carrying a second hand-edited version constant in YAML.
 */
export function releaseMetadata(root = ROOT) {
  const packageVersion = JSON.parse(
    readFileSync(path.join(root, "package.json"), "utf8"),
  ).version;
  const tauriVersion = JSON.parse(
    readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8"),
  ).version;
  const cargoVersion = readFileSync(path.join(root, "src-tauri/Cargo.toml"), "utf8")
    .match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  const versions = [packageVersion, tauriVersion, cargoVersion];
  if (versions.some((version) => typeof version !== "string" || !VERSION_RE.test(version))) {
    throw new Error("package, Tauri, and Cargo versions must all be valid semver values");
  }
  if (new Set(versions).size !== 1) {
    throw new Error(
      `release version drift: package=${packageVersion}, tauri=${tauriVersion}, cargo=${cargoVersion}`,
    );
  }
  const version = packageVersion;
  const installer = `BackLog_${version}_x64-setup.exe`;
  return {
    version,
    tag: `v${version}`,
    installer,
    portable: `BackLog_${version}_x64-portable.zip`,
    signature: `${installer}.sig`,
    manifest: "latest.json",
  };
}

export function shouldStartRelease({
  ref,
  tagTarget,
  releaseState,
  releaseSha,
}) {
  if (tagTarget !== null && !/^[0-9a-f]{40}$/i.test(tagTarget)) {
    throw new TypeError("tagTarget must be null or a full commit SHA");
  }
  if (!["missing", "draft", "published"].includes(releaseState)) {
    throw new TypeError("releaseState must be missing, draft, or published");
  }
  if (!/^[0-9a-f]{40}$/i.test(releaseSha ?? "")) {
    throw new TypeError("releaseSha must be a full commit SHA");
  }
  if (tagTarget && tagTarget.toLowerCase() !== releaseSha.toLowerCase()) {
    throw new Error("the release tag points at a different commit");
  }
  return ref === "refs/heads/main" && releaseState !== "published";
}

export function releasePlan(privateKey) {
  const signed = typeof privateKey === "string" && privateKey.trim().length > 0;
  return {
    signed,
    prerelease: !signed,
    publishUpdater: signed,
  };
}

export function buildUpdaterManifest({
  version,
  repository,
  installerName,
  signature,
  pubDate,
  notes,
}) {
  if (!VERSION_RE.test(version)) {
    throw new Error(`invalid release version: ${JSON.stringify(version)}`);
  }
  if (!REPOSITORY_RE.test(repository)) {
    throw new Error(`invalid GitHub repository: ${JSON.stringify(repository)}`);
  }
  if (!installerName || path.basename(installerName) !== installerName) {
    throw new Error(`installer name must be a basename: ${JSON.stringify(installerName)}`);
  }
  const cleanSignature = String(signature ?? "").trim();
  if (!cleanSignature) {
    throw new Error("updater signature is empty");
  }
  if (Number.isNaN(Date.parse(pubDate))) {
    throw new Error(`invalid updater publication date: ${JSON.stringify(pubDate)}`);
  }

  return {
    version,
    notes: String(notes ?? "").trim(),
    pub_date: pubDate,
    platforms: {
      "windows-x86_64": {
        signature: cleanSignature,
        url:
          `https://github.com/${repository}/releases/download/` +
          `v${version}/${installerName}`,
      },
    },
  };
}

async function requireNonemptyFile(file, label) {
  let info;
  try {
    info = await stat(file);
  } catch {
    throw new Error(`${label} is missing: ${file}`);
  }
  if (!info.isFile() || info.size === 0) {
    throw new Error(`${label} is empty or not a file: ${file}`);
  }
}

async function fileExists(file) {
  try {
    await access(file);
    return true;
  } catch {
    return false;
  }
}

export async function verifyReleaseArtifacts({
  signed,
  installer,
  signature,
  manifest,
}) {
  await requireNonemptyFile(installer, "installer");

  if (!signed) {
    if (await fileExists(signature)) {
      throw new Error("unsigned release must not contain an updater signature");
    }
    if (await fileExists(manifest)) {
      throw new Error("unsigned release must not contain latest.json");
    }
    return;
  }

  await requireNonemptyFile(signature, "updater signature");
  await requireNonemptyFile(manifest, "latest.json");
  const signatureText = (await readFile(signature, "utf8")).trim();
  const parsed = JSON.parse(await readFile(manifest, "utf8"));
  const manifestSignature = parsed?.platforms?.["windows-x86_64"]?.signature;
  if (manifestSignature !== signatureText) {
    throw new Error("latest.json signature does not match the detached signature");
  }
  const updateUrl = parsed?.platforms?.["windows-x86_64"]?.url;
  if (!updateUrl || path.basename(new URL(updateUrl).pathname) !== path.basename(installer)) {
    throw new Error("latest.json does not point at this installer");
  }
}

function parseArgs(values) {
  const args = {};
  for (let index = 0; index < values.length; index += 2) {
    const key = values[index];
    const value = values[index + 1];
    if (!key?.startsWith("--") || value === undefined) {
      throw new Error(`expected --name value arguments, received ${values.join(" ")}`);
    }
    args[key.slice(2)] = value;
  }
  return args;
}

async function runCli() {
  const [command, ...values] = process.argv.slice(2);

  if (command === "gate") {
    const output = process.env.GITHUB_OUTPUT;
    const branch = process.env.RELEASE_BRANCH;
    const ref = branch ? `refs/heads/${branch}` : process.env.GITHUB_REF;
    const tag = process.env.TAG;
    if (!output) throw new Error("GITHUB_OUTPUT is required for release gate");
    if (!ref) throw new Error("GITHUB_REF is required for release gate");
    if (!TAG_RE.test(tag ?? "")) {
      throw new Error(`invalid release tag: ${JSON.stringify(tag)}`);
    }

    const releaseSha = process.env.RELEASE_SHA;
    if (!/^[0-9a-f]{40}$/i.test(releaseSha ?? "")) {
      throw new Error("RELEASE_SHA must be the full CI-tested commit SHA");
    }

    let tagTarget = null;
    if (ref === "refs/heads/main") {
      const lookup = spawnSync(
        "git",
        [
          "ls-remote",
          "--exit-code",
          "--tags",
          "origin",
          `refs/tags/${tag}`,
          `refs/tags/${tag}^{}`,
        ],
        { encoding: "utf8" },
      );
      if (lookup.error) throw lookup.error;
      if (lookup.status === 0) {
        const refs = lookup.stdout
          .trim()
          .split(/\r?\n/)
          .map((line) => line.trim().split(/\s+/, 2))
          .filter(([sha, name]) => /^[0-9a-f]{40}$/i.test(sha) && name);
        const resolved = refs.find(([, name]) => name.endsWith("^{}")) ?? refs[0];
        tagTarget = resolved?.[0] ?? null;
        if (!tagTarget) throw new Error(`could not resolve ${tag} to a commit`);
      } else if (lookup.status !== 2) {
        throw new Error(
          `could not check ${tag} on origin: ${lookup.stderr.trim() || `git exited ${lookup.status}`}`,
        );
      }
    }

    let releaseState = "missing";
    if (tagTarget) {
      const releaseLookup = spawnSync(
        "gh",
        ["release", "view", tag, "--json", "isDraft", "--jq", ".isDraft"],
        { encoding: "utf8" },
      );
      if (releaseLookup.error) throw releaseLookup.error;
      if (releaseLookup.status === 0) {
        releaseState = releaseLookup.stdout.trim() === "true" ? "draft" : "published";
      } else {
        const detail = `${releaseLookup.stdout}\n${releaseLookup.stderr}`.trim();
        if (
          releaseLookup.status !== 1 ||
          !/(release not found|not found|HTTP 404)/i.test(detail)
        ) {
          throw new Error(
            `could not inspect ${tag} release state: ${detail || `gh exited ${releaseLookup.status}`}`,
          );
        }
      }
    }

    const release = shouldStartRelease({
      ref,
      tagTarget,
      releaseState,
      releaseSha,
    });
    await appendFile(output, `release=${release}\n`, "utf8");
    if (release) {
      console.log(
        releaseState === "draft"
          ? `${tag} is an interrupted draft for this commit: publication will resume.`
          : `${tag} is not published: the ${tag} release build will start.`,
      );
    } else if (releaseState === "published") {
      console.log(`${tag} is already published: this workflow run is complete.`);
    } else {
      console.log(`Release skipped for ${ref}; ${tag} publishes only from main.`);
    }
    return;
  }

  if (command === "metadata") {
    const output = process.env.GITHUB_OUTPUT;
    if (!output) throw new Error("GITHUB_OUTPUT is required for release metadata");
    const metadata = releaseMetadata();
    await appendFile(
      output,
      Object.entries(metadata)
        .map(([key, value]) => `${key}=${value}`)
        .join("\n") + "\n",
      "utf8",
    );
    console.log(`Release metadata verified: ${metadata.version} (${metadata.tag}).`);
    return;
  }

  if (command === "mode") {
    const output = process.env.GITHUB_OUTPUT;
    if (!output) throw new Error("GITHUB_OUTPUT is required for release mode");
    const plan = releasePlan(process.env.TAURI_SIGNING_PRIVATE_KEY ?? "");
    await appendFile(
      output,
      `signed=${plan.signed}\nprerelease=${plan.prerelease}\n` +
        `publish_updater=${plan.publishUpdater}\n`,
      "utf8",
    );
    console.log(
      plan.signed
        ? "Updater key available: stable signed publication enabled."
        : "Updater key absent: unsigned prerelease selected.",
    );
    return;
  }

  const args = parseArgs(values);
  if (command === "manifest") {
    const signature = await readFile(args.signature, "utf8");
    const notes = args.notes ? await readFile(args.notes, "utf8") : "";
    const manifest = buildUpdaterManifest({
      version: args.version,
      repository: args.repository,
      installerName: path.basename(args.installer),
      signature,
      pubDate: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
      notes,
    });
    await writeFile(args.out, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
    console.log(`Wrote signed updater manifest: ${args.out}`);
    return;
  }

  if (command === "verify") {
    if (args.signed !== "true" && args.signed !== "false") {
      throw new Error("--signed must be true or false");
    }
    await verifyReleaseArtifacts({
      signed: args.signed === "true",
      installer: args.installer,
      signature: args.signature,
      manifest: args.manifest,
    });
    console.log(
      args.signed === "true"
        ? "Signed release contains installer, matching signature, and latest.json."
        : "Unsigned release contains the installer and portable ZIP; updater files are absent.",
    );
    return;
  }

  throw new Error("usage: release-contract.mjs <metadata|gate|mode|manifest|verify> [options]");
}

const isMain = process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  runCli().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}
