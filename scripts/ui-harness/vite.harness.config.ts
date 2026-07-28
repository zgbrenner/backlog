// Builds the REAL frontend (src/main.ts, src/styles.css, index.html) against a
// browser stand-in for the Tauri runtime, so the UI can be rendered and
// screenshotted without Windows, the sidecars, or the GGUF weights.
//
// Kept deliberately separate from the root `vite.config.ts`: the shipped bundle
// must never resolve `@tauri-apps/*` to a mock. Nothing in this file is
// referenced by `npm run build` or `tauri build`.

import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";

const mock = fileURLToPath(new URL("./mock-tauri.ts", import.meta.url));
const root = fileURLToPath(new URL("../..", import.meta.url));

export default defineConfig({
  root,
  clearScreen: false,
  resolve: {
    alias: [
      { find: "@tauri-apps/api/core", replacement: mock },
      { find: "@tauri-apps/api/event", replacement: mock },
      { find: "@tauri-apps/plugin-dialog", replacement: mock },
      { find: "@tauri-apps/plugin-updater", replacement: mock },
      { find: "@tauri-apps/plugin-process", replacement: mock },
    ],
  },
  build: {
    target: "es2021",
    outDir: fileURLToPath(new URL("../../dist-harness", import.meta.url)),
    emptyOutDir: true,
  },
  server: { port: 1421, strictPort: true },
});
