// Renders every harness scenario in headless Chromium and writes a PNG per
// scenario, in both light and dark, at the app's real default window size —
// then drives the two interactions that are easy to break and impossible to
// see in a screenshot.
//
//   node scripts/ui-harness/shoot.mjs [--out DIR] [--scenario NAME] [--no-assert]
//
// Exits non-zero if any page logs a console error or throws, if a scenario's
// light and dark renders are byte-identical (which means the theme work has
// regressed), or if a behavioural assertion fails — so this is a smoke test of
// the frontend, not just a screenshotter.

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
const SKIP_ASSERTS = argv.includes("--no-assert");

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

// An ephemeral port, not the config's fixed 1421: that one belongs to
// `npm run harness` (a human wants a stable URL), and two shoots racing for it
// made one of them die with EADDRINUSE.
const server = await createServer({
  configFile: path.join(HERE, "vite.harness.config.ts"),
  server: { port: 0, strictPort: false },
});
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

/** Attach the console/network/exception watchers. Anything they collect fails
 *  the run, so this doubles as "the frontend still boots". */
function watch(page) {
  const problems = [];
  // Chromium requests /favicon.ico unprompted; a 404 for it is an artifact of
  // running in a browser, not a defect in an app that ships its icon through
  // the Tauri window. Everything else counts. The message text for a failed
  // subresource is generic, so the URL has to come from location().
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
  return problems;
}

async function boot(page, name) {
  await page.goto(`${base}/?scenario=${encodeURIComponent(name)}`, { waitUntil: "networkidle" });
  await page.waitForSelector("#app .shell, #app .fatal", { timeout: 10_000 });
  const wanted = SCENARIOS[name]?.view;
  if (wanted) {
    await page.click(`nav button[data-v="${wanted}"]`);
  }
  // Let the microtask queue, the render loop and the header coalescer settle.
  await page.waitForTimeout(600);
}

/** Per-scenario driving that puts the page into a state the fixtures alone
 *  cannot reach, because it lives behind a click and a stream of events. */
const DRIVERS = {
  async downloading(page) {
    await page.click("#download-models-button");
    await page.waitForSelector("#dl-fill", { state: "attached" });
    await page.evaluate(() => {
      window.__harness.emit("model-download-progress", {
        current_file: "Qwen3-1.7B-Q8_0.gguf",
        file_bytes_done: 812_000_000,
        file_bytes_total: 1_890_000_000,
        files_done: 1,
        files_total: 2,
        overall_percent: 61.4,
      });
    });
    await page.waitForTimeout(200);
  },
  async "review-detail"(page) {
    const card = page.locator(".card").first();
    await card.locator('[data-act="evidence"]').click();
    await card.locator('[data-act="events"]').click();
    await page.waitForTimeout(300);
  },
  async toasts(page) {
    // Start fails; three presses is the misconfigured-machine case where every
    // toast used to be positioned identically and only the last was readable.
    for (let i = 0; i < 3; i++) {
      await page.click("#runbtn");
      await page.waitForTimeout(150);
    }
    await page.waitForTimeout(300);
  },
};

const browser = await launchChromium();
const failures = [];

for (const name of names) {
  const rendered = {};
  for (const scheme of ["light", "dark"]) {
    const ctx = await browser.newContext({ viewport: VIEWPORT, colorScheme: scheme, deviceScaleFactor: 2 });
    const page = await ctx.newPage();
    const problems = watch(page);

    await boot(page, name);
    if (DRIVERS[name]) await DRIVERS[name](page);

    const file = path.join(OUT, `${name}.${scheme}.png`);
    // Keep the bytes: the light/dark comparison below reads them from memory
    // rather than from disk, so a concurrent clean of dist-harness cannot turn
    // a passing run into a spurious ENOENT.
    rendered[scheme] = await page.screenshot({ path: file, fullPage: true });

    // `main` is the scroll container, not the document, so fullPage still stops
    // at the window edge — a reviewer looking only at these would never see the
    // Settings form under the Readiness panel. Write a tall companion shot
    // whenever the view actually overflows.
    const overflow = await page.evaluate(() => {
      const pane = document.getElementById("content");
      return pane ? pane.scrollHeight - pane.clientHeight : 0;
    });
    if (overflow > 4) {
      await page.setViewportSize({
        width: VIEWPORT.width,
        height: Math.min(VIEWPORT.height + overflow + 24, 4200),
      });
      await page.waitForTimeout(250);
      await page.screenshot({ path: file.replace(/\.png$/, ".full.png"), fullPage: true });
      await page.setViewportSize(VIEWPORT);
    }

    if (problems.length) failures.push(`${name} (${scheme}): ${problems.join(" | ")}`);
    console.log(
      `${problems.length ? "FAIL" : "ok  "}  ${name} (${scheme}) -> ${path.relative(process.cwd(), file)}`
    );
    await ctx.close();
  }

  // The whole point of shooting both schemes is that they differ. They were
  // byte-identical for the life of this harness, because the stylesheet had no
  // colour-scheme handling at all.
  if (rendered.light?.equals(rendered.dark)) {
    failures.push(`${name}: light and dark renders are identical`);
  }
}

// ---------------------------------------------------------------------------
// Behavioural assertions. These cover the two ways the review loop used to eat
// a reviewer's work: a background event re-rendering the shell mid-correction,
// and the model download re-rendering Settings five times a second.
// ---------------------------------------------------------------------------

const CHECKS = [
  {
    name: "a job-updated event never touches a card being edited",
    async run(page) {
      await boot(page, "review");
      const card = page.locator(".card").first();
      const sha = await card.getAttribute("data-sha");
      const subject = card.locator('[name="subject"]');
      await subject.click();
      await subject.fill("");
      await page.keyboard.type("Riverside lease agreement");

      // Exactly what pipeline.rs::emit_update ships: the full ledger row.
      await page.evaluate((sha256) => {
        window.__harness.emit("job-updated", {
          sha256,
          original_path: "C:\\Processing\\IMG_20260214_113355.jpg",
          original_name: "IMG_20260214_113355.jpg",
          original_relpath: "IMG_20260214_113355.jpg",
          ext: "jpg",
          state: "flagged",
          flag_reason: "DATE_NOT_IN_EVIDENCE:2026-02-14",
          quarantine_path: null,
          proposed_date: "1999-01-01",
          date_source: "document",
          proposed_subject: "CLOBBERED BY A BACKGROUND EVENT",
          description: "Clobbered.",
          final_filename: null,
          doc_type: "scan",
          soft_flags: null,
          created_at: "2026-07-28T09:10:00.000Z",
          updated_at: new Date().toISOString(),
        });
      }, sha);
      // Longer than the old 400ms render coalescer and the 1.2s header refresh.
      await page.waitForTimeout(1800);

      const value = await subject.inputValue();
      const focused = await page.evaluate(() => document.activeElement?.getAttribute("name"));
      const problems = [];
      if (value !== "Riverside lease agreement") {
        problems.push(`subject was rewritten to ${JSON.stringify(value)}`);
      }
      if (focused !== "subject") problems.push(`focus moved to ${JSON.stringify(focused)}`);
      // The update must still be discoverable, just not destructive.
      const chip = await page.locator("#refresh-chip").isVisible();
      if (!chip) problems.push("no refresh chip offered the update");
      return problems;
    },
  },
  {
    name: "model download progress never touches the Settings form",
    async run(page) {
      await boot(page, "downloading");
      await page.click("#download-models-button");
      await page.waitForSelector("#dl-fill", { state: "attached" });

      const folder = page.locator('[name="processing_dir"]');
      await folder.click();
      await folder.fill("");
      await page.keyboard.type("D:\\Scans\\Intake");

      // The backend throttles to ~200ms/file for ~2.4 GB; five in a row is a
      // second of a twenty-minute download.
      for (let i = 1; i <= 5; i++) {
        await page.evaluate((n) => {
          window.__harness.emit("model-download-progress", {
            current_file: "Qwen3-0.6B-Q8_0.gguf",
            file_bytes_done: n * 40_000_000,
            file_bytes_total: 640_000_000,
            files_done: 0,
            files_total: 2,
            overall_percent: n * 4,
          });
        }, i);
        await page.waitForTimeout(60);
      }
      await page.waitForTimeout(400);

      const problems = [];
      if ((await folder.inputValue()) !== "D:\\Scans\\Intake") {
        problems.push(`the folder field was reset to ${JSON.stringify(await folder.inputValue())}`);
      }
      const focused = await page.evaluate(() => document.activeElement?.getAttribute("name"));
      if (focused !== "processing_dir") problems.push(`focus moved to ${JSON.stringify(focused)}`);
      const width = await page.locator("#dl-fill").evaluate((n) => n.style.width);
      if (width !== "20%") problems.push(`progress bar did not advance in place (width ${width})`);
      return problems;
    },
  },
  {
    name: "first run shows unknown checks, not ten red failures",
    async run(page) {
      await boot(page, "first-run");
      const unknown = await page.locator(".check-unknown").count();
      const failed = await page.locator(".check-fail").count();
      const problems = [];
      if (unknown !== 11) problems.push(`expected 11 'Not checked' rows, saw ${unknown}`);
      if (failed !== 0) problems.push(`${failed} rows claimed Blocked before any check ran`);
      const chip = await page.locator("#readiness-chip").textContent();
      if (chip.trim() !== "Not checked") problems.push(`readiness chip said ${JSON.stringify(chip)}`);
      return problems;
    },
  },
  {
    name: "Start is disabled with a visible on-screen reason",
    async run(page) {
      await boot(page, "first-run");
      const problems = [];
      if (!(await page.locator("#runbtn").isDisabled())) problems.push("Start was not disabled");
      if (!(await page.locator("#start-hint").isVisible())) {
        problems.push("no visible reason beside the disabled Start button");
      }
      if (await page.locator("#runbtn").getAttribute("title")) {
        problems.push("Start still carries a title= a disabled control cannot show");
      }
      return problems;
    },
  },
  {
    name: "the queue search and state filter reach the backend",
    async run(page) {
      await boot(page, "scale");
      await page.fill("#queue-search", "batch-0042");
      await page.waitForTimeout(600);
      const problems = [];
      const rows = await page.locator("tbody tr").count();
      if (rows !== 1) problems.push(`search for one file returned ${rows} rows`);
      const args = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "list_jobs").pop()?.args
      );
      if (args?.query !== "batch-0042") problems.push(`list_jobs got query ${JSON.stringify(args)}`);
      await page.click('.filter-chip[aria-pressed="false"] >> nth=0');
      await page.waitForTimeout(400);
      return problems;
    },
  },
  {
    name: "approving a card is undoable before anything is written",
    async run(page) {
      await boot(page, "review");
      const card = page.locator(".card").first();
      await card.locator('[name="date"]').fill("2026-02-14");
      await card.locator('[name="subject"]').fill("Riverside lease agreement");
      await card
        .locator('[name="description"]')
        .fill("Signed lease agreement for the Riverside unit between Contoso and A. Patel.");
      await card.locator('[data-act="approve"]').click();
      await page.waitForTimeout(300);

      const problems = [];
      if (!(await card.locator(".undo-strip").isVisible())) problems.push("no undo strip appeared");
      let called = await page.evaluate(() =>
        window.__harness.invocations.some((i) => i.cmd === "resubmit")
      );
      if (called) problems.push("resubmit fired before the undo window elapsed");

      await card.locator('[data-act="undo"]').click();
      await page.waitForTimeout(300);
      called = await page.evaluate(() =>
        window.__harness.invocations.some((i) => i.cmd === "resubmit")
      );
      if (called) problems.push("undo did not stop the pending approval");
      if ((await card.locator('[name="subject"]').inputValue()) !== "Riverside lease agreement") {
        problems.push("undo lost the reviewer's typing");
      }
      return problems;
    },
  },
];

if (!SKIP_ASSERTS && !ONLY) {
  for (const check of CHECKS) {
    const ctx = await browser.newContext({ viewport: VIEWPORT, colorScheme: "dark" });
    const page = await ctx.newPage();
    const problems = watch(page);
    try {
      problems.push(...(await check.run(page)));
    } catch (e) {
      problems.push(`threw: ${e.message}`);
    }
    if (problems.length) failures.push(`assert "${check.name}": ${problems.join(" | ")}`);
    console.log(`${problems.length ? "FAIL" : "ok  "}  assert: ${check.name}`);
    await ctx.close();
  }
}

await browser.close();
await server.close();

if (failures.length) {
  console.error(`\n${failures.length} problem(s):`);
  for (const f of failures) console.error(`  - ${f}`);
  process.exit(1);
}
console.log(`\nAll ${names.length} scenario(s) rendered clean into ${OUT}`);
