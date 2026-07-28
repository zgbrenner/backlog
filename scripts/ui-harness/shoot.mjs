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
      // The app boots straight to Settings when it is unconfigured, so this
      // hint is read on the screen it used to send the user to — where
      // navigate() was a no-op and clicking it did nothing whatsoever.
      const label = (await page.locator("#start-hint").textContent()).trim();
      if (/in Settings$/.test(label)) {
        problems.push(`the hint says ${JSON.stringify(label)} while already on Settings`);
      }
      await page.click("#start-hint");
      await page.waitForTimeout(400);
      const focused = await page.evaluate(() => document.activeElement?.id ?? "");
      if (focused !== "preflight-button") {
        problems.push(`clicking the hint left focus on ${JSON.stringify(focused)}`);
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
      await boot(page, "stalling");
      const problems = [];
      const early = await page.locator("#activity").innerText();
      if (!/Working on/.test(early)) {
        problems.push(`the bar read ${JSON.stringify(early)} before the threshold`);
      }
      const before = await page.evaluate(() => window.__harness.invocations.length);
      // No event is emitted here on purpose. The threshold is
      // per_file_wall_clock_secs * 3 = 270s and the fixture starts 262s in.
      await page.waitForTimeout(12_000);
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
