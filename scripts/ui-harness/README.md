# Browser UI harness

Renders the **real, unmodified** frontend (`src/main.ts`, `src/styles.css`,
`index.html`) in plain Chromium by aliasing every `@tauri-apps/*` import to a
mock runtime.

BackLog targets Windows and needs `convertd`, `llama-server`, and ~2.4 GB of
GGUF weights before it will start. Without this harness there is no way to look
at the UI — or catch a frontend regression — on a Linux CI runner, on a
reviewer's machine, or in a code review. With it, both are a few seconds.

## Use it

```bash
npm run harness              # dev server at http://localhost:1421
npm run harness:shots        # PNG per scenario, light + dark, into dist-harness/shots/
```

Pick a state with `?scenario=`:

| Scenario | What it shows |
|---|---|
| `first-run` | Very first launch: nothing configured, nothing checked, no files |
| `blocked` | Preflight ran and failed — the "why can't I start?" state |
| `downloading` | Mid model download, driven through the real progress events |
| `ready` | Configured, preflight green, a real queue behind it |
| `review` | The Needs Review backlog — the screen a user spends time in |
| `review-detail` | A review card with the document text and the event trail open |
| `errors` | Backend down: every list command rejects |
| `loading` | Every read hangs: the first paint, before any data resolves |
| `toasts` | Three failures at once, to prove they stack rather than overlap |
| `update` | An update is waiting, exercising the banner |
| `running` / `paused` | A live backfill, and the state after a reload while paused |
| `scale` | A 5,000-file backfill, to expose problems that only appear at size |

An unknown scenario name throws rather than silently falling back, so a typo
cannot leave you screenshotting the wrong state and believing you checked it.

A scenario can declare `view: "flagged" | "settings" | "queue"`; the shoot
script clicks that real nav button after boot rather than the frontend growing a
test-only URL parameter.

## Shots

Two files per scenario per scheme:

- `NAME.SCHEME.png` — the app's real window size (1180x780, matching
  `tauri.conf.json`), i.e. what the user actually sees.
- `NAME.SCHEME.full.png` — written only when the view overflows. `main` is the
  scroll container rather than the document, so Playwright's `fullPage` still
  stops at the window edge; without this a reviewer would never see the Settings
  form beneath the Readiness panel.

## It is also a test

`harness:shots` exits non-zero when:

- any scenario logs a console error, fails a request, or throws;
- a scenario's light and dark renders are **byte-identical** (they were, for the
  whole life of this harness, because the stylesheet had no colour-scheme
  handling at all);
- any of the behavioural checks fail. Those cover the ways the review loop used
  to eat a reviewer's work and the states that are impossible to judge from a
  still image:
  - a `job-updated` event never rewrites or unfocuses a card being edited, and
    still offers the update through the refresh chip;
  - `model-download-progress` never touches the Settings form — the folder field
    keeps its value and its focus while the bar advances in place;
  - first run shows eleven neutral "Not checked" rows, not eleven red failures;
  - Start is disabled **with a visible on-screen reason** and no `title=`;
  - the queue's search box and state chips reach `list_jobs` as real arguments;
  - Approve parks on an undo timer and writes nothing until it elapses.

Run it before pushing a change to `src/`.

## Rules

- **Never** let the shipped bundle resolve `@tauri-apps/*` to the mock. The
  aliases live only in `vite.harness.config.ts`; `npm run build` and
  `tauri build` use the root `vite.config.ts` and are untouched.
- **Keep `fixtures.ts` honest.** Its shapes must match what the Rust commands in
  `src-tauri/src/lib.rs`, `preflight.rs` and `ledger.rs` actually return — down
  to the `RuntimeProblem` codes and the `detail`/`action` fields, because the UI
  branches on them. A drifting fixture is worse than no fixture: it shows a UI
  state that cannot exist. The list commands are functions rather than constants
  so `query`, state, `limit` and `offset` are really applied.
- Chromium is resolved from `PLAYWRIGHT_BROWSERS_PATH` when the image already
  stages one (CI images routinely pin a revision the npm package disagrees
  with), falling back to Playwright's own download. `BACKLOG_CHROMIUM`
  overrides.
