// Renders every harness scenario in headless Chromium and writes a PNG per
// scenario, in both light and dark, at the app's real default window size.
//
//   node scripts/ui-harness/shoot.mjs [--out DIR] [--scenario NAME]
//
// Exits non-zero if any page logs a console error or throws — so this doubles
// as a smoke test that the frontend actually boots, not just a screenshotter.

import { createServer } from "vite";
import { chromium } from "playwright";
import { fileURLToPath } from "node:url";
import { mkdir, rm } from "node:fs/promises";
import path from "node:path";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const argOf = (flag, fallback) => {
  const i = argv.indexOf(flag);
  return i !== -1 && argv[i + 1] ? argv[i + 1] : fallback;
};

const OUT = path.resolve(argOf("--out", path.join(HERE, "../../dist-harness/shots")));
const ONLY = argOf("--scenario", null);

// Matches the `app.windows[0]` size in src-tauri/tauri.conf.json, so a
// screenshot shows what the user actually sees on first launch.
const VIEWPORT = { width: 1180, height: 780 };

const { SCENARIOS } = await import("./fixtures.ts").catch(async () => {
  // fixtures.ts is TypeScript; go through Vite's own module loader so we do not
  // need a separate TS runtime just to read a list of scenario names.
  const s = await createServer({
    configFile: path.join(HERE, "vite.harness.config.ts"),
    server: { middlewareMode: true },
  });
  const mod = await s.ssrLoadModule(path.join(HERE, "fixtures.ts"));
  await s.close();
  return mod;
});

const names = ONLY ? [ONLY] : Object.keys(SCENARIOS);
if (ONLY && !SCENARIOS[ONLY]) {
  console.error(`unknown scenario '${ONLY}'; known: ${Object.keys(SCENARIOS).join(", ")}`);
  process.exit(2);
}

await rm(OUT, { recursive: true, force: true });
await mkdir(OUT, { recursive: true });

const server = await createServer({ configFile: path.join(HERE, "vite.harness.config.ts") });
await server.listen();
const base = server.resolvedUrls.local[0].replace(/\/$/, "");

// Prefer a Chromium already staged in the image (PLAYWRIGHT_BROWSERS_PATH) over
// downloading one: CI images commonly pin a build that does not match whatever
// revision the installed `playwright` package wants, and `playwright install`
// may be blocked outright. BACKLOG_CHROMIUM overrides for an unusual setup.
async function launchChromium() {
  const explicit = process.env.BACKLOG_CHROMIUM;
  const candidates = [
    explicit,
    ...(process.env.PLAYWRIGHT_BROWSERS_PATH
      ? [
          `${process.env.PLAYWRIGHT_BROWSERS_PATH}/chromium/chrome-linux/chrome`,
          ...(await import("node:fs")).globSync?.(
            `${process.env.PLAYWRIGHT_BROWSERS_PATH}/chromium-*/chrome-linux/chrome`
          ) ?? [],
        ]
      : []),
  ].filter(Boolean);
  const { existsSync } = await import("node:fs");
  for (const executablePath of candidates) {
    if (existsSync(executablePath)) return chromium.launch({ executablePath });
  }
  // Nothing staged — fall back to whatever the package manages itself.
  return chromium.launch();
}

const browser = await launchChromium();
const failures = [];

for (const name of names) {
  for (const scheme of ["light", "dark"]) {
    const ctx = await browser.newContext({
      viewport: VIEWPORT,
      colorScheme: scheme,
      deviceScaleFactor: 2,
    });
    const page = await ctx.newPage();
    const problems = [];
    // Chromium requests /favicon.ico unprompted; a 404 for it is an artifact of
    // running in a browser, not a defect in an app that ships its icon through
    // the Tauri window. Everything else counts.
    // The message text for a failed subresource is generic ("Failed to load
    // resource: ... 404"), so the URL has to come from location(), not text().
    page.on("console", (m) => {
      if (m.type() !== "error") return;
      const url = m.location()?.url ?? "";
      if (/favicon\.ico/.test(url) || /favicon\.ico/.test(m.text())) return;
      problems.push(`console.error: ${m.text()}${url ? ` (${url})` : ""}`);
    });
    page.on("requestfailed", (r) => {
      if (!/favicon\.ico/.test(r.url())) problems.push(`request failed: ${r.url()}`);
    });
    page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));

    await page.goto(`${base}/?scenario=${encodeURIComponent(name)}`, {
      waitUntil: "networkidle",
    });
    // The frontend's startup IIFE awaits get_config then renders; give the
    // microtask queue and the 400ms render coalescer room to settle.
    await page.waitForSelector("#app .shell, #app .fatal", { timeout: 10_000 });
    await page.waitForTimeout(600);

    const file = path.join(OUT, `${name}.${scheme}.png`);
    await page.screenshot({ path: file, fullPage: true });

    if (problems.length) failures.push(`${name} (${scheme}): ${problems.join(" | ")}`);
    console.log(`${problems.length ? "FAIL" : "ok  "}  ${name} (${scheme}) -> ${path.relative(process.cwd(), file)}`);
    await ctx.close();
  }
}

await browser.close();
await server.close();

if (failures.length) {
  console.error(`\n${failures.length} scenario(s) produced errors:`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`\nAll ${names.length} scenario(s) rendered clean into ${OUT}`);
