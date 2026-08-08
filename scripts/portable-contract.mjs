#!/usr/bin/env node

import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  access,
  lstat,
  mkdir,
  open,
  readdir,
  readFile,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const VERSION_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;
const SHA256_RE = /^[0-9a-f]{64}$/i;
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

export const PORTABLE_MANIFEST_NAME = "portable-manifest.json";
export const PORTABLE_STUB_MARKER = "BACKLOG-DEV-STUB-DO-NOT-SHIP";
export const PORTABLE_WEBVIEW2_DIRECTORY = "webview2-fixed";
export const PORTABLE_WEBVIEW2_VERSION = "151.0.4129.59";
export const PORTABLE_WEBVIEW2_CAB_URL =
  "https://msedge.sf.dl.delivery.mp.microsoft.com/filestreamingservice/files/3cb717d2-b86d-4160-a13e-f3860141dc7f/Microsoft.WebView2.FixedVersionRuntime.151.0.4129.59.x64.cab";
export const PORTABLE_WEBVIEW2_CAB_SHA256 =
  "056858a027a7bf29893b6013c0eb0c6ea7e29755a20c9d043be469d9d78657dc";
export const PORTABLE_WEBVIEW2_CAB_SIZE = 304114944;
export const PORTABLE_WEBVIEW2_FILE_COUNT = 256;
export const PORTABLE_REQUIRED_PATHS = Object.freeze([
  "BackLog.exe",
  "BackLog-Portable.cmd",
  "llama-server-x86_64-pc-windows-msvc.exe",
  "convertd/convertd.exe",
  "resources/name.gbnf",
  "resources/models/Qwen3-0.6B-Q8_0.gguf",
  "resources/models/semantic/all-MiniLM-L6-v2/model.onnx",
  "resources/models/semantic/all-MiniLM-L6-v2/vocab.txt",
  "README-PORTABLE.md",
  `${PORTABLE_WEBVIEW2_DIRECTORY}/msedgewebview2.exe`,
  `${PORTABLE_WEBVIEW2_DIRECTORY}/runtime-manifest.json`,
]);

// These are also present in models/models.lock.json and scripts/stage-release-inputs.ps1.
// Keeping them here makes the portable artifact independently fail closed if a
// caller bypasses the earlier release-input gate.
export const PORTABLE_REQUIRED_HASHES = Object.freeze({
  "llama-server-x86_64-pc-windows-msvc.exe":
    "78af9cfb34f346b0de1e4f9c1577061cb3d55e8be55c8d540fde878e56bd0fe2",
  "resources/models/Qwen3-0.6B-Q8_0.gguf":
    "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
  "resources/models/semantic/all-MiniLM-L6-v2/model.onnx":
    "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1",
  "resources/models/semantic/all-MiniLM-L6-v2/vocab.txt":
    "07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3",
});

function posixPath(value) {
  return value.replaceAll("\\", "/");
}

function assertSafeRelativePath(value, label = "portable path") {
  if (
    typeof value !== "string" ||
    !value ||
    value.includes("\0") ||
    value.startsWith("/") ||
    /^[A-Za-z]:[\\/]/.test(value)
  ) {
    throw new Error(`${label} must be a relative path: ${JSON.stringify(value)}`);
  }
  const normalized = posixPath(value);
  if (normalized.split("/").some((part) => part === ".." || part === "")) {
    throw new Error(`${label} contains an unsafe path: ${JSON.stringify(value)}`);
  }
  if (normalized !== value) {
    throw new Error(`${label} must use forward slashes: ${JSON.stringify(value)}`);
  }
  return normalized;
}

function assertVersion(version) {
  if (typeof version !== "string" || !VERSION_RE.test(version)) {
    throw new Error(`invalid portable release version: ${JSON.stringify(version)}`);
  }
}

async function listFiles(root, relative = "") {
  const directory = path.join(root, relative);
  const entries = await readdir(directory, { withFileTypes: true });
  const files = [];
  for (const entry of entries) {
    const child = relative ? path.join(relative, entry.name) : entry.name;
    if (entry.isSymbolicLink()) {
      throw new Error(`portable artifact must not contain symlinks: ${child}`);
    }
    if (entry.isDirectory()) {
      files.push(...(await listFiles(root, child)));
    } else if (entry.isFile()) {
      files.push(posixPath(child));
    } else {
      throw new Error(`portable artifact contains an unsupported filesystem entry: ${child}`);
    }
  }
  return files.sort();
}

function hashFile(file) {
  return new Promise((resolve, reject) => {
    const hash = createHash("sha256");
    const stream = createReadStream(file);
    stream.on("error", reject);
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("end", () => resolve(hash.digest("hex")));
  });
}

async function readPrefix(file, length) {
  const handle = await open(file, "r");
  try {
    const buffer = Buffer.alloc(length);
    const { bytesRead } = await handle.read(buffer, 0, length, 0);
    return buffer.subarray(0, bytesRead);
  } finally {
    await handle.close();
  }
}

async function isPortableExecutable(file) {
  const header = await readPrefix(file, 64);
  if (header.length < 64 || header[0] !== 0x4d || header[1] !== 0x5a) return false;
  const peOffset = header.readInt32LE(0x3c);
  if (peOffset <= 0) return false;
  const signature = await readPrefixAt(file, peOffset, 4);
  return signature.length === 4 && signature.toString("ascii") === "PE\0\0";
}

async function readPrefixAt(file, offset, length) {
  const handle = await open(file, "r");
  try {
    const buffer = Buffer.alloc(length);
    const { bytesRead } = await handle.read(buffer, 0, length, offset);
    return buffer.subarray(0, bytesRead);
  } finally {
    await handle.close();
  }
}

async function carriesStubMarker(file) {
  const prefix = await readPrefix(file, 4096);
  return prefix.includes(Buffer.from(PORTABLE_STUB_MARKER, "ascii"));
}

async function readManifest(root) {
  const file = path.join(root, PORTABLE_MANIFEST_NAME);
  try {
    await access(file);
    return JSON.parse(await readFile(file, "utf8"));
  } catch (error) {
    throw new Error(`portable manifest is missing or invalid: ${error.message}`);
  }
}

function manifestRecords(manifest) {
  if (!Array.isArray(manifest?.files) || manifest.files.length === 0) {
    throw new Error("portable manifest must list at least one payload file");
  }
  const records = new Map();
  for (const record of manifest.files) {
    const relative = assertSafeRelativePath(record?.path, "portable manifest path");
    if (relative === PORTABLE_MANIFEST_NAME) {
      throw new Error("portable manifest must not list itself");
    }
    if (records.has(relative)) {
      throw new Error(`portable manifest lists ${relative} more than once`);
    }
    if (!Number.isSafeInteger(record?.size) || record.size < 0) {
      throw new Error(`portable manifest has an invalid size for ${relative}`);
    }
    if (!SHA256_RE.test(record?.sha256 ?? "")) {
      throw new Error(`portable manifest has an invalid SHA-256 for ${relative}`);
    }
    records.set(relative, { path: relative, size: record.size, sha256: record.sha256.toLowerCase() });
  }
  return records;
}

async function validateFixedWebView2(
  root,
  manifest,
  expectedFileCount = PORTABLE_WEBVIEW2_FILE_COUNT,
) {
  if (manifest.webview2 !== "fixed-runtime") {
    throw new Error("portable artifact must declare its bundled fixed WebView2 runtime");
  }
  const metadata = manifest.webview2_runtime;
  if (
    metadata?.version !== PORTABLE_WEBVIEW2_VERSION ||
    metadata?.architecture !== "x64" ||
    metadata?.directory !== PORTABLE_WEBVIEW2_DIRECTORY ||
    metadata?.cab_url !== PORTABLE_WEBVIEW2_CAB_URL ||
    metadata?.cab_sha256 !== PORTABLE_WEBVIEW2_CAB_SHA256 ||
    metadata?.cab_size !== PORTABLE_WEBVIEW2_CAB_SIZE ||
    metadata?.file_count !== expectedFileCount
  ) {
    throw new Error("portable manifest has an unexpected fixed WebView2 runtime pin");
  }

  const runtimeManifestPath = path.join(
    root,
    PORTABLE_WEBVIEW2_DIRECTORY,
    "runtime-manifest.json",
  );
  let runtimeManifest;
  try {
    runtimeManifest = JSON.parse(await readFile(runtimeManifestPath, "utf8"));
  } catch (error) {
    throw new Error(`fixed WebView2 runtime manifest is missing or invalid: ${error.message}`);
  }
  if (
    runtimeManifest?.schema !== 1 ||
    runtimeManifest?.product !== "BackLog" ||
    runtimeManifest?.version !== PORTABLE_WEBVIEW2_VERSION ||
    runtimeManifest?.architecture !== "x64" ||
    runtimeManifest?.cab_url !== PORTABLE_WEBVIEW2_CAB_URL ||
    runtimeManifest?.cab_sha256 !== PORTABLE_WEBVIEW2_CAB_SHA256 ||
    runtimeManifest?.cab_size !== PORTABLE_WEBVIEW2_CAB_SIZE ||
    runtimeManifest?.file_count !== expectedFileCount ||
    !Array.isArray(runtimeManifest.files) ||
    runtimeManifest.files.length !== expectedFileCount
  ) {
    throw new Error("fixed WebView2 runtime manifest does not match its release pin");
  }

  const runtimeFiles = (await listFiles(path.join(root, PORTABLE_WEBVIEW2_DIRECTORY)))
    .filter((file) => file !== "runtime-manifest.json");
  const listedRuntimeFiles = runtimeManifest.files.map((file) =>
    assertSafeRelativePath(file, "fixed WebView2 runtime path"),
  );
  if (new Set(listedRuntimeFiles).size !== listedRuntimeFiles.length) {
    throw new Error("fixed WebView2 runtime manifest lists a file more than once");
  }
  if (runtimeFiles.length !== expectedFileCount) {
    throw new Error(
      `fixed WebView2 runtime contains ${runtimeFiles.length} files; expected ${expectedFileCount}`,
    );
  }
  const listedRuntimeSet = new Set(listedRuntimeFiles);
  if (
    runtimeFiles.length !== listedRuntimeFiles.length ||
    runtimeFiles.some((file) => !listedRuntimeSet.has(file))
  ) {
    throw new Error("fixed WebView2 runtime payload does not match runtime-manifest.json");
  }
  for (const required of [
    "msedgewebview2.exe",
    "msedge.dll",
    `${PORTABLE_WEBVIEW2_VERSION}.manifest`,
  ]) {
    if (!listedRuntimeFiles.includes(required)) {
      throw new Error(`fixed WebView2 runtime is missing ${required}`);
    }
  }

  const launcher = await readFile(path.join(root, "BackLog-Portable.cmd"), "utf8");
  if (
    !launcher.includes("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") ||
    !launcher.includes(PORTABLE_WEBVIEW2_DIRECTORY) ||
    !launcher.includes("BackLog.exe") ||
    !launcher.includes("BACKLOG_ROOT:~0,2") ||
    !launcher.includes("S-1-15-2-2") ||
    !launcher.includes("S-1-15-2-1") ||
    !launcher.includes("icacls")
  ) {
    throw new Error(
      "BackLog-Portable.cmd must select the bundled runtime and handle fixed-runtime Windows permissions",
    );
  }
}

export async function writePortableManifest(
  root,
  {
    version,
    artifact = `BackLog_${version}_x64-portable.zip`,
    webview2FileCount = PORTABLE_WEBVIEW2_FILE_COUNT,
  } = {},
) {
  assertVersion(version);
  assertSafeRelativePath(artifact, "portable artifact name");
  if (!Number.isSafeInteger(webview2FileCount) || webview2FileCount <= 0) {
    throw new Error("webview2FileCount must be a positive safe integer");
  }
  const files = (await listFiles(root)).filter((file) => file !== PORTABLE_MANIFEST_NAME);
  const records = [];
  for (const relative of files) {
    const info = await lstat(path.join(root, relative));
    records.push({
      path: relative,
      size: info.size,
      sha256: await hashFile(path.join(root, relative)),
    });
  }
  const manifest = {
    schema: 1,
    product: "BackLog",
    version,
    artifact,
    platform: "windows-x64",
    webview2: "fixed-runtime",
    webview2_runtime: {
      version: PORTABLE_WEBVIEW2_VERSION,
      architecture: "x64",
      directory: PORTABLE_WEBVIEW2_DIRECTORY,
      cab_url: PORTABLE_WEBVIEW2_CAB_URL,
      cab_sha256: PORTABLE_WEBVIEW2_CAB_SHA256,
      cab_size: PORTABLE_WEBVIEW2_CAB_SIZE,
      file_count: webview2FileCount,
    },
    files: records,
  };
  await writeFile(
    path.join(root, PORTABLE_MANIFEST_NAME),
    `${JSON.stringify(manifest, null, 2)}\n`,
    "utf8",
  );
  return manifest;
}

export async function validatePortableTree(
  root,
  {
    expectedVersion,
    requiredHashes = PORTABLE_REQUIRED_HASHES,
    expectedWebView2FileCount = PORTABLE_WEBVIEW2_FILE_COUNT,
  } = {},
) {
  const manifest = await readManifest(root);
  if (manifest?.schema !== 1 || manifest?.product !== "BackLog") {
    throw new Error("portable manifest schema or product is unsupported");
  }
  assertVersion(manifest.version);
  if (expectedVersion !== undefined && manifest.version !== expectedVersion) {
    throw new Error(
      `portable manifest version ${manifest.version} does not match ${expectedVersion}`,
    );
  }
  const expectedArtifact = `BackLog_${manifest.version}_x64-portable.zip`;
  if (manifest.artifact !== expectedArtifact) {
    throw new Error(`portable manifest artifact must be ${expectedArtifact}`);
  }
  if (manifest.platform !== "windows-x64") {
    throw new Error("portable artifact must target windows-x64");
  }
  await validateFixedWebView2(root, manifest, expectedWebView2FileCount);

  const records = manifestRecords(manifest);
  const actualFiles = (await listFiles(root)).filter((file) => file !== PORTABLE_MANIFEST_NAME);
  const actualSet = new Set(actualFiles);
  for (const relative of records.keys()) {
    if (!actualSet.has(relative)) throw new Error(`portable payload is missing ${relative}`);
  }
  for (const relative of actualFiles) {
    if (!records.has(relative)) throw new Error(`portable payload has an unlisted file: ${relative}`);
  }

  for (const record of records.values()) {
    const file = path.join(root, record.path);
    const info = await lstat(file);
    if (info.size !== record.size) {
      throw new Error(`${record.path} size changed after manifest generation`);
    }
    const actualHash = await hashFile(file);
    if (actualHash !== record.sha256) {
      throw new Error(`${record.path} SHA-256 does not match portable-manifest.json`);
    }
    if (
      /\.(?:exe|dll)$/i.test(record.path) &&
      !(await isPortableExecutable(file))
    ) {
      throw new Error(`${record.path} is not a valid Windows PE image`);
    }
    if (
      /\.(?:exe|dll|gguf|onnx)$/i.test(record.path) &&
      (await carriesStubMarker(file))
    ) {
      throw new Error(`${record.path} carries ${PORTABLE_STUB_MARKER}`);
    }
  }

  for (const required of PORTABLE_REQUIRED_PATHS) {
    if (!records.has(required)) throw new Error(`portable payload is missing ${required}`);
  }
  const runtimeDlls = actualFiles.filter(
    (file) => /^[^/]+\.dll$/i.test(file) && file.toLowerCase() !== "_placeholder.dll",
  );
  if (runtimeDlls.length === 0) {
    throw new Error("portable payload has no app-local runtime DLL");
  }
  // The ZIP carries exactly one GGUF: the 0.6B in PORTABLE_REQUIRED_HASHES.
  // The optional 1.7B escalation model is downloaded in-app precisely because
  // carrying it puts this artifact over GitHub's 2 GiB per-release-asset limit,
  // so finding it here means the tree was staged from a developer's app-data
  // models folder. Caught here rather than at upload time, where the symptom is
  // a failed release rather than a named file.
  //
  // The 4B is checked too even though nothing ships or downloads it any more:
  // `config.rs` still catalogues its shape so a hand-configured 4B is sized
  // correctly, which means a developer can still have one sitting in app-data,
  // which is exactly the staging accident this guard exists to catch.
  if (actualFiles.some((file) => /(?:^|\/)Qwen3-1\.7B-Q8_0\.gguf$/i.test(file))) {
    throw new Error("portable payload must not include the optional Qwen3 1.7B model");
  }
  if (actualFiles.some((file) => /(?:^|\/)Qwen3-4B-Q4_K_M\.gguf$/i.test(file))) {
    throw new Error("portable payload must not include the Qwen3 4B model");
  }
  if (actualFiles.some((file) => /(?:^|\/)__MACOSX(?:\/|$)/i.test(file))) {
    throw new Error("portable payload must not contain archive metadata");
  }

  for (const [relative, expectedHash] of Object.entries(requiredHashes)) {
    const record = records.get(relative);
    if (!record) throw new Error(`portable hash-pinned file is missing: ${relative}`);
    if (record.sha256 !== expectedHash.toLowerCase()) {
      throw new Error(`${relative} does not match its release SHA-256 pin`);
    }
  }

  return {
    manifest,
    files: actualFiles,
    totalBytes: [...records.values()].reduce((total, record) => total + record.size, 0),
  };
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
  const args = parseArgs(values);
  const root = path.resolve(args.root ?? "");
  if (!root || root === path.parse(root).root) {
    throw new Error("--root must identify a specific portable staging directory");
  }
  await mkdir(root, { recursive: true });
  if (command === "write") {
    const manifest = await writePortableManifest(root, {
      version: args.version,
      artifact: args.artifact,
    });
    console.log(`Wrote portable manifest for ${manifest.version}: ${manifest.files.length} files.`);
    return;
  }
  if (command === "verify") {
    const result = await validatePortableTree(root, { expectedVersion: args.version });
    console.log(
      `Portable tree verified: ${result.files.length} files, ${result.totalBytes} payload bytes.`,
    );
    return;
  }
  throw new Error("usage: portable-contract.mjs <write|verify> --root DIR --version VERSION");
}

const isMain = process.argv[1] &&
  path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isMain) {
  runCli().catch((error) => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  });
}
