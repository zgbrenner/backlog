import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  PORTABLE_REQUIRED_PATHS,
  PORTABLE_STUB_MARKER,
  PORTABLE_WEBVIEW2_CAB_SHA256,
  PORTABLE_WEBVIEW2_CAB_SIZE,
  PORTABLE_WEBVIEW2_CAB_URL,
  PORTABLE_WEBVIEW2_VERSION,
  validatePortableTree,
  writePortableManifest,
} from "./portable-contract.mjs";

const VERSION = "0.6.0";
const ARTIFACT = `BackLog_${VERSION}_x64-portable.zip`;
const WEBVIEW2_FILE_COUNT = 3;

function peFixture() {
  const bytes = Buffer.alloc(128);
  bytes[0] = 0x4d;
  bytes[1] = 0x5a;
  bytes.writeInt32LE(64, 0x3c);
  bytes.write("PE\0\0", 64, "ascii");
  return bytes;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function writeFixture(root) {
  const runtimeFiles = [
    `${PORTABLE_WEBVIEW2_VERSION}.manifest`,
    "msedge.dll",
    "msedgewebview2.exe",
  ];
  const files = new Map([
    ["BackLog.exe", peFixture()],
    [
      "BackLog-Portable.cmd",
      Buffer.from(
        '@echo off\r\nset "BACKLOG_ROOT=%~dp0"\r\nif "%BACKLOG_ROOT:~0,2%"=="\\" exit /b 4\r\nset "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=%BACKLOG_ROOT%webview2-fixed"\r\nicacls "%~dp0webview2-fixed" /grant "*S-1-15-2-2:(OI)(CI)(RX)" /T\r\nicacls "%~dp0webview2-fixed" /grant "*S-1-15-2-1:(OI)(CI)(RX)" /T\r\n"%~dp0BackLog.exe"\r\n',
      ),
    ],
    ["llama-server-x86_64-pc-windows-msvc.exe", peFixture()],
    ["convertd/convertd.exe", peFixture()],
    ["runtime.dll", peFixture()],
    ["resources/name.gbnf", Buffer.from("root ::= \"fixture\"\n")],
    ["resources/models/Qwen3-0.6B-Q8_0.gguf", Buffer.from("primary")],
    [
      "resources/models/semantic/all-MiniLM-L6-v2/model.onnx",
      Buffer.from("semantic-model"),
    ],
    [
      "resources/models/semantic/all-MiniLM-L6-v2/vocab.txt",
      Buffer.from("semantic-vocab\n"),
    ],
    ["README-PORTABLE.md", Buffer.from("Run BackLog-Portable.cmd.\n")],
    [
      "webview2-fixed/msedgewebview2.exe",
      peFixture(),
    ],
    ["webview2-fixed/msedge.dll", peFixture()],
    [
      `webview2-fixed/${PORTABLE_WEBVIEW2_VERSION}.manifest`,
      Buffer.from("<assembly></assembly>\n"),
    ],
    [
      "webview2-fixed/runtime-manifest.json",
      Buffer.from(
        `${JSON.stringify(
          {
            schema: 1,
            product: "BackLog",
            version: PORTABLE_WEBVIEW2_VERSION,
            architecture: "x64",
            cab_url: PORTABLE_WEBVIEW2_CAB_URL,
            cab_sha256: PORTABLE_WEBVIEW2_CAB_SHA256,
            cab_size: PORTABLE_WEBVIEW2_CAB_SIZE,
            file_count: WEBVIEW2_FILE_COUNT,
            files: runtimeFiles,
          },
          null,
          2,
        )}\n`,
      ),
    ],
  ]);
  for (const [relative, bytes] of files) {
    const file = path.join(root, relative);
    await mkdir(path.dirname(file), { recursive: true });
    await writeFile(file, bytes);
  }
  await writePortableManifest(root, {
    version: VERSION,
    artifact: ARTIFACT,
    webview2FileCount: WEBVIEW2_FILE_COUNT,
  });
  const requiredHashes = Object.fromEntries(
    [
      "llama-server-x86_64-pc-windows-msvc.exe",
      "resources/models/Qwen3-0.6B-Q8_0.gguf",
      "resources/models/semantic/all-MiniLM-L6-v2/model.onnx",
      "resources/models/semantic/all-MiniLM-L6-v2/vocab.txt",
    ].map((relative) => [relative, sha256(files.get(relative))]),
  );
  return requiredHashes;
}

async function withFixture(callback) {
  const root = await mkdtemp(path.join(os.tmpdir(), "backlog-portable-"));
  try {
    const requiredHashes = await writeFixture(root);
    await callback(root, requiredHashes);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

test("a portable tree has an exact, hash-recorded payload contract", async () => {
  await withFixture(async (root, requiredHashes) => {
    const result = await validatePortableTree(root, {
      expectedVersion: VERSION,
      requiredHashes,
      expectedWebView2FileCount: WEBVIEW2_FILE_COUNT,
    });
    assert.deepEqual(
      result.files,
      [
        ...PORTABLE_REQUIRED_PATHS,
        "runtime.dll",
        "webview2-fixed/msedge.dll",
        `webview2-fixed/${PORTABLE_WEBVIEW2_VERSION}.manifest`,
      ].sort(),
    );
    assert.equal(result.manifest.artifact, ARTIFACT);
    assert.equal(result.manifest.webview2, "fixed-runtime");
    assert.equal(result.manifest.webview2_runtime.version, PORTABLE_WEBVIEW2_VERSION);
    assert.equal(result.manifest.webview2_runtime.file_count, WEBVIEW2_FILE_COUNT);

    const first = await readFile(path.join(root, "portable-manifest.json"), "utf8");
    await writePortableManifest(root, {
      version: VERSION,
      artifact: ARTIFACT,
      webview2FileCount: WEBVIEW2_FILE_COUNT,
    });
    assert.equal(
      await readFile(path.join(root, "portable-manifest.json"), "utf8"),
      first,
      "manifest generation must be stable for unchanged inputs",
    );
  });
});

test("a changed payload is rejected by its recorded hash", async () => {
  await withFixture(async (root, requiredHashes) => {
    await writeFile(path.join(root, "README-PORTABLE.md"), "tampered\n");
    await assert.rejects(
      () => validatePortableTree(root, {
        expectedVersion: VERSION,
        requiredHashes,
        expectedWebView2FileCount: WEBVIEW2_FILE_COUNT,
      }),
      /README-PORTABLE\.md (?:size changed|SHA-256 does not match)/,
    );
  });
});

test("an unlisted payload file is rejected", async () => {
  await withFixture(async (root, requiredHashes) => {
    await writeFile(path.join(root, "unexpected.txt"), "not in the manifest\n");
    await assert.rejects(
      () => validatePortableTree(root, {
        expectedVersion: VERSION,
        requiredHashes,
        expectedWebView2FileCount: WEBVIEW2_FILE_COUNT,
      }),
      /unlisted file: unexpected\.txt/,
    );
  });
});

test("development stubs and the optional escalation model cannot enter the ZIP", async () => {
  await withFixture(async (root, requiredHashes) => {
    await writeFile(
      path.join(root, "resources/models/Qwen3-0.6B-Q8_0.gguf"),
      PORTABLE_STUB_MARKER,
    );
    await writePortableManifest(root, {
      version: VERSION,
      artifact: ARTIFACT,
      webview2FileCount: WEBVIEW2_FILE_COUNT,
    });
    await assert.rejects(
      () => validatePortableTree(root, {
        expectedVersion: VERSION,
        requiredHashes,
        expectedWebView2FileCount: WEBVIEW2_FILE_COUNT,
      }),
      /carries BACKLOG-DEV-STUB-DO-NOT-SHIP/,
    );

    await writeFile(path.join(root, "resources/models/Qwen3-0.6B-Q8_0.gguf"), "primary");
    await writeFile(path.join(root, "resources/models/Qwen3-1.7B-Q8_0.gguf"), "optional");
    await writePortableManifest(root, {
      version: VERSION,
      artifact: ARTIFACT,
      webview2FileCount: WEBVIEW2_FILE_COUNT,
    });
    await assert.rejects(
      () => validatePortableTree(root, {
        expectedVersion: VERSION,
        requiredHashes,
        expectedWebView2FileCount: WEBVIEW2_FILE_COUNT,
      }),
      /must not include the optional Qwen3 1\.7B model/,
    );
  });
});
