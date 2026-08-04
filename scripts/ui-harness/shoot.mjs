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

async function boot(page, name, { disableLazyEvidence = false } = {}) {
  if (disableLazyEvidence) {
    // The evidence retry assertion needs the first get_evidence call to come
    // from the button, not the date-chip observer. This is a harness-only
    // control; production keeps the lazy date extraction behavior.
    await page.addInitScript(() => {
      window.IntersectionObserver = class {
        observe() {}
        disconnect() {}
      };
    });
  }
  // Vite keeps a development HMR connection open, and the production shell
  // may start a background update check. Neither is part of first paint; using
  // networkidle here made a healthy page hang for 30 seconds before the actual
  // selector/assertion checks could run. DOM readiness plus the shell selector
  // below proves the app booted without coupling the harness to background
  // traffic.
  await page.goto(`${base}/?scenario=${encodeURIComponent(name)}`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector("#app .shell, #app .fatal", { timeout: 10_000 });
  const wanted = SCENARIOS[name]?.view;
  if (wanted) {
    const tab = wanted === "flagged"
      ? page.getByRole("tab", { name: /Needs Review/ })
      : page.getByRole("tab", { name: wanted === "settings" ? "Settings" : "Queue" });
    await tab.click();
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
    // Twelve presses, because errors never auto-dismiss: this is the state
    // where the column used to run off the top of the window and cover the
    // very button being pressed.
    for (let i = 0; i < 12; i++) {
      await page.click("#runbtn");
      await page.waitForTimeout(90);
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
    name: "a hanging loading read becomes a retryable error",
    async run(page) {
      await boot(page, "loading");
      const problems = [];
      await page.getByRole("tab", { name: "Settings" }).click();
      await page.getByRole("heading", { name: "Folders" }).waitFor({ state: "visible", timeout: 5000 });
      await page.getByRole("tab", { name: "Queue" }).click();
      const error = page.locator(".err-state");
      await error.waitFor({ state: "visible", timeout: 5000 });
      if (!/taking too long to answer/i.test(await error.innerText())) {
        problems.push("the hung read did not become a plain-language timeout");
      }
      const before = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "get_stats").length
      );
      await error.getByRole("button", { name: "Try again" }).click();
      await error.waitFor({ state: "visible", timeout: 5000 });
      const after = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "get_stats").length
      );
      if (after <= before) problems.push("Try again did not issue a fresh bounded read");
      return problems;
    },
  },
  {
    name: "folder Browse buttons have unique accessible names",
    async run(page) {
      await boot(page, "first-run");
      const browse = page.getByRole("button", { name: /^Browse for / });
      const names = await browse.evaluateAll((buttons) =>
        buttons.map((button) => button.getAttribute("aria-label") ?? "")
      );
      const problems = [];
      if (names.length !== 3) problems.push(`expected 3 visible folder Browse buttons, saw ${names.length}`);
      if (new Set(names).size !== names.length) problems.push("visible Browse buttons still share an accessible name");
      for (const folder of ["Processing folder", "Outbox folder", "Quarantine folder"]) {
        if (!names.some((name) => name.startsWith(`Browse for ${folder}`))) {
          problems.push(`no accessible Browse name for ${folder}`);
        }
      }
      return problems;
    },
  },
  {
    name: "legacy settings visibly default to the Power Automate handoff",
    async run(page) {
      await boot(page, "first-run");
      const mode = page.locator('[name="output_mode"]');
      const problems = [];
      if ((await mode.inputValue()) !== "power_automate") {
        problems.push(`legacy config selected ${JSON.stringify(await mode.inputValue())}, not Power Automate`);
      }
      if (!(await page.locator('[name="outbox_dir"]').isVisible())) {
        problems.push("Power Automate mode did not show Outbox");
      }
      if (await page.locator('[name="local_output_dir"]').isVisible()) {
        problems.push("Power Automate mode still showed Local Output");
      }
      if (!(await page.getByText("Outbox folder is writable").isVisible())) {
        problems.push("Power Automate readiness did not name Outbox");
      }
      if (!(await page.getByText(/handoff manifest to Outbox for Flow 2/i).isVisible())) {
        problems.push("Power Automate explanatory copy was missing");
      }
      return problems;
    },
  },
  {
    name: "Local Output changes only the required delivery folder and readiness label",
    async run(page) {
      await boot(page, "local-ready");
      const problems = [];
      if (!(await page.locator('[name="local_output_dir"]').isVisible())) {
        problems.push("Local mode did not show Local Output");
      }
      if (await page.locator('[name="outbox_dir"]').isVisible()) {
        problems.push("Local mode still showed Outbox");
      }
      if (!(await page.getByText("Local Output folder is writable").isVisible())) {
        problems.push("Local mode readiness did not name Local Output");
      }
      if (await page.getByText("Outbox folder is writable").isVisible()) {
        problems.push("Local mode readiness still named Outbox");
      }
      if (!(await page.getByText(/finished renamed document directly to Local Output/i).isVisible())) {
        problems.push("Local Output explanatory copy was missing");
      }
      return problems;
    },
  },
  {
    name: "switching output modes preserves both paths and targets the folder picker",
    async run(page) {
      await boot(page, "first-run");
      const mode = page.locator('[name="output_mode"]');
      const outbox = page.locator('[name="outbox_dir"]');
      const local = page.locator('[name="local_output_dir"]');
      await outbox.fill('D:\\Outbox');
      await mode.selectOption("local");
      await local.fill('D:\\Filed');
      const problems = [];
      if (await outbox.isVisible()) problems.push("switching to Local did not hide Outbox");
      if ((await local.inputValue()) !== 'D:\\Filed') problems.push("Local Output value was not retained");
      const localBrowse = page.getByRole("button", { name: /^Browse for Local Output folder/ });
      await localBrowse.click();
      const picker = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "open_dialog").at(-1)
      );
      if (!picker || picker.args?.directory !== true || picker.args?.multiple !== false) {
        problems.push(`Local Output Browse did not open a single-folder picker: ${JSON.stringify(picker)}`);
      }
      await mode.selectOption("power_automate");
      if ((await outbox.inputValue()) !== 'D:\\Outbox') problems.push("Outbox value was lost after switching back");
      if (await local.isVisible()) problems.push("switching back to Power Automate did not hide Local Output");
      return problems;
    },
  },
  {
    name: "first-run Local Output saves the selected mode and does not require Outbox",
    async run(page) {
      await boot(page, "first-run-save");
      await page.locator('[name="output_mode"]').selectOption("local");
      await page.locator('[name="processing_dir"]').fill("D:\\Intake");
      await page.locator('[name="local_output_dir"]').fill("D:\\Filed");
      await page.locator('[name="quarantine_dir"]').fill("D:\\Quarantine");
      const problems = [];
      const action = page.locator('.settings button[type="submit"]');
      if ((await action.textContent()).trim() !== "Save and check this computer") {
        problems.push("Local first run did not retain the combined save-and-check action");
      }
      await action.click();
      await page.waitForTimeout(450);
      const saved = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "set_config").at(-1)?.args?.cfg
      );
      if (saved?.output_mode !== "local") {
        problems.push(`Local first run saved mode ${JSON.stringify(saved?.output_mode)}`);
      }
      if (saved?.local_output_dir !== "D:\\Filed") {
        problems.push(`Local Output path was not saved: ${JSON.stringify(saved?.local_output_dir)}`);
      }
      return problems;
    },
  },
  {
    name: "review copy follows each job's pinned delivery mode after Settings switches",
    async run(page) {
      await boot(page, "local-review");
      await page.getByRole("tab", { name: "Settings" }).click();
      await page.locator('[name="output_mode"]').selectOption("power_automate");
      await page.getByRole("tab", { name: /Needs Review/ }).click();
      const note = page.locator(".delivery-note").first();
      if (!/directly from Quarantine into Local Output/i.test(await note.innerText())) {
        return ["Local-pinned review followed the later Power Automate Settings selection"];
      }
      await boot(page, "review");
      await page.getByRole("tab", { name: "Settings" }).click();
      await page.locator('[name="output_mode"]').selectOption("local");
      await page.getByRole("tab", { name: /Needs Review/ }).click();
      if (!/updates the Power Automate handoff/i.test(await page.locator(".delivery-note").first().innerText())) {
        return ["Power-Automate-pinned review followed the later Local Settings selection"];
      }
      return [];
    },
  },
  {
    name: "the narrow queue stays readable without page-wide clipping",
    async run(page) {
      await boot(page, "ready");
      await page.setViewportSize({ width: 480, height: 720 });
      await page.waitForTimeout(100);
      const metrics = await page.getByRole("table").evaluate((table) => {
        const wrap = table.parentElement;
        const cell = table.querySelector("td.mono");
        if (!wrap || !cell) return null;
        const wrapStyle = getComputedStyle(wrap);
        const cellStyle = getComputedStyle(cell);
        return {
          overflowX: wrapStyle.overflowX,
          tableWidth: table.scrollWidth,
          wrapWidth: wrap.clientWidth,
          pageWidth: document.documentElement.scrollWidth,
          viewportWidth: window.innerWidth,
          wordBreak: cellStyle.wordBreak,
        };
      });
      const problems = [];
      if (!metrics) return ["queue table metrics were unavailable"];
      if (metrics.overflowX !== "auto") problems.push(`queue overflow was ${metrics.overflowX}, not auto`);
      if (metrics.tableWidth <= metrics.wrapWidth) problems.push("the narrow table had no reachable overflow");
      if (metrics.pageWidth > metrics.viewportWidth + 1) problems.push("the page itself overflowed horizontally");
      if (metrics.wordBreak === "break-word") problems.push("filenames still break into narrow fragments");
      if (!(await page.getByText("On a narrow window, scroll horizontally to see all queue columns.").isVisible())) {
        problems.push("the narrow queue did not explain how to reach the remaining columns");
      }
      return problems;
    },
  },
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
    // This catches a tempting but dangerous split flow: checking a computer
    // before the folders displayed on screen have been saved tests yesterday's
    // configuration, not the one the operator just chose.
    name: "first-run save and check stores chosen folders before preflight",
    async run(page) {
      await boot(page, "first-run-save");
      await page.locator('[name="processing_dir"]').fill("D:\\Intake");
      await page.locator('[name="outbox_dir"]').fill("D:\\Outbox");
      await page.locator('[name="quarantine_dir"]').fill("D:\\Quarantine");
      const problems = [];
      if (!(await page.locator(".setup-intro").isVisible())) problems.push("no three-step first-run introduction");
      const action = page.locator('.settings button[type="submit"]');
      if ((await action.textContent()).trim() !== "Save and check this computer") {
        problems.push(`first-run action said ${JSON.stringify(await action.textContent())}`);
      }
      await action.click();
      await page.waitForTimeout(500);
      const calls = await page.evaluate(() => window.__harness.invocations.map((i) => i.cmd));
      const save = calls.lastIndexOf("set_config");
      const check = calls.lastIndexOf("run_preflight");
      if (save === -1 || check === -1 || save > check) {
        problems.push(`expected set_config before run_preflight, got ${JSON.stringify(calls)}`);
      }
      return problems;
    },
  },
  {
    name: "first run leads with setup and keeps the primary action in reach",
    async run(page) {
      await boot(page, "first-run");
      const setup = page.locator(".setup-intro");
      const readiness = page.locator(".preflight-panel");
      const action = page.locator('.settings button[type="submit"]');
      const problems = [];
      const setupBox = await setup.boundingBox();
      const readinessBox = await readiness.boundingBox();
      const actionBox = await action.boundingBox();
      if (!setupBox || !readinessBox || setupBox.y >= readinessBox.y) {
        problems.push("readiness details appeared before the first-run setup guidance");
      }
      if (!actionBox || actionBox.y + actionBox.height > 780) {
        problems.push("the first-run Save and check action is below the initial viewport");
      }
      if (!/Save and check this computer/.test(await setup.innerText())) {
        problems.push("the setup guidance does not describe the combined primary action");
      }
      if (!/optional backup model/.test(await setup.innerText())) {
        problems.push("the setup guidance does not explain the optional model download");
      }
      return problems;
    },
  },
  {
    name: "Save and check never claims success when the live check fails",
    async run(page) {
      await boot(page, "first-run-preflight-error");
      await page.locator('[name="processing_dir"]').fill("D:\\Intake");
      await page.locator('[name="outbox_dir"]').fill("D:\\Outbox");
      await page.locator('[name="quarantine_dir"]').fill("D:\\Quarantine");
      await page.locator('.settings button[type="submit"]').click();
      await page.waitForTimeout(450);
      const problems = [];
      const success = (await page.locator(".ok-msg").textContent()).trim();
      if (success !== "") {
        problems.push(`Save and check announced false success: ${JSON.stringify(success)}`);
      }
      if (!(await page.locator(".toast.error").isVisible())) {
        problems.push("the failed live check did not show an error");
      }
      return problems;
    },
  },
  {
    name: "an active model download can be cancelled",
    async run(page) {
      await boot(page, "downloading");
      await page.click("#download-models-button");
      await page.waitForSelector("#cancel-model-download");
      const problems = [];
      if ((await page.locator("#cancel-model-download").textContent()).trim() !== "Cancel download") {
        problems.push("active download did not offer Cancel download");
      }
      await page.click("#cancel-model-download");
      const called = await page.evaluate(() =>
        window.__harness.invocations.some((i) => i.cmd === "cancel_model_download")
      );
      if (!called) problems.push("Cancel download never reached cancel_model_download");
      return problems;
    },
  },
  {
    name: "a normal cancellation stays calm instead of reporting an error",
    async run(page) {
      await boot(page, "download-cancelling");
      await page.click("#download-models-button");
      await page.click("#cancel-model-download");
      await page.waitForTimeout(150);
      await page.evaluate(() => {
        window.__harness.emit("model-download-done", {
          ok: false,
          cancelled: true,
          error: "Download cancelled.",
          finished_at: new Date().toISOString(),
        });
      });
      await page.waitForTimeout(250);
      const problems = [];
      if (await page.locator(".toast.error").count()) {
        problems.push("cancelling the download displayed a generic error toast");
      }
      if (!/Download cancelled/.test(await page.locator(".model-download-terminal").innerText())) {
        problems.push("the cancellation did not leave a calm cancelled state");
      }
      return problems;
    },
  },
  {
    name: "terminal model downloads remain recoverable after Settings renders",
    async run(page) {
      const problems = [];
      for (const scenario of ["download-cancelled", "download-failed"]) {
        await boot(page, scenario);
        await page.waitForTimeout(350);
        const resume = page.locator("#download-models-button");
        if (!/^Resume download/.test((await resume.textContent()).trim())) {
          problems.push(`${scenario} did not offer Resume download`);
        }
      }
      await boot(page, "download-completed");
      const before = await page.evaluate(() => ({
        config: window.__harness.invocations.filter((i) => i.cmd === "get_config").length,
        preflight: window.__harness.invocations.filter((i) => i.cmd === "run_preflight").length,
      }));
      await page.click('nav button[data-v="settings"]');
      await page.waitForTimeout(350);
      if (!(await page.locator(".model-download-terminal").isVisible())) {
        problems.push("completed download was lost after returning to Settings");
      }
      if (!/Model download complete/.test(await page.locator(".model-download-terminal").innerText())) {
        problems.push("completed download did not restore its safe completion message");
      }
      const after = await page.evaluate(() => ({
        config: window.__harness.invocations.filter((i) => i.cmd === "get_config").length,
        preflight: window.__harness.invocations.filter((i) => i.cmd === "run_preflight").length,
      }));
      if (after.config <= before.config) {
        problems.push("completion did not refresh configuration");
      }
      if (after.preflight <= before.preflight) {
        problems.push("completion did not run a live readiness check");
      }
      const backupRow = page.locator(".check-row", { hasText: "Backup model file is present" });
      if (!(await backupRow.evaluate((row) => row.classList.contains("check-pass")))) {
        problems.push("completion did not change backup-model readiness to Ready");
      }
      return problems;
    },
  },
  {
    name: "a Power Automate caught-up queue keeps its handoff copy and real history",
    async run(page) {
      await boot(page, "caught-up-reviews");
      const text = await page.locator(".caught-up").innerText();
      const problems = [];
      if (!/Processing is caught up/.test(text)) problems.push(`missing caught-up state: ${JSON.stringify(text)}`);
      if (!/4 files need review/.test(text)) problems.push(`missing remaining review count: ${JSON.stringify(text)}`);
      if (!/Done means BackLog has handed a document to Power Automate\./.test(text)) {
        problems.push("Power Automate Done handoff copy changed");
      }
      if ((await page.locator("tbody tr").count()) !== 22) {
        problems.push("historical queue rows disappeared while showing caught-up status");
      }
      return problems;
    },
  },
  {
    name: "a Local Output caught-up queue describes local delivery and real history",
    async run(page) {
      await boot(page, "local-caught-up-reviews");
      const text = await page.locator(".caught-up").innerText();
      const problems = [];
      if (!/Processing is caught up/.test(text)) problems.push(`missing Local caught-up state: ${JSON.stringify(text)}`);
      if (!/4 files need review/.test(text)) problems.push(`missing Local remaining review count: ${JSON.stringify(text)}`);
      if (!/Done means BackLog wrote the renamed document to Local Output and recorded its receipt\./.test(text)) {
        problems.push("Local Output Done copy was missing");
      }
      if (/Power Automate|SharePoint/i.test(text)) {
        problems.push(`Local caught-up copy mentioned an unrelated handoff: ${JSON.stringify(text)}`);
      }
      if ((await page.locator("tbody tr").count()) !== 22) {
        problems.push("Local historical queue rows disappeared while showing caught-up status");
      }
      return problems;
    },
  },
  {
    name: "a Local Output empty queue does not direct intake through SharePoint",
    async run(page) {
      await boot(page, "local-queue-awaiting-first-row");
      const text = await page.locator(".empty").innerText();
      const problems = [];
      if (!/No files yet/.test(text)) problems.push(`missing Local queue empty state: ${JSON.stringify(text)}`);
      if (!/drop files into the Processing folder\./.test(text)) {
        problems.push("Local queue empty state did not give the direct Processing action");
      }
      if (/Power Automate|SharePoint/i.test(text)) {
        problems.push(`Local queue empty state mentioned unrelated intake: ${JSON.stringify(text)}`);
      }
      return problems;
    },
  },
  {
    name: "the download action describes only the missing optional backup model",
    async run(page) {
      await boot(page, "optional-backup-model");
      await page.click('nav button[data-v="settings"]');
      const button = await page.locator("#download-models-button").innerText();
      const text = await page.locator(".model-download").innerText();
      const problems = [];
      if (button.trim() !== "Download optional backup model (~1.8 GB)") {
        problems.push(`optional backup action said ${JSON.stringify(button)}`);
      }
      if (!/everyday model is already installed/i.test(text)) {
        problems.push("optional backup explanation does not say the everyday model is installed");
      }
      return problems;
    },
  },
  {
    name: "Start is disabled with a visible on-screen reason that works",
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
      // On a new computer there is no readiness control worth focusing yet:
      // the next truthful action is the first missing setup folder.
      const label = (await page.locator("#start-hint").textContent()).trim();
      if (label !== "finish setup below") {
        problems.push(`the first-run hint said ${JSON.stringify(label)}`);
      }
      await page.click("#start-hint");
      await page.waitForTimeout(400);
      const focused = await page.evaluate(() => document.activeElement?.id ?? "");
      if (focused !== "") {
        problems.push(`the setup hint focused an unexpected id ${JSON.stringify(focused)}`);
      }
      const focusedName = await page.evaluate(() => document.activeElement?.getAttribute("name") ?? "");
      if (focusedName !== "processing_dir") {
        problems.push(`clicking the hint left focus on ${JSON.stringify(focusedName)}`);
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
  {
    name: "navigation keeps a pending approval undoable",
    async run(page) {
      await boot(page, "review");
      const card = page.getByRole("article", { name: "Review IMG_20260214_113355.jpg" });
      const sha = await card.getAttribute("data-sha");
      await card.locator('[name="date"]').fill("2026-02-14");
      await card.locator('[name="subject"]').fill("Riverside lease agreement");
      await card.locator('[name="description"]').fill(
        "Signed lease agreement for the Riverside unit between Contoso and A. Patel."
      );
      await card.locator('[data-act="approve"]').click();
      await page.getByRole("tab", { name: "Settings" }).click();
      await page.getByRole("heading", { name: "Folders" }).waitFor({ state: "visible" });
      await page.getByRole("tab", { name: /Needs Review/ }).click();
      const returned = page.locator(`[data-sha="${sha}"]`);
      await returned.waitFor({ state: "attached" });
      const problems = [];
      if (!(await page.locator("#pending-approval-tray").isVisible())) {
        problems.push("pending approval disappeared on navigation");
      }
      if (!(await returned.locator(".undo-strip").isVisible())) {
        problems.push("the parked card did not return with its Undo strip");
      }
      if (await returned.locator("form").isVisible()) {
        problems.push("the parked card returned as an editable form");
      }
      await page.locator("#pending-approval-tray").getByRole("button", { name: "Undo" }).click();
      await page.waitForTimeout(300);
      const called = await page.evaluate(() =>
        window.__harness.invocations.some((i) => i.cmd === "resubmit" || i.cmd === "approve_job")
      );
      if (called) problems.push("Undo after navigation still filed the document");
      return problems;
    },
  },
  {
    name: "evidence read failures are distinct and retryable",
    async run(page) {
      await boot(page, "review-evidence-retry", { disableLazyEvidence: true });
      const card = page.getByRole("article", { name: "Review IMG_20260214_113355.jpg" });
      await card.getByRole("button", { name: "Document text" }).click();
      const failure = card.locator(".evidence-error");
      await failure.waitFor({ state: "visible" });
      const problems = [];
      if (!/could not read the saved text/i.test(await failure.innerText())) {
        problems.push("the read failure was not identified as a read failure");
      }
      if (/no saved text/i.test(await failure.innerText())) {
        problems.push("the read failure was presented as missing evidence");
      }
      const before = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "get_evidence").length
      );
      await failure.getByRole("button", { name: "Try again" }).click();
      const evidence = card.locator("pre.evidence");
      await evidence.waitFor({ state: "visible" });
      if (!/Tenancy Agreement/.test(await evidence.innerText())) {
        problems.push("Retry did not restore the saved evidence");
      }
      const after = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "get_evidence").length
      );
      if (after <= before) problems.push("Retry did not issue a fresh evidence read");

      const missing = page.getByRole("article", { name: "Review payroll-run-locked.pdf" });
      await missing.getByRole("button", { name: "Document text" }).click();
      await missing.locator("pre.evidence").waitFor({ state: "visible" });
      if (!/no saved text/i.test(await missing.locator("pre.evidence").innerText())) {
        problems.push("a missing evidence file did not retain its distinct message");
      }
      return problems;
    },
  },
  {
    name: "reviews can be filtered by reason and ordered oldest first",
    async run(page) {
      await boot(page, "review-reasons");
      const problems = [];
      const filter = page.locator("#review-reason-filter");
      const order = page.locator("#review-order");
      if (!(await filter.isVisible()) || !(await order.isVisible())) {
        problems.push("review reason filter or ordering control is missing");
        return problems;
      }
      if ((await page.locator(".card").count()) !== 25) {
        problems.push("the unfiltered review screen eagerly rendered more than its first 25 cards");
      }
      const values = await filter.locator("option").evaluateAll((options) =>
        options.map((option) => option.value)
      );
      if (!values.includes("ENCRYPTED")) {
        problems.push("the reason filter omitted a result beyond the first page");
      }
      await filter.selectOption("ENCRYPTED");
      await order.selectOption("oldest");
      await page.waitForTimeout(400);
      if ((await page.locator(".card").count()) !== 1) problems.push("reason filter did not use the complete review result set");
      if (!/reason-29\.pdf/.test(await page.locator(".card").innerText())) {
        problems.push("oldest ordering did not render the filtered result");
      }
      const footer = await page.locator("#review-foot").innerText();
      if (!/Showing all 1 file matching The file is password protected, oldest first\./.test(footer)) {
        problems.push(`filtered review footer was not honest: ${JSON.stringify(footer)}`);
      }
      return problems;
    },
  },
  {
    // The chip exists so a background event cannot destroy typing. It used to
    // do exactly that itself: renderOnce cleared the keyed card map and
    // replaced #content wholesale, rebuilding every card from the ledger.
    name: "the refresh chip never discards an in-progress correction",
    async run(page) {
      await boot(page, "review");
      const sha = await page.locator(".card").nth(1).getAttribute("data-sha");
      const card = page.locator(`[data-sha="${sha}"]`);
      const subject = card.locator('[name="subject"]');
      const description = card.locator('[name="description"]');
      await subject.fill("Board minutes March 2026");
      await description.fill("Board minutes for the March 2026 Riverside committee meeting.");

      // Some other flagged file, off-screen, changes — which during a backfill
      // happens continuously.
      await page.evaluate(() => {
        window.__harness.emit("job-updated", {
          sha256: "3".repeat(64),
          original_path: "C:\\Processing\\other.pdf",
          original_name: "other.pdf",
          original_relpath: "other.pdf",
          ext: "pdf",
          state: "flagged",
          flag_reason: "UNREADABLE:all conversion attempts exhausted",
          quarantine_path: null,
          proposed_date: null,
          date_source: null,
          proposed_subject: null,
          description: null,
          final_filename: null,
          doc_type: null,
          soft_flags: null,
          created_at: "2026-07-28T09:10:00.000Z",
          updated_at: new Date().toISOString(),
        });
      });
      await page.waitForTimeout(400);

      const problems = [];
      if (!(await page.locator("#refresh-chip").isVisible())) {
        problems.push("the off-screen change was never offered as a refresh");
      }
      await page.click("#refresh-chip");
      await page.waitForTimeout(900);

      if ((await subject.inputValue()) !== "Board minutes March 2026") {
        problems.push(`refresh rewrote the subject to ${JSON.stringify(await subject.inputValue())}`);
      }
      if (!(await description.inputValue()).startsWith("Board minutes for the March")) {
        problems.push("refresh emptied the description");
      }
      if (!(await card.locator(".kept-note").isVisible())) {
        problems.push("nothing told the reviewer why that card did not refresh");
      }
      // The rest of the list must still have been rebuilt.
      if ((await page.locator(".card").count()) !== 4) {
        problems.push(`refresh produced ${await page.locator(".card").count()} cards`);
      }
      return problems;
    },
  },
  {
    // A full render while an approval is parked used to detach the card and
    // rebuild it as an editable form showing the OLD values: the Undo strip
    // vanished, the countdown kept running, and the commit then deleted the
    // rebuilt card's map entry while leaving its node on screen — an
    // un-removable card for a file Power Automate had already been handed.
    name: "a full re-render cannot strand a pending approval",
    async run(page) {
      await boot(page, "review");
      const sha = await page.locator(".card").first().getAttribute("data-sha");
      const card = page.locator(`[data-sha="${sha}"]`);
      await card.locator('[name="date"]').fill("2026-02-09");
      await card.locator('[name="subject"]').fill("Riverside lease agreement");
      await card
        .locator('[name="description"]')
        .fill("Signed lease agreement for the Riverside unit between Contoso and A. Patel.");
      await card.locator('[data-act="approve"]').click();
      await page.waitForTimeout(300);

      const problems = [];
      if (!(await card.locator(".undo-strip").isVisible())) problems.push("no undo strip appeared");

      // The run button always calls render(); so do the pager, the refresh
      // chip and the error-state retry.
      await page.click("#runbtn");
      await page.waitForTimeout(900);

      if (!(await card.locator(".undo-strip").isVisible())) {
        problems.push("the re-render took the Undo strip away while the countdown ran");
      }
      if (await card.locator("form").isVisible()) {
        problems.push("the parked card came back as an editable form");
      }
      const count = await page.locator(".card").count();
      if (count !== 4) problems.push(`the re-render produced ${count} cards`);

      await page.waitForTimeout(11_000);
      const calls = await page.evaluate(() =>
        window.__harness.invocations.filter((i) => i.cmd === "resubmit")
      );
      if (calls.length !== 1) problems.push(`resubmit fired ${calls.length} times`);
      if (calls[0]?.args?.subject !== "Riverside lease agreement") {
        problems.push(`resubmit sent ${JSON.stringify(calls[0]?.args)}`);
      }
      if ((await page.locator(`[data-sha="${sha}"]`).count()) !== 0) {
        problems.push("the filed card is still on screen after the commit");
      }
      return problems;
    },
  },
  {
    // Approve is deferred by ten seconds, so the commit lands long after the
    // reviewer has moved on. dropCard used to focus the next card's date input
    // unconditionally, yanking the caret out of the very card being typed in —
    // and a type=date input silently swallows everything typed into it.
    name: "a committing approval leaves the reviewer's caret alone",
    async run(page) {
      await boot(page, "review");
      const firstSha = await page.locator(".card").first().getAttribute("data-sha");
      const secondSha = await page.locator(".card").nth(1).getAttribute("data-sha");
      const first = page.locator(`[data-sha="${firstSha}"]`);
      const second = page.locator(`[data-sha="${secondSha}"]`);

      await first.locator('[name="date"]').fill("2026-02-09");
      await first.locator('[name="subject"]').fill("Riverside lease agreement");
      await first
        .locator('[name="description"]')
        .fill("Signed lease agreement for the Riverside unit between Contoso and A. Patel.");
      await first.locator('[data-act="approve"]').click();
      await page.waitForTimeout(200);

      // The reviewer carries on in the next card while the first counts down.
      await second.locator('[name="subject"]').fill("Board minutes");
      await page.keyboard.type(" March 2026");
      await page.waitForTimeout(11_000);

      const problems = [];
      const active = await page.evaluate(() => ({
        name: document.activeElement?.getAttribute("name") ?? "",
        sha: document.activeElement?.closest("[data-sha]")?.getAttribute("data-sha") ?? "",
      }));
      if (active.name !== "subject" || active.sha !== secondSha) {
        problems.push(`the commit moved focus to ${JSON.stringify(active)}`);
      }
      const typed = await second.locator('[name="subject"]').inputValue();
      if (typed !== "Board minutes March 2026") {
        problems.push(`typing landed somewhere else: ${JSON.stringify(typed)}`);
      }
      return problems;
    },
  },
  {
    // The flagged set shrinks as it is worked, so an offset walked forward over
    // it skips whatever was resolved on the previous page. At 250 files that
    // silently left half of them unreviewed with nothing on screen saying so.
    name: "every flagged file is reachable as the list is worked",
    async run(page) {
      await boot(page, "review-scale");
      const shasOnScreen = () =>
        page.evaluate(() =>
          Array.from(document.querySelectorAll(".card")).map((c) => c.dataset.sha)
        );
      const seen = new Set();
      const problems = [];
      for (let guard = 0; guard < 200; guard++) {
        let shas = await shasOnScreen();
        if (!shas.length) {
          // A refetch of the head may still be in flight.
          await page.waitForTimeout(800);
          shas = await shasOnScreen();
        }
        if (!shas.length) break;
        for (const sha of shas) seen.add(sha);
        // Two clicks: one accidental click must never retire a document.
        const button = page.locator(`[data-sha="${shas[0]}"] [data-act="dismiss"]`);
        await button.click();
        await button.click();
        await page.waitForTimeout(40);
      }
      if (seen.size !== 60) {
        problems.push(`only ${seen.size} of 60 flagged files were ever rendered`);
      }
      if (!(await page.locator(".empty").isVisible())) {
        problems.push("the list did not end on the empty state");
      }
      return problems;
    },
  },
  {
    // Errors never auto-dismiss, by design. Without a cap the column grew
    // upward off the top of the window — unreachable and unclosable — and the
    // toasts below it covered the header, so the primary control could not be
    // clicked at all.
    name: "a pile of errors never covers the Start button",
    async run(page) {
      await boot(page, "toasts");
      for (let i = 0; i < 12; i++) {
        await page.click("#runbtn");
        await page.waitForTimeout(80);
      }
      const problems = [];
      const visible = await page.locator(".toast:visible").count();
      if (visible > 3) problems.push(`${visible} toasts on screen at once`);
      const box = await page.locator(".toast-host").boundingBox();
      if (box.y < 0) problems.push(`the toast column runs off the top of the window (y=${box.y})`);
      if (!(await page.locator(".toast-more").isVisible())) {
        problems.push("the folded messages are not reachable");
      }
      const counts = await page.evaluate(() =>
        Array.from(document.querySelectorAll(".toast")).map((t) => Number(t.dataset.count))
      );
      const total = counts.reduce((a, b) => a + b, 0);
      if (total !== 12) problems.push(`the toasts account for ${total} of 12 failures`);
      if (counts.length > 4) problems.push(`${counts.length} toasts for 4 distinct messages`);
      const over = await page.evaluate(() => {
        const rect = document.getElementById("runbtn").getBoundingClientRect();
        const at = document.elementFromPoint(rect.x + rect.width / 2, rect.y + rect.height / 2);
        return at ? at.id || at.className : "nothing";
      });
      if (over !== "runbtn") problems.push(`${over} covers the Start button`);
      return problems;
    },
  },
  {
    // Nothing used to tick the activity bar: it repainted only when a
    // job-updated event arrived, so the stall line could appear only while the
    // pipeline was NOT stalled, and the cold-start line could never expire.
    name: "the activity bar goes stalled on the clock, with no events",
    async run(page) {
      // Drive a fake clock rather than sleeping. The threshold is
      // per_file_wall_clock_secs * 3 = 270s and the fixture stamps the job 262s
      // in, so a real-time version has to sit out an 8s margin and trust that a
      // headless page's setInterval is not being throttled — which it is, the
      // moment this suite shares a machine with a cargo build. Installing the
      // clock before boot() keeps the browser's Date.now() under our control;
      // the fixture's own timestamp comes from Node and is unaffected, which is
      // exactly the relationship the production code sees.
      await page.clock.install();
      await boot(page, "stalling");
      const problems = [];
      const early = await page.locator("#activity").innerText();
      if (!/Working on/.test(early)) {
        problems.push(`the bar read ${JSON.stringify(early)} before the threshold`);
      }
      const before = await page.evaluate(() => window.__harness.invocations.length);
      // No event is emitted here on purpose: crossing the threshold is the only
      // thing that may change the line. fastForward fires the intervening
      // timers, so this exercises the real ticker rather than skipping it.
      await page.clock.fastForward(30_000);
      const late = await page.locator("#activity").innerText();
      if (!/Stalled/.test(late)) {
        problems.push(`no stall indicator once the threshold passed: ${JSON.stringify(late)}`);
      }
      const after = await page.evaluate(() => window.__harness.invocations.length);
      if (after <= before) problems.push("nothing re-read the pipeline while it was running");
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
