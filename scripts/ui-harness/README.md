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
| `ready` | Configured, preflight green, a real queue behind it |
| `review` | The Needs Review backlog — the screen a user actually spends time in |
| `errors` | Backend down: every list command rejects |
| `update` | An update is waiting, exercising the banner |
| `scale` | A 5,000-file backfill, to expose problems that only appear at size |

An unknown scenario name throws rather than silently falling back, so a typo
cannot leave you screenshotting the wrong state and believing you checked it.

## It is also a smoke test

`harness:shots` exits non-zero if any scenario logs a console error, fails a
request, or throws — so it catches a frontend that no longer boots, not just one
that looks wrong. Run it before pushing a change to `src/`.

## Rules

- **Never** let the shipped bundle resolve `@tauri-apps/*` to the mock. The
  aliases live only in `vite.harness.config.ts`; `npm run build` and
  `tauri build` use the root `vite.config.ts` and are untouched.
- **Keep `fixtures.ts` honest.** Its shapes must match what the Rust commands in
  `src-tauri/src/lib.rs` and `preflight.rs` actually return. A drifting fixture
  is worse than no fixture: it shows a UI state that cannot exist.
- Chromium is resolved from `PLAYWRIGHT_BROWSERS_PATH` when the image already
  stages one (CI images routinely pin a revision the npm package disagrees
  with), falling back to Playwright's own download. `BACKLOG_CHROMIUM`
  overrides.
