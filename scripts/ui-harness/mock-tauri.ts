// Browser stand-in for the Tauri runtime, so `src/main.ts` — the real,
// unmodified frontend — can be rendered, screenshotted, and interacted with in
// plain Chromium. BackLog targets Windows and needs `convertd`, `llama-server`,
// and ~2.4 GB of GGUF weights to run for real, so without this there is no way
// to look at the UI on a Linux CI box or review a frontend change visually.
//
// `vite.harness.config.ts` aliases every `@tauri-apps/*` import to this file.
// Nothing here is bundled into the shipped app: the production build uses the
// root `vite.config.ts`, which has no aliases.
//
// The scenario is picked from `?scenario=` on the URL and defined in
// `fixtures.ts`, so a reviewer can jump straight to a specific state (empty
// first run, a busy queue, a review backlog, a failed preflight) instead of
// trying to reproduce it against a live pipeline.

import { SCENARIOS, type Scenario } from "./fixtures";

function currentScenario(): Scenario {
  const name = new URLSearchParams(location.search).get("scenario") ?? "ready";
  const found = SCENARIOS[name];
  if (!found) {
    // Loud rather than silent: a typo'd scenario name silently falling back to
    // the default is exactly how a reviewer ends up screenshotting the wrong
    // state and believing they checked something they did not.
    throw new Error(
      `unknown harness scenario '${name}'; known: ${Object.keys(SCENARIOS).join(", ")}`
    );
  }
  return found;
}

const scenario = currentScenario();
const listeners = new Map<string, Array<(e: { payload: unknown }) => void>>();

/** Push an event into the app exactly as the Tauri event bus would. */
export function emit(event: string, payload: unknown): void {
  for (const cb of listeners.get(event) ?? []) cb({ payload });
}

/** Test hook: what the app has invoked, in order, for behavioural assertions. */
export const invocations: Array<{ cmd: string; args: unknown }> = [];

export async function invoke<T>(cmd: string, args?: unknown): Promise<T> {
  invocations.push({ cmd, args });
  const handler = scenario.commands[cmd];
  if (handler === undefined) {
    throw new Error(`harness has no stub for Tauri command '${cmd}'`);
  }
  const value = typeof handler === "function" ? handler(args) : handler;
  if (value instanceof Error) throw value;
  return value as T;
}

export async function listen<T>(
  event: string,
  cb: (e: { payload: T }) => void
): Promise<() => void> {
  const list = listeners.get(event) ?? [];
  list.push(cb as (e: { payload: unknown }) => void);
  listeners.set(event, list);
  return () => {
    const remaining = (listeners.get(event) ?? []).filter((f) => f !== cb);
    listeners.set(event, remaining);
  };
}

// --- plugin-dialog ---------------------------------------------------------
export async function open(opts?: unknown): Promise<string | null> {
  // Folder selection is part of Settings behavior. Record the target/options
  // beside IPC calls so harness assertions can prove each visible Browse
  // control opens the directory picker rather than a file picker.
  invocations.push({ cmd: "open_dialog", args: opts });
  return scenario.pickedPath ?? null;
}

// --- plugin-updater --------------------------------------------------------
export async function check(): Promise<unknown> {
  return scenario.update ?? null;
}

// --- plugin-process --------------------------------------------------------
export async function relaunch(): Promise<void> {
  /* no-op in the browser */
}

// Expose the hooks so a Playwright script can drive live events and read back
// what the app called, without reaching into module internals.
declare global {
  interface Window {
    __harness: { emit: typeof emit; invocations: typeof invocations; scenario: string };
  }
}
window.__harness = {
  emit,
  invocations,
  scenario: new URLSearchParams(location.search).get("scenario") ?? "ready",
};
