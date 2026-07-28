import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// ---------------------------------------------------------------------------
// Types. Every one of these mirrors a Rust type that crosses the IPC boundary;
// when a command's shape changes in src-tauri, it changes here and in
// scripts/ui-harness/fixtures.ts, or the harness starts showing a UI state
// that cannot exist.
// ---------------------------------------------------------------------------

// Mirrors src-tauri/src/ledger.rs::Job. This is also the `job-updated` event
// payload, which is what lets that event patch one node instead of forcing a
// re-read of the whole list.
type Job = {
  sha256: string;
  original_path: string;
  original_name: string;
  original_relpath: string | null;
  ext: string;
  state: string;
  flag_reason: string | null;
  quarantine_path: string | null;
  proposed_date: string | null;
  date_source: string | null;
  proposed_subject: string | null;
  description: string | null;
  final_filename: string | null;
  doc_type: string | null;
  soft_flags: string | null;
  created_at: string;
  updated_at: string;
};

// Mirrors src-tauri/src/ledger.rs::Event.
type LedgerEvent = {
  id: number;
  sha256: string;
  at: string;
  stage: string;
  detail: string;
};

// Mirrors src-tauri/src/config.rs::Config.
type Config = {
  processing_dir: string;
  outbox_dir: string;
  quarantine_dir: string;
  cache_dir: string;
  llama_port: number;
  slm_primary_gguf: string;
  slm_escalation_gguf: string;
  slm_parallel: number;
  evidence_token_budget: number;
  ettin_model_dir: string;
  convert_workers: number;
  sidecar_timeout_secs: number;
  manifest_emit_per_min: number;
  max_head_pages: number;
  max_tail_pages: number;
  max_filename_len: number;
  max_stage_attempts: number;
  per_file_wall_clock_secs: number;
  retain_cache: boolean;
  cache_ttl_days: number;
};

// Mirrors src-tauri/src/preflight.rs::{RuntimeProblem, ProblemAction}.
type ProblemAction = "create_folder" | "download_models";
type RuntimeProblem = {
  field: string;
  code: string;
  message: string;
  detail: string | null;
  severity: "error" | "warning";
  action: ProblemAction | null;
};

// Mirrors src-tauri/src/preflight.rs::RuntimeStatus.
type RuntimeStatus = {
  configured: boolean;
  checked: boolean;
  running: boolean;
  paused: boolean;
  processing_dir_ready: boolean;
  outbox_writable: boolean;
  quarantine_writable: boolean;
  cache_writable: boolean;
  sidecar_found: boolean;
  sidecar_ok: boolean;
  llama_server_found: boolean;
  llama_server_ok: boolean;
  grammar_found: boolean;
  primary_model_found: boolean;
  escalation_model_found: boolean;
  offline_runtime: boolean;
  processing_entry_count: number | null;
  processing_entry_count_capped: boolean;
  processing_sample: string[];
  checked_at: string | null;
  problems: RuntimeProblem[];
};

// Mirrors src-tauri/src/model_download.rs::DownloadProgress. `files_done`
// ranges 0..files_total while `current_file` is in flight; show
// `files_done + 1` for a 1-based counter.
type ModelDownloadProgress = {
  current_file: string;
  file_bytes_done: number;
  file_bytes_total: number;
  files_done: number;
  files_total: number;
  overall_percent: number;
};

// Mirrors src-tauri/src/model_download.rs::DownloadDone.
type ModelDownloadDone = { ok: boolean; error?: string | null };

// Mirrors src-tauri/src/lib.rs::Diagnostics (only the fields the UI shows).
type Diagnostics = {
  app_version: string;
  platform: string;
  sidecar_versions: Record<string, unknown>;
};

type ViewName = "queue" | "flagged" | "settings";

// ---------------------------------------------------------------------------
// Copy tables. The pipeline speaks in machine codes; an office worker does not.
// ---------------------------------------------------------------------------

// Boolean pass/fail checks surfaced in the Readiness panel, in display order.
// Mirrors RuntimeStatus::checks() so the panel and `summary()` never disagree.
const READINESS_CHECKS: Array<[label: string, key: keyof RuntimeStatus]> = [
  ["Processing folder is readable", "processing_dir_ready"],
  ["Outbox folder is writable", "outbox_writable"],
  ["Quarantine folder is writable", "quarantine_writable"],
  ["Working folder is writable", "cache_writable"],
  ["Document reader (convertd) is installed", "sidecar_found"],
  ["Document reader answers", "sidecar_ok"],
  ["Naming engine (llama-server) is installed", "llama_server_found"],
  ["Naming engine starts", "llama_server_ok"],
  ["Naming rules file is installed", "grammar_found"],
  ["Everyday model file is present", "primary_model_found"],
  ["Backup model file is present", "escalation_model_found"],
];

// The ledger's seven in-flight/terminal states in the user's language. The raw
// value still rides along in data-state so a support conversation can ask for
// it. Order matters: it is also the filter-chip order.
const STATE_LABEL: Record<string, string> = {
  ingested: "Queued",
  converted: "Reading",
  filtered: "Understanding",
  named: "Naming",
  validated: "Checking",
  emitted: "Done",
  flagged: "Needs review",
  dismissed: "Dismissed",
};
const JOB_STATES = Object.keys(STATE_LABEL);

const STATE_BADGE: Record<string, string> = {
  ingested: "b-wait", converted: "b-wait", filtered: "b-wait", named: "b-wait",
  validated: "b-wait", emitted: "b-ok", flagged: "b-flag", dismissed: "b-off",
};

/** Plain sentence + concrete next action for every code the pipeline can put
 *  in `flag_reason`, keyed on the part before ':'. The raw string stays behind
 *  a "Technical detail" disclosure — the reviewer is told what happened, the
 *  support call still gets the code. */
const REASON_COPY: Record<string, { title: string; why: string; next: string }> = {
  // --- routing / conversion (src-tauri/src/routing.rs, pipeline.rs) ---
  CORRUPT: {
    title: "The file is damaged",
    why: "BackLog could not read any content from this file — it is empty or the copy is broken.",
    next: "Find the original and drop a fresh copy into the Processing folder.",
  },
  UNSUPPORTED: {
    title: "BackLog cannot open this kind of file",
    why: "This file type is not one BackLog knows how to read.",
    next: "Save it as PDF, Word or an image and drop that into the Processing folder instead.",
  },
  UNSUPPORTED_TYPE: {
    title: "BackLog cannot open this kind of file",
    why: "This file type is not one BackLog knows how to read.",
    next: "Save it as PDF, Word or an image and drop that into the Processing folder instead.",
  },
  ENCRYPTED: {
    title: "The file is password protected",
    why: "BackLog was refused access to the contents because the file is locked with a password.",
    next: "Open it, remove the password, save it, and drop it back into the Processing folder.",
  },
  CONVERT_FAIL: {
    title: "Nothing readable came out",
    why: "BackLog opened the file but found no text in it at all.",
    next: "If it is a scan, re-scan it right way up at 300 dpi. Otherwise name it yourself below.",
  },
  UNREADABLE: {
    title: "The text could not be read",
    why: "BackLog tried every way it has of reading this file, including reading the scan as an "
      + "image, and still could not get usable text out of it.",
    next: "Open the file yourself and type the date and subject below, or re-scan it more clearly.",
  },
  SLM_FAIL: {
    title: "BackLog could not suggest a name it trusts",
    why: "BackLog read the document but could not propose a date and subject it could prove "
      + "against the text, so it refused to guess.",
    next: "Read the document text below and fill in the date and subject yourself.",
  },
  TIMEOUT: {
    title: "This file took too long",
    why: "BackLog gave up after spending its whole time budget on this one file. Very large or "
      + "very slow scans do this.",
    next: "Press Try again. If it happens twice, name it yourself below.",
  },
  CRASH_LOOP: {
    title: "This file kept failing at the same step",
    why: "BackLog tried this file several times and it stopped at the same step every time.",
    next: "Press Try again after restarting BackLog, or name it yourself below.",
  },
  RUNTIME_FAIL: {
    title: "Something on this computer failed",
    why: "The failure was in BackLog or the computer, not in your document — a folder went away, "
      + "a disk filled up, or a part of BackLog stopped answering.",
    next: "Check Settings, then press Try again.",
  },
  DISMISSED: {
    title: "You set this one aside",
    why: "This file was marked as not worth filing.",
    next: "Nothing to do. Press Try again if you changed your mind.",
  },
  // --- deterministic checker (src-tauri/core/src/checker.rs) ---
  BAD_DATE: {
    title: "The date was not a real date",
    why: "The date BackLog proposed is not a date that exists on the calendar.",
    next: "Type the date printed on the document below.",
  },
  DATE_OUT_OF_RANGE: {
    title: "The date was implausible",
    why: "The proposed date was before 1800 or more than a year in the future, which is almost "
      + "always a misread scan.",
    next: "Type the date printed on the document below.",
  },
  DATE_NOT_IN_EVIDENCE: {
    title: "The date is not in the document",
    why: "BackLog will not put a date on a file unless it can point at that date in the document "
      + "or the file's own properties. It could not.",
    next: "Open the document, find the date printed on it, and type it below.",
  },
  BAD_DATE_SOURCE: {
    title: "BackLog could not say where the date came from",
    why: "Every date has to be traceable to the document text or the file's properties.",
    next: "Type the date printed on the document below.",
  },
  BAD_SUBJECT: {
    title: "The suggested subject was not usable",
    why: "The subject was empty, generic (\"Scanned Document\"), or looked like an identifier "
      + "rather than a description.",
    next: "Write a short subject below — what this document is, in a few words.",
  },
  BAD_DESCRIPTION: {
    title: "The one-sentence description was not usable",
    why: "The description was too short, too long, more than one sentence, or just repeated the "
      + "subject.",
    next: "Write one sentence below saying what this document is and who it is from.",
  },
  TOO_LONG: {
    title: "The name came out too long",
    why: "Date plus subject exceeded the filename length this system allows.",
    next: "Write a shorter subject below.",
  },
};

const UNKNOWN_REASON = {
  title: "This file needs a person to look at it",
  why: "BackLog stopped short of naming this file and did not record a reason it can explain.",
  next: "Read the document text below and fill in the date and subject yourself.",
};

/** Soft flags are advisory notes the pipeline attaches to a file it DID name.
 *  They used to live only in a title= tooltip, which is invisible to both the
 *  keyboard and a screen reader. */
const SOFT_FLAG_COPY: Record<string, string> = {
  DUPLICATE_CONTENT: "The same document has already been filed under another name.",
  POSSIBLE_MULTIDOC: "This file may contain several documents scanned together.",
  SPAN_MISMATCH: "The suggested subject did not line up exactly with the document text.",
  SPAN_MISMATCH_PERSISTED: "The suggested subject still did not line up with the document text "
    + "after a second attempt.",
  DATE_FROM_FILE_MTIME: "No date was printed on the document, so the file's own date was used.",
  DATE_FROM_BODY: "The date was taken from the body of the document rather than its heading.",
  DATE_AMBIGUOUS_FORMAT: "The date could be read as either day/month or month/day.",
  DATE_IN_FUTURE: "The date on this document is in the future.",
  DATE_SOURCE_CORRECTED: "BackLog corrected where it said the date came from.",
  SUBJECT_UNGROUNDED: "The subject is not a phrase that appears in the document.",
  SUBJECT_DATE_STRIPPED: "A date was removed from the subject — it is already in the file name.",
  SUBJECT_EXT_STRIPPED: "A file extension was removed from the subject.",
  HUMAN_CORRECTED: "A person corrected this file's name by hand.",
};

/** The tuning defaults from src-tauri/src/config.rs::Config::default(), for
 *  "Reset to recommended". `convert_workers` is machine-dependent there
 *  (cores - 2, clamped 1..6); 4 is what a typical office laptop resolves to. */
const RECOMMENDED_TUNING: Record<string, number> = {
  convert_workers: 4,
  slm_parallel: 4,
  evidence_token_budget: 1500,
  manifest_emit_per_min: 0,
  max_stage_attempts: 3,
  per_file_wall_clock_secs: 90,
};

/** Expected model basenames. These duplicate model_download::PRIMARY_GGUF_NAME
 *  and ESCALATION_GGUF_NAME — see `cross_workstream_requests`: the honest fix
 *  is for the backend to ship them in a command so the label can never drift
 *  from what the downloader actually fetches again. */
const PRIMARY_GGUF_NAME = "Qwen3-0.6B-Q8_0.gguf";
const ESCALATION_GGUF_NAME = "Qwen3-1.7B-Q8_0.gguf";

const QUEUE_PAGE = 200;
const REVIEW_PAGE = 25;
/** How long Approve waits before it writes the manifest Power Automate eats.
 *  Nothing downstream can un-file a document, so the only truthful undo is one
 *  that happens before the write. */
const UNDO_SECONDS = 10;
/** Guidance shown under the review inputs. The Rust checker is the sole
 *  authority: for a HUMAN it deliberately does not enforce the word count
 *  (checker.rs gates that on Source::Model), so these counters advise and only
 *  the genuinely impossible cases disable Approve. */
const SUBJECT_WORDS = [2, 10] as const;
const DESCRIPTION_CHARS = [15, 200] as const;

// ---------------------------------------------------------------------------
// Small DOM helpers
// ---------------------------------------------------------------------------

const app = document.getElementById("app")!;
const toastHost = document.getElementById("toasts")!;

function el(html: string): HTMLElement {
  const t = document.createElement("template");
  t.innerHTML = html.trim();
  return t.content.firstElementChild as HTMLElement;
}

function esc(s: string | null | undefined): string {
  return (s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!)
  );
}

function q<T extends Element>(root: ParentNode, sel: string): T {
  return root.querySelector(sel) as T;
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 MB";
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function formatCount(n: number): string {
  return n.toLocaleString();
}

/** Short, unambiguous, and stable across locales-with-no-seconds. */
function formatWhen(iso: string | null | undefined): string {
  if (!iso) return "";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const age = Date.now() - t;
  if (age < 60_000) return "just now";
  if (age < 3_600_000) return `${Math.floor(age / 60_000)} min ago`;
  if (age < 86_400_000) return `${Math.floor(age / 3_600_000)} h ago`;
  return new Date(t).toLocaleDateString();
}

function formatDuration(hours: number): string {
  if (hours < 1) return `about ${Math.max(1, Math.round(hours * 60))} min left`;
  if (hours < 24) return `about ${Math.round(hours)} h left`;
  return `about ${Math.round(hours / 24)} days left`;
}

// ---------------------------------------------------------------------------
// Errors and toasts
// ---------------------------------------------------------------------------

/** Turn a backend string into something an office worker can act on, keeping
 *  the raw text for the disclosure. The backend's own preflight problems are
 *  already plain language (preflight.rs writes them that way); what arrives
 *  raw is the deterministic checker's Display form and the occasional OS
 *  error, and those are what this maps. */
function friendlyError(raw: string): { message: string; raw: string | null } {
  const text = raw.replace(/^Error:\s*/i, "").trim();
  const rules: Array<[RegExp, string]> = [
    [/^date '.*' is not a valid calendar date/i,
      "That is not a date on the calendar. Use the date picker, or type it as YYYY-MM-DD."],
    [/outside plausible range/i,
      "That date is too far in the past or the future for BackLog to accept."],
    [/not present in document evidence/i,
      "BackLog could not find that date in the document. Check the date printed on it."],
    [/^subject invalid:/i,
      "That subject cannot be used in a file name. Try a few plain words describing the document."],
    [/^description invalid:/i,
      "The description has to be one sentence, ending in a full stop, "
      + `between ${DESCRIPTION_CHARS[0]} and ${DESCRIPTION_CHARS[1]} characters.`],
    [/composed filename too long/i,
      "Date plus subject is too long for a file name. Try a shorter subject."],
    [/no longer flagged|already moved on|already been dismissed/i,
      "This file has already moved on — refresh the list to see where it went."],
    [/unknown job|no record of that file/i,
      "BackLog no longer has a record of that file. Refresh the list."],
    [/locked by another process|database is locked/i,
      "BackLog's record of processed files is being used by something else. Close any other copy "
      + "of BackLog — check the notification area — and try again."],
  ];
  for (const [pattern, message] of rules) {
    if (pattern.test(text)) return { message, raw: text };
  }
  // Anything that reads like a machine talking gets a plain wrapper rather
  // than being dumped at the user verbatim.
  if (/os error|\(code \d+\)|panicked|Traceback|errno/i.test(text)) {
    return { message: "Something on this computer went wrong. The technical detail is below.", raw: text };
  }
  return { message: text, raw: null };
}

type ToastOptions = {
  kind?: "error" | "success" | "info";
  raw?: string | null;
  /** A single labelled recovery the toast can offer. */
  action?: { label: string; run: () => void };
  /** Errors never auto-dismiss: the user has to be able to read and copy them. */
  autoDismissMs?: number;
};

/** How many toasts stay on screen. Errors never auto-dismiss, so without a cap
 *  the column grows upward past the top of the window — where nothing can be
 *  read, scrolled to or closed — and the pile ends up covering the header,
 *  including the Start button it is complaining about. */
const MAX_VISIBLE_TOASTS = 3;
let toastsExpanded = false;

function toastKey(kind: string, message: string, raw: string | null | undefined): string {
  return `${kind} ${message} ${raw ?? ""}`;
}

function dismissToast(toast: HTMLElement): void {
  toast.remove();
  trimToasts();
}

/** Keep at most MAX_VISIBLE_TOASTS on screen and fold the rest behind one row.
 *  The oldest are the ones that get folded: the newest message is the one the
 *  user's last action produced. */
function trimToasts(): void {
  const toasts = Array.from(toastHost.querySelectorAll<HTMLElement>(".toast"));
  const folded = toastsExpanded ? 0 : Math.max(0, toasts.length - MAX_VISIBLE_TOASTS);
  toasts.forEach((toast, i) => {
    toast.hidden = i < folded;
  });
  let more = toastHost.querySelector<HTMLElement>(".toast-more");
  if (folded === 0) {
    more?.remove();
    if (!toasts.length) toastsExpanded = false;
    return;
  }
  if (!more) {
    more = el(`
      <div class="toast-more">
        <button type="button" class="toast-more-show"></button>
        <button type="button" class="toast-clear">Clear all</button>
      </div>`);
    q<HTMLElement>(more, ".toast-more-show").addEventListener("click", () => {
      toastsExpanded = true;
      trimToasts();
    });
    q<HTMLElement>(more, ".toast-clear").addEventListener("click", () => {
      toastHost.replaceChildren();
      toastsExpanded = false;
    });
  }
  // Always first, so the fold indicator sits above the messages it hides.
  toastHost.prepend(more);
  q<HTMLElement>(more, ".toast-more-show").textContent =
    `Show ${formatCount(folded)} earlier message${folded === 1 ? "" : "s"}`;
}

function showToast(message: string, opts: ToastOptions = {}): void {
  const kind = opts.kind ?? "error";
  const key = toastKey(kind, message, opts.raw);
  // The same failure repeated — pressing Start on a machine that is still
  // misconfigured, a backfill hitting the same locked ledger on every file —
  // is one message that happened N times, not N messages.
  const existing = toastHost.querySelector<HTMLElement>(`.toast[data-key="${CSS.escape(key)}"]`);
  if (existing) {
    const count = Number(existing.dataset.count ?? "1") + 1;
    existing.dataset.count = String(count);
    const badge = q<HTMLElement>(existing, ".toast-count");
    badge.textContent = `×${count}`;
    badge.hidden = false;
    existing.hidden = false;
    trimToasts();
    return;
  }
  const toast = el(
    `<div class="toast ${kind}" role="${kind === "error" ? "alert" : "status"}" data-count="1">
       <div class="toast-body"></div>
       <span class="toast-count" hidden></span>
       <button type="button" class="toast-close" aria-label="Dismiss this message">&times;</button>
     </div>`
  );
  toast.dataset.key = key;
  q<HTMLElement>(toast, ".toast-body").textContent = message;
  if (opts.raw) {
    const details = el(`<details><summary>Technical details</summary><pre class="raw"></pre></details>`);
    q<HTMLElement>(details, ".raw").textContent = opts.raw;
    toast.appendChild(details);
  }
  if (opts.action) {
    const button = el(`<button type="button" class="toast-action"></button>`);
    button.textContent = opts.action.label;
    button.addEventListener("click", () => {
      opts.action!.run();
      dismissToast(toast);
    });
    toast.appendChild(button);
  }
  q<HTMLElement>(toast, ".toast-close").addEventListener("click", () => dismissToast(toast));
  toastHost.appendChild(toast);
  trimToasts();
  if (opts.autoDismissMs) setTimeout(() => dismissToast(toast), opts.autoDismissMs);
}

function showError(raw: unknown, action?: ToastOptions["action"]): void {
  const { message, raw: detail } = friendlyError(String(raw));
  showToast(message, { kind: "error", raw: detail, action });
}

function showSuccess(message: string, action?: ToastOptions["action"]): void {
  showToast(message, { kind: "success", action, autoDismissMs: action ? undefined : 5000 });
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

type Theme = "system" | "light" | "dark";
const THEME_KEY = "backlog.theme";
let theme: Theme = "system";

function applyTheme(next: Theme): void {
  theme = next;
  const root = document.documentElement;
  if (next === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", next);
  try {
    localStorage.setItem(THEME_KEY, next);
  } catch {
    // Private/blocked storage: the choice just does not survive a restart.
  }
  const button = document.getElementById("theme-toggle");
  if (button) {
    button.textContent = { system: "Theme: System", light: "Theme: Light", dark: "Theme: Dark" }[next];
  }
}

function loadTheme(): void {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(THEME_KEY);
  } catch {
    stored = null;
  }
  applyTheme(stored === "light" || stored === "dark" ? stored : "system");
}

// ---------------------------------------------------------------------------
// Module state
// ---------------------------------------------------------------------------

function uncheckedRuntime(): RuntimeStatus {
  return {
    configured: false, checked: false, running: false, paused: false,
    processing_dir_ready: false, outbox_writable: false, quarantine_writable: false,
    cache_writable: false, sidecar_found: false, sidecar_ok: false,
    llama_server_found: false, llama_server_ok: false, grammar_found: false,
    primary_model_found: false, escalation_model_found: false, offline_runtime: true,
    processing_entry_count: null, processing_entry_count_capped: false,
    processing_sample: [], checked_at: null, problems: [],
  };
}

let cfg: Config | null = null;
let runtime: RuntimeStatus = uncheckedRuntime();
let view: ViewName = "queue";
let stats: Record<string, number> = {};

let queueQuery = "";
let queueState: string | null = null;
let queuePage = 0;

let modelsDownloading = false;
let modelDownloadProgress: ModelDownloadProgress | null = null;
let diagnostics: Diagnostics | null = null;
let diagnosticsRequested = false;
let diagnosticsError = false;

/** Files the pipeline touched that the current view is not showing. Surfaced
 *  as a chip the reviewer presses, never as a re-render underneath them. */
let pendingChanges = 0;
/** Newest non-terminal job seen, for "Working on:". Fed by the event stream so
 *  it costs no extra IPC. */
let activeJob: Job | null = null;
/** Baseline set when Start succeeds, so "starting the naming engine" can end
 *  the moment the first file gets past it rather than on a fixed timer. */
let coldStart: { at: number; namedBaseline: number } | null = null;

// Self-update (checked once at startup; see checkForUpdates below).
let pendingUpdate: Update | null = null;
let updateDismissed = false;
let updateStatus: "idle" | "downloading" | "installing" | "error" = "idle";
let updateError: string | null = null;
let updateDownloadedBytes = 0;
let updateTotalBytes = 0;

// ---------------------------------------------------------------------------
// Backend wrappers
// ---------------------------------------------------------------------------

/** Refreshes the in-memory runtime status. `live` runs the full machine check;
 *  otherwise this re-reads whatever the backend last cached. On failure the
 *  previous status is kept rather than reset, so a transient IPC hiccup can't
 *  make a Ready panel flash back to unchecked.
 *
 *  This is also the ONLY place running/paused come from. They used to be
 *  module-local mirrors that nothing ever synced, so reloading the webview
 *  while paused showed "Start" over a pipeline that was already up. */
async function refreshRuntime(live: boolean): Promise<void> {
  try {
    runtime = await invoke<RuntimeStatus>(live ? "run_preflight" : "get_runtime_status");
  } catch (e) {
    showError(e);
  }
}

async function loadStats(): Promise<void> {
  try {
    stats = await invoke<Record<string, number>>("get_stats");
  } catch {
    // The header degrades to the last known counts; a failed stats read is
    // never worth a toast during a backfill that emits thousands of them.
  }
}

/** `get_stats` returns one key per ledger state plus `per_hour`, so the total
 *  has to be summed over the states rather than over the whole object. */
function totalFiles(): number {
  return JOB_STATES.reduce((sum, s) => sum + (stats[s] ?? 0), 0);
}

function resolvedFiles(): number {
  return (stats["emitted"] ?? 0) + (stats["flagged"] ?? 0) + (stats["dismissed"] ?? 0);
}

/** Tauri v2 sends command arguments camelCase by default. Both spellings are
 *  passed because the app cannot be run against a real Tauri host here and a
 *  silently-ignored filter would show the operator the whole backfill while
 *  claiming it is filtered; unknown keys are ignored by the arg deserializer. */
function jobListArgs(extra: Record<string, unknown>): Record<string, unknown> {
  const state = queueState;
  return { query: queueQuery || null, jobState: state, job_state: state, ...extra };
}

// ---------------------------------------------------------------------------
// Shell: built once, patched in place. A full teardown of the header on every
// event is what used to destroy focus, scroll position and half-typed
// corrections.
// ---------------------------------------------------------------------------

function ensureShell(): void {
  if (document.getElementById("content")) return;
  app.replaceChildren(el(`
    <div class="shell">
      <header>
        <div class="brand">Back<span>Log</span></div>
        <nav role="tablist" aria-label="Views">
          <button type="button" role="tab" id="tab-queue" data-v="queue"
            aria-selected="false" aria-controls="content">Queue</button>
          <button type="button" role="tab" id="tab-flagged" data-v="flagged"
            aria-selected="false" aria-controls="content">Needs Review<span id="flagged-pill"></span></button>
          <button type="button" role="tab" id="tab-settings" data-v="settings"
            aria-selected="false" aria-controls="content">Settings</button>
        </nav>
        <div class="run">
          <button type="button" id="theme-toggle" class="theme-toggle">Theme: System</button>
          <span id="readiness-chip" class="readiness-chip unknown" aria-live="polite">Not checked</span>
          <span id="stats" class="stats" aria-live="polite"></span>
          <button type="button" id="runbtn">Start</button>
        </div>
      </header>
      <div id="update-slot"></div>
      <div id="activity" class="activity-bar" role="status" aria-live="polite" hidden></div>
      <main id="content" role="tabpanel" tabindex="-1"></main>
    </div>`));

  app.querySelectorAll<HTMLElement>("nav button").forEach((b) =>
    b.addEventListener("click", () => navigate(b.dataset.v as ViewName))
  );
  // Arrow keys move between tabs, which is what makes role=tablist honest.
  q<HTMLElement>(app, "nav").addEventListener("keydown", (ev) => {
    const key = (ev as KeyboardEvent).key;
    if (key !== "ArrowRight" && key !== "ArrowLeft") return;
    const order: ViewName[] = ["queue", "flagged", "settings"];
    const at = order.indexOf(view);
    const next = order[(at + (key === "ArrowRight" ? 1 : order.length - 1)) % order.length];
    navigate(next);
    document.getElementById(`tab-${next}`)?.focus();
    ev.preventDefault();
  });
  document.getElementById("runbtn")!.addEventListener("click", onRunButton);
  document.getElementById("theme-toggle")!.addEventListener("click", () => {
    applyTheme(theme === "system" ? "light" : theme === "light" ? "dark" : "system");
  });
  // The "why is Start greyed out?" hint lives in the activity bar, which is
  // repainted wholesale, so its click is delegated from the bar itself.
  document.getElementById("activity")!.addEventListener("click", (ev) => {
    if ((ev.target as HTMLElement).id === "start-hint") void goToPreflight();
  });
  applyTheme(theme);
}

function navigate(next: ViewName): Promise<void> {
  if (next === view) return Promise.resolve();
  // Anything parked on an undo timer commits now rather than being abandoned
  // on a screen the user has left.
  flushPendingApprovals();
  view = next;
  pendingChanges = 0;
  // The convertd probe behind get_diagnostics is only worth paying for once
  // the user is on the screen that shows what it returns.
  if (next === "settings") void loadDiagnostics();
  return render();
}

/** What the "why can't I start?" hint actually has to do. The app boots
 *  straight to Settings when it is unconfigured, which is where a first-run
 *  user meets this hint — so navigate("settings") is a no-op there and the
 *  link that replaced a never-rendering tooltip was itself inert in exactly
 *  the state it was written for. Take the user to the control, every time. */
async function goToPreflight(): Promise<void> {
  await navigate("settings");
  const button = document.getElementById("preflight-button");
  if (!button) return;
  button.scrollIntoView({ block: "center" });
  button.focus();
}

function paintHeader(): void {
  const running = runtime.running;
  const paused = runtime.paused;

  // Roving tabindex: Tab reaches the tablist once, then the arrow keys move
  // between tabs — the pattern role=tablist actually promises.
  for (const name of ["queue", "flagged", "settings"] as ViewName[]) {
    const tab = document.getElementById(`tab-${name}`);
    tab?.setAttribute("aria-selected", String(name === view));
    tab?.setAttribute("tabindex", name === view ? "0" : "-1");
  }
  document.getElementById("content")!.setAttribute("aria-labelledby", `tab-${view}`);
  const pill = document.getElementById("flagged-pill")!;
  const flagged = stats["flagged"] ?? 0;
  pill.className = flagged ? "pill" : "";
  pill.textContent = flagged ? formatCount(flagged) : "";

  const chip = document.getElementById("readiness-chip")!;
  const chipState = runtime.configured ? "ready" : runtime.checked ? "blocked" : "unknown";
  chip.className = `readiness-chip ${chipState}`;
  chip.textContent = { ready: "Ready", blocked: "Blocked", unknown: "Not checked" }[chipState];

  document.getElementById("stats")!.textContent =
    `${formatCount(totalFiles())} files · ${formatCount(stats["emitted"] ?? 0)} done`
    + ` · ${formatCount(flagged)} need review`;

  const runbtn = document.getElementById("runbtn") as HTMLButtonElement;
  runbtn.className = running ? (paused ? "paused" : "live") : "";
  runbtn.textContent = running ? (paused ? "Resume" : "Pause") : "Start";
  runbtn.disabled = !running && !runtime.configured;
  // No title= here on purpose: a disabled control swallows pointer events in
  // WebView2, so the tooltip that used to be the ONLY explanation never
  // rendered at all. The reason goes in the activity bar instead, where it is
  // a full-width sentence with a button in it.
  runbtn.removeAttribute("title");

  paintUpdateBanner();
  paintActivity();
}

/** Files/min, an ETA, what is being worked on right now, and whether the
 *  naming engine is still warming up. The only global indicator used to be
 *  three integers that do not move for the first ninety seconds. */
function paintActivity(): void {
  const bar = document.getElementById("activity");
  if (!bar) return;
  const parts: string[] = [];
  const perHour = stats["per_hour"] ?? 0;
  const total = totalFiles();
  const remaining = Math.max(0, total - resolvedFiles());

  if (!runtime.running && !runtime.configured) {
    // "…in Settings" is wrong when the user is already looking at Settings,
    // which on an unconfigured install is where the app boots.
    const here = view === "settings";
    const label = runtime.checked
      ? (here ? "fix what's listed below" : "fix what Settings lists")
      : (here ? "check this computer" : "check this computer in Settings");
    parts.push(`<span class="blocked-note">BackLog can't start yet — `
      + `<button type="button" id="start-hint" class="start-hint">${label}</button>.</span>`);
  }

  if (runtime.running && coldStart && (stats["named"] ?? 0) + (stats["validated"] ?? 0)
    + (stats["emitted"] ?? 0) <= coldStart.namedBaseline && Date.now() - coldStart.at < 180_000) {
    parts.push(`<span class="coldstart"><span class="spinner-dot"></span>Starting the naming `
      + `engine — the first file can take up to a minute and a half.</span>`);
  } else if (activeJob && runtime.running && !runtime.paused) {
    const stalledAfter = (cfg?.per_file_wall_clock_secs ?? 90) * 3 * 1000;
    const age = Date.now() - Date.parse(activeJob.updated_at);
    parts.push(Number.isFinite(age) && age > stalledAfter
      ? `<span class="warnish">Stalled on <b>${esc(activeJob.original_name)}</b> for `
        + `${Math.round(age / 60_000)} min.</span>`
      : `<span class="now"><span class="spinner-dot"></span>Working on: `
        + `<b>${esc(activeJob.original_name)}</b></span>`);
  } else if (runtime.paused) {
    parts.push(`<span class="warnish">Paused.</span>`);
  }

  // Under ~10 files the rate is noise, and an ETA built on noise is worse than
  // no ETA on a job someone is deciding whether to leave running overnight.
  // A rate is only meaningful while the pipeline is actually running: the
  // ledger keeps reporting the last hour's throughput after Pause.
  if (runtime.running && !runtime.paused && perHour >= 10) {
    parts.push(`<span>${(perHour / 60).toFixed(1)} files/min</span>`);
    if (remaining > 0) parts.push(`<span>${esc(formatDuration(remaining / perHour))}</span>`);
  }

  bar.innerHTML = parts.join("");
  bar.hidden = parts.length === 0;
  syncActivityTicker();
}

/** Everything in the bar above is derived from wall-clock time — how long the
 *  current file has sat, whether the cold start has outlived its ninety
 *  seconds, how long the remainder will take — but the only thing that used to
 *  repaint it was an incoming job-updated event. So the stall line could only
 *  appear while the pipeline was NOT stalled, and "Starting the naming engine"
 *  stayed on screen forever if llama-server never came up. One slow tick while
 *  running evaluates them against the clock instead of against the event
 *  stream. */
let activityTicker = 0;
const ACTIVITY_TICK_MS = 5000;

function syncActivityTicker(): void {
  if (runtime.running && !activityTicker) {
    activityTicker = window.setInterval(() => {
      void loadStats().then(paintHeader);
    }, ACTIVITY_TICK_MS);
  } else if (!runtime.running && activityTicker) {
    window.clearInterval(activityTicker);
    activityTicker = 0;
  }
}

// ---------------------------------------------------------------------------
// Render orchestration
//
// One renderer runs at a time and the DOM is touched exactly once, after every
// await has resolved. render() used to await get_stats, clear the document,
// then await the list — so an older render could blank the window after a
// newer one had finished painting, and a resolved-late render appended into a
// node that was no longer in the document.
// ---------------------------------------------------------------------------

let renderSeq = 0;
let renderInFlight = false;
let renderAgain = false;

async function render(): Promise<void> {
  if (renderInFlight) {
    renderAgain = true;
    return;
  }
  renderInFlight = true;
  try {
    do {
      renderAgain = false;
      await renderOnce();
    } while (renderAgain);
  } finally {
    renderInFlight = false;
  }
}

type ViewData =
  | { kind: "queue"; jobs: Job[]; total: number; error?: string }
  | { kind: "flagged"; jobs: Job[]; total: number; error?: string }
  | { kind: "settings" };

async function renderOnce(): Promise<void> {
  const token = ++renderSeq;
  const wanted = view;
  ensureShell();

  // First paint only: the shell is up but no list has resolved yet. Without
  // this the window shows a header over a void while the ledger is read.
  const pane = document.getElementById("content")!;
  if (!pane.firstChild) {
    pane.replaceChildren(el(`<div class="empty" role="status">Loading…</div>`));
  }
  // Paint the chrome from what is already known before waiting on the ledger,
  // so a nav click highlights its tab at once instead of after a round trip.
  paintHeader();

  await loadStats();
  const data = await loadViewData(wanted);
  // Superseded (a nav click, an event, a button) — the newer render owns the
  // DOM and this one must not touch it.
  if (token !== renderSeq || wanted !== view) return;

  const content = document.getElementById("content")!;
  const scroll = content.scrollTop;
  const focus = captureFocus();

  // Cards holding work that has not been written yet — a half-typed
  // correction, a busy invoke, an approval counting down behind its Undo —
  // are carried across the rebuild instead of being reconstructed from the
  // ledger. The refresh chip exists precisely so a background event cannot
  // destroy a correction; a rebuild that discarded every card would make the
  // chip (and the run button, and the retry link) do exactly that instead.
  const held = new Map<string, ReviewCard>();
  for (const [sha, card] of reviewCards) {
    if (isHeldCard(sha, card)) held.set(sha, card);
  }

  queueRows.clear();
  reviewCards.clear();
  paintHeader();
  content.replaceChildren(...buildView(data, held));
  content.scrollTop = scroll;
  restoreFocus(focus);
}

/** A card that must survive a re-render: unsaved typing, an invoke in flight,
 *  or a deferred approval whose countdown and Undo are the only thing standing
 *  between the reviewer and an irreversible manifest. */
function isHeldCard(sha256: string, card: ReviewCard): boolean {
  return card.dirty || card.busy || pendingApprovals.has(sha256);
}

function requestRender(): void {
  void render();
}

async function loadViewData(wanted: ViewName): Promise<ViewData> {
  if (wanted === "settings") return { kind: "settings" };
  if (wanted === "queue") {
    try {
      const [jobs, total] = await Promise.all([
        invoke<Job[]>("list_jobs", jobListArgs({ limit: QUEUE_PAGE, offset: queuePage * QUEUE_PAGE })),
        invoke<number>("count_jobs", jobListArgs({})),
      ]);
      // Seed "Working on:" from the page we just fetched (ordered by
      // updated_at DESC), so the line is populated the moment the queue opens
      // rather than only after the next job-updated event arrives.
      const newest = jobs.find((j) => !TERMINAL_STATES.has(j.state));
      if (newest) activeJob = newest;
      return { kind: "queue", jobs, total };
    } catch (e) {
      return { kind: "queue", jobs: [], total: 0, error: String(e) };
    }
  }
  try {
    const [jobs, total] = await Promise.all([
      // Always the head of the queue: see buildReviewFoot for why this must
      // not be an offset that walks forward over a set the reviewer is
      // emptying as they go.
      invoke<Job[]>("list_flagged", { limit: REVIEW_PAGE, offset: 0 }),
      invoke<number>("count_jobs", { query: null, jobState: "flagged", job_state: "flagged" }),
    ]);
    return { kind: "flagged", jobs, total };
  } catch (e) {
    return { kind: "flagged", jobs: [], total: 0, error: String(e) };
  }
}

function buildView(data: ViewData, held: Map<string, ReviewCard>): Node[] {
  if (data.kind === "queue") {
    return buildQueue(data);
  }
  if (data.kind === "flagged") {
    return buildFlagged(data, held);
  }
  return buildSettings();
}

// --- focus preservation -----------------------------------------------------

type FocusSnapshot = { selector: string; start: number | null; end: number | null } | null;

function captureFocus(): FocusSnapshot {
  const active = document.activeElement;
  if (!(active instanceof HTMLElement)) return null;
  let selector: string | null = null;
  if (active.id) selector = `#${CSS.escape(active.id)}`;
  else {
    const card = active.closest<HTMLElement>("[data-sha]");
    const name = active.getAttribute("name");
    if (card && name) selector = `[data-sha="${CSS.escape(card.dataset.sha!)}"] [name="${name}"]`;
  }
  if (!selector) return null;
  const input = active instanceof HTMLInputElement ? active : null;
  return {
    selector,
    start: input && input.type !== "date" ? input.selectionStart : null,
    end: input && input.type !== "date" ? input.selectionEnd : null,
  };
}

function restoreFocus(snapshot: FocusSnapshot): void {
  if (!snapshot) return;
  const target = document.querySelector<HTMLElement>(snapshot.selector);
  if (!target) return;
  target.focus();
  if (target instanceof HTMLInputElement && snapshot.start !== null) {
    try {
      target.setSelectionRange(snapshot.start, snapshot.end ?? snapshot.start);
    } catch {
      // Input types that do not support selection ranges: nothing to restore.
    }
  }
}

// ---------------------------------------------------------------------------
// Update banner
// ---------------------------------------------------------------------------

function paintUpdateBanner(): void {
  const slot = document.getElementById("update-slot");
  if (!slot) return;
  if (!pendingUpdate || updateDismissed) {
    slot.replaceChildren();
    return;
  }
  const busy = updateStatus === "downloading" || updateStatus === "installing";
  const pct = updateTotalBytes > 0 ? Math.round((updateDownloadedBytes / updateTotalBytes) * 100) : null;
  const statusText =
    updateStatus === "installing"
      ? "Installing update, BackLog will restart…"
      : updateStatus === "downloading"
        ? `Downloading update…${pct !== null ? ` ${pct}%` : ""}`
        : `A new version (${pendingUpdate.version}) is available.`;
  const banner = el(`
    <div class="update-banner" role="status">
      <span class="update-text"></span>
      <div class="update-actions">
        ${busy ? "" : `<button type="button" id="update-now-button">Update now</button>
           <button type="button" id="update-dismiss-button" class="ghost">Later</button>`}
      </div>
      ${updateError ? `<div class="update-error"></div>` : ""}
    </div>`);
  q<HTMLElement>(banner, ".update-text").textContent = statusText;
  if (updateError) q<HTMLElement>(banner, ".update-error").textContent = updateError;
  banner.querySelector("#update-now-button")?.addEventListener("click", onUpdateNowClick);
  banner.querySelector("#update-dismiss-button")?.addEventListener("click", () => {
    updateDismissed = true;
    paintUpdateBanner();
  });
  slot.replaceChildren(banner);
}

/** Fire-and-forget check against the `latest.json` endpoint configured in
 *  tauri.conf.json. Must never block or break startup: no releases yet, no
 *  network, or a misbehaving endpoint all just leave the app quiet. */
async function checkForUpdates(announce = false): Promise<void> {
  try {
    const update = await check();
    if (update) {
      pendingUpdate = update;
      updateDismissed = false;
      paintUpdateBanner();
    } else if (announce) {
      showSuccess("BackLog is up to date.");
    }
  } catch (e) {
    // Silent on startup; explicit when the user pressed the button.
    if (announce) showError(e);
  }
}

let updateProgressRenderQueued = false;
function queueUpdateProgressPaint(): void {
  // Chunk events fire many times a second for a multi-MB installer.
  if (updateProgressRenderQueued) return;
  updateProgressRenderQueued = true;
  setTimeout(() => {
    updateProgressRenderQueued = false;
    paintUpdateBanner();
  }, 200);
}

async function onUpdateNowClick(): Promise<void> {
  if (!pendingUpdate || updateStatus === "downloading" || updateStatus === "installing") return;
  updateStatus = "downloading";
  updateError = null;
  updateDownloadedBytes = 0;
  updateTotalBytes = 0;
  paintUpdateBanner();
  try {
    await pendingUpdate.downloadAndInstall((event) => {
      if (event.event === "Started") {
        updateTotalBytes = event.data.contentLength ?? 0;
        paintUpdateBanner();
      } else if (event.event === "Progress") {
        updateDownloadedBytes += event.data.chunkLength;
        queueUpdateProgressPaint();
      } else if (event.event === "Finished") {
        updateStatus = "installing";
        paintUpdateBanner();
      }
    });
    // The installer already ran; relaunch into the new version. If this throws
    // the update is already on disk, so surface it without re-arming.
    await relaunch();
  } catch (e) {
    updateStatus = "error";
    updateError = String(e);
    paintUpdateBanner();
  }
}

// ---------------------------------------------------------------------------
// Run button
// ---------------------------------------------------------------------------

async function onRunButton(): Promise<void> {
  const button = document.getElementById("runbtn") as HTMLButtonElement;
  button.disabled = true;
  try {
    if (!runtime.running) {
      // Snapshot before starting so the cold-start line can end on evidence
      // (a file got past the naming engine) rather than on a fixed timer.
      coldStart = {
        at: Date.now(),
        namedBaseline: (stats["named"] ?? 0) + (stats["validated"] ?? 0) + (stats["emitted"] ?? 0),
      };
      try {
        await invoke("start_pipeline");
      } catch (e) {
        coldStart = null;
        showError(e, { label: "Open Settings", run: () => void navigate("settings") });
        // Same reason navigate() does it: nothing parked on an undo timer may
        // be abandoned on a screen the user is being moved away from.
        flushPendingApprovals();
        view = "settings";
      }
    } else {
      await invoke("set_paused", { paused: !runtime.paused });
    }
  } catch (e) {
    showError(e);
  }
  // running/paused are ALWAYS re-read from the backend, which computes them
  // fresh from the live pipeline. Nothing in the frontend guesses them.
  await refreshRuntime(false);
  await render();
}

// ---------------------------------------------------------------------------
// Queue view
// ---------------------------------------------------------------------------

const queueRows = new Map<string, HTMLTableRowElement>();

function softFlagSentences(raw: string | null): string[] {
  if (!raw) return [];
  return raw.split(/[,;]\s*/).filter(Boolean).map((flag) => {
    const key = flag.split(":")[0];
    return SOFT_FLAG_COPY[flag] ?? SOFT_FLAG_COPY[key] ?? key.replace(/_/g, " ").toLowerCase();
  });
}

function fillQueueRow(row: HTMLTableRowElement, job: Job): void {
  row.dataset.sha = job.sha256;
  row.dataset.state = job.state;
  const notes = softFlagSentences(job.soft_flags);
  const noteId = `soft-${job.sha256.slice(0, 12)}`;
  row.innerHTML = `
    <td class="mono"><span class="name"></span></td>
    <td class="mono"><span class="final"></span></td>
    <td class="desc"></td>
    <td>
      <span class="badge ${STATE_BADGE[job.state] ?? "b-wait"}">${esc(STATE_LABEL[job.state] ?? job.state)}</span>
      ${notes.length
        ? `<button type="button" class="soft-btn" aria-expanded="false" aria-controls="${noteId}"
             aria-label="Notes about this file">!</button>`
        : ""}
      <span class="row-reason"></span>
      <span class="soft-note" id="${noteId}" hidden></span>
    </td>
    <td class="when"></td>`;
  // The original path is what answers "did my file go through?"; the sha256
  // that used to be here answers nothing a user can act on.
  const name = q<HTMLElement>(row, ".name");
  name.textContent = job.original_name;
  name.title = job.original_path;
  q<HTMLElement>(row, ".final").textContent = job.final_filename ?? "—";
  q<HTMLElement>(row, ".desc").textContent = job.description ?? "";
  const reason = q<HTMLElement>(row, ".row-reason");
  reason.textContent = job.state === "flagged" ? reasonCopy(job.flag_reason).title : "";
  q<HTMLElement>(row, ".when").textContent = formatWhen(job.updated_at);
  const note = q<HTMLElement>(row, ".soft-note");
  note.textContent = notes.join(" ");
  row.querySelector(".soft-btn")?.addEventListener("click", (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    note.hidden = !note.hidden;
    button.setAttribute("aria-expanded", String(!note.hidden));
  });
}

/** A read that failed, told the same way everywhere: what went wrong in plain
 *  words, the raw string one disclosure away, and a way back. */
function buildErrorState(heading: string, raw: string): HTMLElement {
  const { message, raw: detail } = friendlyError(raw);
  const box = el(`
    <div class="empty err-state">
      <strong></strong>
      <p class="msg"></p>
      <details class="tech"><summary>Technical detail</summary><code class="reason"></code></details>
      <p><button type="button" class="ghost retry">Try again</button></p>
    </div>`);
  q<HTMLElement>(box, "strong").textContent = heading;
  q<HTMLElement>(box, ".msg").textContent = message;
  q<HTMLElement>(box, ".reason").textContent = detail ?? raw;
  q<HTMLElement>(box, ".retry").addEventListener("click", requestRender);
  return box;
}

function buildQueue(data: { jobs: Job[]; total: number; error?: string }): Node[] {
  const nodes: Node[] = [buildQueueToolbar()];
  if (data.error) {
    nodes.push(buildErrorState("BackLog couldn't read the queue", data.error));
    return nodes;
  }
  if (!data.jobs.length) {
    const filtered = queueQuery !== "" || queueState !== null;
    nodes.push(el(filtered
      ? `<div class="empty"><strong>Nothing matches</strong>No file matches that search or filter.
         Clear them to see everything BackLog has processed.</div>`
      : `<div class="empty"><strong>No files yet</strong>Set your folders in Settings and check this
         computer, then drop files into the Processing folder — or let your SharePoint intake put
         them there.</div>`));
    return nodes;
  }
  const table = el(`
    <div class="table-wrap">
      <table>
        <thead><tr>
          <th scope="col">Original</th><th scope="col">New name</th>
          <th scope="col">Description</th><th scope="col">State</th><th scope="col">Updated</th>
        </tr></thead>
        <tbody></tbody>
      </table>
    </div>`);
  const body = q<HTMLTableSectionElement>(table, "tbody");
  for (const job of data.jobs) {
    const row = document.createElement("tr");
    fillQueueRow(row, job);
    queueRows.set(job.sha256, row);
    body.appendChild(row);
  }
  nodes.push(table);
  nodes.push(buildPager(data.jobs.length, data.total, queuePage, QUEUE_PAGE, (next) => {
    queuePage = next;
    requestRender();
  }));
  return nodes;
}

function buildQueueToolbar(): HTMLElement {
  const bar = el(`
    <div class="toolbar">
      <label class="sr-only" for="queue-search">Search file names</label>
      <input id="queue-search" class="search" type="search" placeholder="Search file names…"
        autocomplete="off">
      <div class="chip-row" role="group" aria-label="Filter by state"></div>
      <span class="grow"></span>
      <button type="button" id="refresh-chip" class="refresh-chip" hidden></button>
    </div>`);
  const search = q<HTMLInputElement>(bar, "#queue-search");
  // Seed from the live field, not from the committed query: the debounce fires
  // a render 250ms after a keystroke, and anything typed while that render is
  // in flight would otherwise be thrown away when the toolbar is rebuilt.
  const live = document.getElementById("queue-search") as HTMLInputElement | null;
  search.value = live ? live.value : queueQuery;
  let debounce = 0;
  search.addEventListener("input", () => {
    window.clearTimeout(debounce);
    debounce = window.setTimeout(() => {
      queueQuery = search.value.trim();
      queuePage = 0;
      requestRender();
    }, 250);
  });

  const chips = q<HTMLElement>(bar, ".chip-row");
  const add = (label: string, value: string | null) => {
    const chip = el(`<button type="button" class="filter-chip"></button>`);
    chip.textContent = label;
    chip.setAttribute("aria-pressed", String(queueState === value));
    chip.addEventListener("click", () => {
      queueState = queueState === value ? null : value;
      queuePage = 0;
      requestRender();
    });
    chips.appendChild(chip);
  };
  add("All", null);
  for (const state of JOB_STATES) add(STATE_LABEL[state], state);

  wireRefreshChip(q<HTMLElement>(bar, "#refresh-chip"));
  return bar;
}

function wireRefreshChip(chip: HTMLElement): void {
  chip.addEventListener("click", () => {
    pendingChanges = 0;
    requestRender();
  });
  paintChipInto(chip);
}

function paintChipInto(chip: HTMLElement): void {
  chip.hidden = pendingChanges === 0;
  chip.textContent = `${formatCount(pendingChanges)} file${pendingChanges === 1 ? "" : "s"} changed — Refresh`;
}

function paintRefreshChip(): void {
  const chip = document.getElementById("refresh-chip");
  if (chip) paintChipInto(chip);
}

function buildPager(
  shown: number,
  total: number,
  page: number,
  pageSize: number,
  goto: (next: number) => void
): HTMLElement {
  const first = page * pageSize + 1;
  const foot = el(`
    <div class="list-foot">
      <span class="count"></span>
      <div class="pager">
        <button type="button" class="prev">Previous</button>
        <button type="button" class="next">Next</button>
      </div>
    </div>`);
  q<HTMLElement>(foot, ".count").textContent =
    total > shown
      ? `Showing ${formatCount(first)}–${formatCount(first + shown - 1)} of ${formatCount(total)}`
      : `Showing all ${formatCount(total)}`;
  const prev = q<HTMLButtonElement>(foot, ".prev");
  const next = q<HTMLButtonElement>(foot, ".next");
  prev.disabled = page === 0;
  next.disabled = first + shown - 1 >= total;
  prev.addEventListener("click", () => goto(page - 1));
  next.addEventListener("click", () => goto(page + 1));
  return foot;
}

// ---------------------------------------------------------------------------
// Needs Review
// ---------------------------------------------------------------------------

type ReviewCard = {
  root: HTMLElement;
  job: Job;
  dirty: boolean;
  busy: boolean;
  patch(job: Job): void;
  markStale(message: string): void;
  markKept(): void;
};

const reviewCards = new Map<string, ReviewCard>();

function reasonCopy(raw: string | null): { title: string; why: string; next: string } {
  if (!raw) return UNKNOWN_REASON;
  return REASON_COPY[raw.split(":")[0].trim().toUpperCase()] ?? UNKNOWN_REASON;
}

function buildFlagged(
  data: { jobs: Job[]; total: number; error?: string },
  held: Map<string, ReviewCard>
): Node[] {
  const bar = el(`
    <div class="toolbar">
      <span class="grow"></span>
      <button type="button" id="refresh-chip" class="refresh-chip" hidden></button>
    </div>`);
  wireRefreshChip(q<HTMLElement>(bar, "#refresh-chip"));
  const nodes: Node[] = [bar];

  const cards: Node[] = [];
  const seen = new Set<string>();
  const keep = (sha256: string, card: ReviewCard) => {
    reviewCards.set(sha256, card);
    card.markKept();
    cards.push(card.root);
    seen.add(sha256);
  };
  for (const job of data.jobs) {
    const existing = held.get(job.sha256);
    if (existing) keep(job.sha256, existing);
    else {
      cards.push(buildReviewCard(job).root);
      seen.add(job.sha256);
    }
  }
  // A held card whose row is no longer in the fetched page still belongs on
  // screen: dropping it would strand an approval mid-countdown with its Undo
  // gone and its timer still running.
  for (const [sha256, card] of held) {
    if (!seen.has(sha256)) keep(sha256, card);
  }

  if (data.error) {
    nodes.push(buildErrorState("BackLog couldn't read the review queue", data.error));
    nodes.push(...cards);
    return nodes;
  }
  if (!cards.length) {
    nodes.push(el(`<div class="empty"><strong>Nothing needs review</strong>As it should be. Files
      BackLog cannot name confidently will appear here.</div>`));
    return nodes;
  }
  nodes.push(...cards);
  nodes.push(buildReviewFoot(cards.length, data.total));
  return nodes;
}

/** No pager here on purpose. The flagged set SHRINKS as it is worked: filing or
 *  setting aside a file removes its row, so an offset carried to the next page
 *  skips exactly as many rows as were resolved on this one — 250 files becomes
 *  ~125 reviewed and the rest never rendered on any page, while the Needs
 *  Review pill still counts them. The reviewer always works the head of the
 *  queue instead, and `dropCard` refetches it when the screen empties. */
function buildReviewFoot(shown: number, total: number): HTMLElement {
  const foot = el(`<div class="list-foot" id="review-foot"><span class="count"></span></div>`);
  paintReviewFootInto(foot, shown, total);
  return foot;
}

function paintReviewFootInto(foot: HTMLElement, shown: number, total: number): void {
  q<HTMLElement>(foot, ".count").textContent = total > shown
    ? `Showing ${formatCount(shown)} of ${formatCount(total)} files that need review — `
      + `as you file or set aside these, the rest move up.`
    : `Showing all ${formatCount(total)} file${total === 1 ? "" : "s"} that need review.`;
}

/** Keep the footer honest as cards leave the screen one at a time. */
function paintReviewFoot(): void {
  const foot = document.getElementById("review-foot");
  if (!foot) return;
  const shown = reviewCards.size;
  paintReviewFootInto(foot, shown, Math.max(shown, stats["flagged"] ?? shown));
}

function buildReviewCard(job: Job): ReviewCard {
  const copy = reasonCopy(job.flag_reason);
  const short = job.sha256.slice(0, 12);
  const root = el(`
    <article class="card" data-sha="${esc(job.sha256)}"
      aria-label="Review ${esc(job.original_name)}">
      <div class="card-head">
        <strong></strong>
        <span class="ext"></span>
      </div>
      <div class="why">
        <span class="why-title"></span>
        <p class="why-body"></p>
        <p class="why-next"></p>
        <details class="tech">
          <summary>Technical detail</summary>
          <code class="reason"></code>
        </details>
      </div>
      <form class="review-form" novalidate>
        <div class="fields">
          <label>Date
            <input type="date" name="date" required>
          </label>
          <label>Subject
            <input name="subject" placeholder="A few plain words" autocomplete="off"
              aria-describedby="c-sub-${short}">
            <span class="counter" id="c-sub-${short}"></span>
          </label>
          <div class="wide date-chips" hidden>
            <span class="lbl">Dates found in the document:</span>
          </div>
          <label class="wide">One-sentence description
            <input name="description" placeholder="One sentence, ending in a full stop."
              autocomplete="off" aria-describedby="c-desc-${short}">
            <span class="counter" id="c-desc-${short}"></span>
          </label>
        </div>
        <p class="blocker" hidden></p>
        <div class="card-actions">
          <button type="submit" class="primary" data-act="approve">Approve and file</button>
          <button type="button" class="ghost" data-act="reprocess">Try again</button>
          <button type="button" class="danger" data-act="dismiss">Can't fix this</button>
          <span class="spacer"></span>
          <button type="button" class="ghost" data-act="reveal">Show me the file</button>
          <button type="button" class="ghost" data-act="evidence" aria-expanded="false">Document text</button>
          <button type="button" class="ghost" data-act="events" aria-expanded="false">What happened</button>
        </div>
        <p class="err" role="alert"></p>
      </form>
      <div class="undo-strip" role="status" hidden>
        <span class="filing"></span>
        <button type="button" class="ghost" data-act="undo">Undo</button>
      </div>
      <pre class="evidence" hidden tabindex="0"></pre>
      <div class="timeline" hidden tabindex="0"></div>
    </article>`);

  const form = q<HTMLFormElement>(root, "form");
  const dateInput = q<HTMLInputElement>(root, '[name="date"]');
  const subjectInput = q<HTMLInputElement>(root, '[name="subject"]');
  const descInput = q<HTMLInputElement>(root, '[name="description"]');
  const errBox = q<HTMLElement>(root, ".err");
  const blocker = q<HTMLElement>(root, ".blocker");
  const approve = q<HTMLButtonElement>(root, '[data-act="approve"]');
  const evidencePane = q<HTMLElement>(root, ".evidence");
  const timelinePane = q<HTMLElement>(root, ".timeline");
  const undoStrip = q<HTMLElement>(root, ".undo-strip");

  const card: ReviewCard = {
    root,
    job,
    dirty: false,
    busy: false,
    patch(next: Job) {
      card.job = next;
      // Only ever reached for a card with nothing unsaved in it, so the "left
      // as you had it" note no longer applies.
      root.querySelector(".kept-note")?.remove();
      const nextCopy = reasonCopy(next.flag_reason);
      q<HTMLElement>(root, ".card-head strong").textContent = next.original_name;
      q<HTMLElement>(root, ".ext").textContent = next.ext.toUpperCase();
      q<HTMLElement>(root, ".why-title").textContent = nextCopy.title;
      q<HTMLElement>(root, ".why-body").textContent = nextCopy.why;
      q<HTMLElement>(root, ".why-next").textContent = nextCopy.next;
      q<HTMLElement>(root, ".reason").textContent = next.flag_reason ?? "(none recorded)";
      dateInput.value = next.proposed_date ?? "";
      subjectInput.value = next.proposed_subject ?? "";
      descInput.value = next.description ?? "";
      updateCounters();
    },
    markStale(message: string) {
      root.classList.add("stale");
      if (!root.querySelector(".stale-note")) {
        const note = el(`<p class="stale-note" role="status"></p>`);
        note.textContent = message;
        root.insertBefore(note, form);
      }
    },
    markKept() {
      // Says out loud why this one card did not change when everything around
      // it did. Silence here reads as a refresh that did not work.
      if (root.querySelector(".kept-note")) return;
      const note = el(`<p class="kept-note" role="status"></p>`);
      note.textContent = "Left exactly as you had it — this card has work you have not filed yet.";
      root.insertBefore(note, form);
    },
  };
  card.patch(job);

  // --- advisory counters ---------------------------------------------------
  // These never veto anything the checker would accept: for a human it does
  // not apply the 2-10 word rule at all (checker.rs gates that on
  // Source::Model), so the word count advises and Approve is only disabled for
  // input the checker genuinely cannot take.
  function updateCounters(): void {
    const words = subjectInput.value.trim().split(/\s+/).filter(Boolean).length;
    const subCounter = q<HTMLElement>(root, `#c-sub-${short}`);
    subCounter.textContent = `${words} word${words === 1 ? "" : "s"} · ${SUBJECT_WORDS[0]}–${SUBJECT_WORDS[1]} recommended`;
    subCounter.classList.toggle("out", words > 0 && (words < SUBJECT_WORDS[0] || words > SUBJECT_WORDS[1]));

    const chars = descInput.value.trim().length;
    const descCounter = q<HTMLElement>(root, `#c-desc-${short}`);
    descCounter.textContent = `${chars} / ${DESCRIPTION_CHARS[0]}–${DESCRIPTION_CHARS[1]} characters`;
    descCounter.classList.toggle("out", chars > 0 && (chars < DESCRIPTION_CHARS[0] || chars > DESCRIPTION_CHARS[1]));

    const problem = approveBlocker();
    blocker.textContent = problem ?? "";
    blocker.hidden = problem === null;
    approve.disabled = problem !== null || card.busy;
  }

  function approveBlocker(): string | null {
    if (!dateInput.value) return "Fill in the date printed on the document before filing it.";
    if (!subjectInput.value.trim()) return "Write a short subject before filing this document.";
    const chars = descInput.value.trim().length;
    if (chars === 0) return "Write a one-sentence description before filing this document.";
    if (chars > DESCRIPTION_CHARS[1]) return `The description is ${chars} characters; BackLog cannot accept more than ${DESCRIPTION_CHARS[1]}.`;
    return null;
  }

  for (const input of [dateInput, subjectInput, descInput]) {
    input.addEventListener("input", () => {
      card.dirty = true;
      updateCounters();
    });
  }

  // --- disclosures ---------------------------------------------------------
  const togglePane = async (
    button: HTMLButtonElement,
    pane: HTMLElement,
    load: () => Promise<void>
  ) => {
    if (pane.hidden) {
      button.disabled = true;
      await load();
      button.disabled = false;
    }
    pane.hidden = !pane.hidden;
    button.setAttribute("aria-expanded", String(!pane.hidden));
  };

  q<HTMLButtonElement>(root, '[data-act="evidence"]').addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    await togglePane(button, evidencePane, async () => {
      evidencePane.textContent = await loadEvidence(card.job.sha256)
        ?? "There is no saved text for this file — it failed before BackLog could read it.";
    });
  });

  q<HTMLButtonElement>(root, '[data-act="events"]').addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    await togglePane(button, timelinePane, async () => {
      timelinePane.replaceChildren(await buildTimeline(card.job.sha256));
    });
  });

  // --- actions -------------------------------------------------------------
  q<HTMLButtonElement>(root, '[data-act="reveal"]').addEventListener("click", async () => {
    try {
      await invoke("reveal_quarantined", { sha256: card.job.sha256 });
    } catch (e) {
      showError(e);
    }
  });

  q<HTMLButtonElement>(root, '[data-act="reprocess"]').addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    button.disabled = true;
    button.textContent = "Putting it back…";
    try {
      await invoke("reprocess", { sha256: card.job.sha256 });
      showSuccess(`${card.job.original_name} went back into the queue for another try.`);
      dropCard(card);
    } catch (e) {
      showError(e);
      button.disabled = false;
      button.textContent = "Try again";
    }
  });

  q<HTMLButtonElement>(root, '[data-act="dismiss"]').addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    if (button.dataset.confirm !== "yes") {
      button.dataset.confirm = "yes";
      button.textContent = "Set aside for good?";
      // One accidental click must not retire a document; a second, deliberate
      // one within five seconds does.
      setTimeout(() => {
        if (button.dataset.confirm !== "yes") return;
        delete button.dataset.confirm;
        button.textContent = "Can't fix this";
      }, 5000);
      return;
    }
    button.disabled = true;
    button.textContent = "Setting aside…";
    try {
      await invoke("dismiss", { sha256: card.job.sha256, note: "left in quarantine by reviewer" });
      showSuccess(`${card.job.original_name} was set aside. The file stays in the Quarantine folder.`);
      dropCard(card);
    } catch (e) {
      showError(e);
      button.disabled = false;
      delete button.dataset.confirm;
      button.textContent = "Can't fix this";
    }
  });

  // A real <form>, so Enter in any field files the card.
  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    if (approve.disabled) return;
    errBox.textContent = "";
    beginApproval(card, {
      date: dateInput.value.trim(),
      subject: subjectInput.value.trim(),
      description: descInput.value.trim(),
    });
  });

  q<HTMLButtonElement>(root, '[data-act="undo"]').addEventListener("click", () => cancelApproval(card));

  // Dates the document actually contains, offered as one click. Loaded lazily
  // so a page of 25 cards does not fire 25 file reads before it paints.
  observeForDates(card, dateInput, q<HTMLElement>(root, ".date-chips"));

  reviewCards.set(job.sha256, card);
  return card;
}

function dropCard(card: ReviewCard): void {
  const sha256 = card.job.sha256;
  // A deferred approval commits ten seconds after the click, and a full render
  // can happen in between. Drop whatever card is on screen for this sha now,
  // not the node the approval closed over — deleting the map entry while a
  // rebuilt node stayed in the document left an editable, un-removable card
  // for a file that had already been filed, and approving it again would write
  // a second manifest for a document Power Automate has already processed.
  const live = reviewCards.get(sha256);
  const target = card.root.isConnected || !live?.root.isConnected ? card : live;
  reviewCards.delete(sha256);

  // Only move focus if it is inside the card that is going away. Approve is
  // deferred, so by the time this runs the reviewer is usually typing in a
  // different card — yanking the caret into a native date input mid-word
  // swallows everything typed after it. beginApproval already advanced focus
  // at the moment of the click, which is the case this was written for.
  const stealing = target.root.contains(document.activeElement);
  const next = target.root.nextElementSibling;
  target.root.remove();
  if (stealing) next?.querySelector<HTMLElement>('[name="date"]')?.focus();

  paintReviewFoot();
  void loadStats().then(() => {
    paintHeader();
    paintReviewFoot();
  });
  // The list is never paged forward over a set that shrinks underneath the
  // reviewer; when the visible head is cleared, the next head is fetched.
  if (view === "flagged" && reviewCards.size === 0) requestRender();
}

// --- evidence, timeline, date candidates ------------------------------------

const evidenceCache = new Map<string, string | null>();

async function loadEvidence(sha256: string): Promise<string | null> {
  if (evidenceCache.has(sha256)) return evidenceCache.get(sha256)!;
  let text: string | null = null;
  try {
    text = await invoke<string>("get_evidence", { sha256 });
  } catch {
    text = null;
  }
  evidenceCache.set(sha256, text);
  return text;
}

async function buildTimeline(sha256: string): Promise<HTMLElement> {
  let events: LedgerEvent[] = [];
  try {
    events = await invoke<LedgerEvent[]>("get_events", { sha256, limit: 60 });
  } catch (e) {
    const err = el(`<p class="err"></p>`);
    err.textContent = friendlyError(String(e)).message;
    return err;
  }
  if (!events.length) {
    const none = el(`<p class="dim-note"></p>`);
    none.textContent = "BackLog kept no step-by-step record for this file.";
    return none;
  }
  const list = el(`<ol></ol>`);
  for (const event of events) {
    const item = el(`<li><span class="at"></span><span class="stage"></span><span class="detail"></span></li>`);
    q<HTMLElement>(item, ".at").textContent = new Date(event.at).toLocaleString();
    q<HTMLElement>(item, ".stage").textContent = event.stage;
    q<HTMLElement>(item, ".detail").textContent = event.detail;
    list.appendChild(item);
  }
  return list;
}

const MONTHS: Record<string, number> = {
  jan: 1, feb: 2, mar: 3, apr: 4, may: 5, jun: 6, jul: 7, aug: 8, sep: 9, oct: 10, nov: 11, dec: 12,
};

/** Dates literally present in the converted text, offered as chips. Purely a
 *  typing shortcut: whatever is chosen still goes through the Rust checker,
 *  which is the only thing that decides whether a date may ship. */
function extractDateCandidates(text: string, limit = 6): string[] {
  const found: string[] = [];
  const push = (y: number, m: number, d: number) => {
    if (m < 1 || m > 12 || d < 1 || d > 31 || y < 1800 || y > 2200) return;
    const iso = `${y}-${String(m).padStart(2, "0")}-${String(d).padStart(2, "0")}`;
    if (!found.includes(iso)) found.push(iso);
  };
  const head = text.slice(0, 20000);
  for (const m of head.matchAll(/\b(\d{4})-(\d{2})-(\d{2})\b/g)) {
    push(+m[1], +m[2], +m[3]);
  }
  for (const m of head.matchAll(/\b(\d{1,2})\s+([A-Za-z]{3,9})\.?,?\s+(\d{4})\b/g)) {
    const month = MONTHS[m[2].slice(0, 3).toLowerCase()];
    if (month) push(+m[3], month, +m[1]);
  }
  for (const m of head.matchAll(/\b([A-Za-z]{3,9})\.?\s+(\d{1,2})(?:st|nd|rd|th)?,?\s+(\d{4})\b/g)) {
    const month = MONTHS[m[1].slice(0, 3).toLowerCase()];
    if (month) push(+m[3], month, +m[2]);
  }
  return found.slice(0, limit);
}

function observeForDates(card: ReviewCard, dateInput: HTMLInputElement, host: HTMLElement): void {
  const fill = async () => {
    const text = await loadEvidence(card.job.sha256);
    if (!text) return;
    const candidates = extractDateCandidates(text);
    if (!candidates.length) return;
    for (const iso of candidates) {
      const chip = el(`<button type="button" class="date-chip"></button>`);
      chip.textContent = iso;
      chip.addEventListener("click", () => {
        dateInput.value = iso;
        card.dirty = true;
        dateInput.dispatchEvent(new Event("input", { bubbles: true }));
        dateInput.focus();
      });
      host.appendChild(chip);
    }
    host.hidden = false;
  };
  if (typeof IntersectionObserver !== "function") {
    void fill();
    return;
  }
  const observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      observer.disconnect();
      void fill();
    }
  });
  observer.observe(card.root);
}

// --- deferred approval ------------------------------------------------------

type PendingApproval = {
  card: ReviewCard;
  fields: { date: string; subject: string; description: string };
  remaining: number;
  timer: number;
};

const pendingApprovals = new Map<string, PendingApproval>();

function beginApproval(card: ReviewCard, fields: PendingApproval["fields"]): void {
  if (pendingApprovals.has(card.job.sha256)) return;
  card.busy = true;
  const form = q<HTMLElement>(card.root, "form");
  const strip = q<HTMLElement>(card.root, ".undo-strip");
  form.hidden = true;
  strip.hidden = false;
  const pending: PendingApproval = { card, fields, remaining: UNDO_SECONDS, timer: 0 };
  const paint = () => {
    q<HTMLElement>(strip, ".filing").innerHTML =
      `Filing <b>${esc(fields.date)} ${esc(fields.subject)}</b> — <span class="count">`
      + `${pending.remaining}s</span> to change your mind.`;
  };
  paint();
  pending.timer = window.setInterval(() => {
    pending.remaining -= 1;
    if (pending.remaining > 0) {
      paint();
      return;
    }
    void commitApproval(pending);
  }, 1000);
  pendingApprovals.set(card.job.sha256, pending);
  // Move on: the next card's date field is where the reviewer is going.
  const next = card.root.nextElementSibling?.querySelector<HTMLElement>('[name="date"]');
  next?.focus();
}

function cancelApproval(card: ReviewCard): void {
  const pending = pendingApprovals.get(card.job.sha256);
  if (!pending) return;
  window.clearInterval(pending.timer);
  pendingApprovals.delete(card.job.sha256);
  card.busy = false;
  q<HTMLElement>(card.root, "form").hidden = false;
  q<HTMLElement>(card.root, ".undo-strip").hidden = true;
  q<HTMLInputElement>(card.root, '[name="subject"]').focus();
}

async function commitApproval(pending: PendingApproval): Promise<void> {
  window.clearInterval(pending.timer);
  const { card, fields } = pending;
  pendingApprovals.delete(card.job.sha256);
  const strip = q<HTMLElement>(card.root, ".undo-strip");
  q<HTMLElement>(strip, ".filing").innerHTML =
    `<span class="spinner-dot"></span>Filing <b>${esc(fields.subject)}</b>…`;
  q<HTMLButtonElement>(strip, '[data-act="undo"]').disabled = true;
  try {
    await invoke("resubmit", { sha256: card.job.sha256, ...fields });
    dropCard(card);
  } catch (e) {
    // The card comes back exactly as the reviewer left it, with their typing
    // intact — this is the one place a lost correction would be unrecoverable.
    card.busy = false;
    strip.hidden = true;
    const form = q<HTMLElement>(card.root, "form");
    form.hidden = false;
    q<HTMLButtonElement>(strip, '[data-act="undo"]').disabled = false;
    q<HTMLInputElement>(card.root, '[name="date"]').value = fields.date;
    q<HTMLInputElement>(card.root, '[name="subject"]').value = fields.subject;
    q<HTMLInputElement>(card.root, '[name="description"]').value = fields.description;
    q<HTMLElement>(card.root, ".err").textContent = friendlyError(String(e)).message;
    showError(e);
  }
}

/** Commit everything still on its countdown. Called when the reviewer leaves
 *  the screen and when the window goes away: a pending approval must never be
 *  silently dropped. */
function flushPendingApprovals(): void {
  for (const pending of [...pendingApprovals.values()]) void commitApproval(pending);
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/** Windows Explorer's Shift+right-click "Copy as path" yields a quoted string,
 *  and it is the standard way a non-technical user gets a path into a text
 *  field. Config::normalize does this too now, but doing it here as well means
 *  the field the user is looking at shows the value that was actually saved. */
function normalizePath(value: string): string {
  const trimmed = value.trim();
  const unquoted = trimmed.replace(/^"(.*)"$/s, "$1").replace(/^'(.*)'$/s, "$1");
  return unquoted.trim();
}

function buildSettings(): Node[] {
  if (!cfg) return [el(`<div class="empty">Loading settings…</div>`)];
  const c = cfg;
  const nodes: Node[] = [renderReadinessPanel()];

  const folder = (label: string, key: keyof Config, note?: string) => `
    <label class="wide">${label}
      <div class="pick"><input name="${key}" value="${esc(String(c[key] ?? ""))}" spellcheck="false">
      <button type="button" class="ghost" data-pick="${key}">Browse</button></div>
      ${note ? `<span class="field-note">${note}</span>` : ""}
    </label>`;

  const num = (label: string, key: keyof Config, min: number, max: number | null, note: string) => `
    <label>${label}
      <input name="${key}" type="number" min="${min}" ${max === null ? "" : `max="${max}"`}
        value="${esc(String(c[key]))}">
      <span class="field-note">${note}</span>
    </label>`;

  const form = el(`
    <form class="settings">
      <h2>Folders</h2>
      ${folder("Processing folder — BackLog watches this for new documents", "processing_dir",
        "This is the OneDrive folder your SharePoint intake drops files into.")}
      ${folder("Outbox folder — BackLog writes its results here", "outbox_dir",
        "Also OneDrive-synced. Manifests go into a <code>_manifests</code> subfolder.")}
      ${folder("Quarantine folder — files that need review wait here", "quarantine_dir",
        "Stays on this computer; it is never synced.")}

      <details class="advanced">
        <summary>Advanced — you shouldn't need to change these</summary>
        <div class="advanced-body">
          <p class="dim-note">These are set correctly by the installer. Changing them can stop
            BackLog working or make this computer unusably slow while it runs.</p>
          <h2>Model files</h2>
          ${folder(`Primary model file (${PRIMARY_GGUF_NAME})`, "slm_primary_gguf")}
          ${folder(`Escalation model file (${ESCALATION_GGUF_NAME})`, "slm_escalation_gguf")}
          ${c.ettin_model_dir
            ? folder("Ettin model dir (blank = disabled)", "ettin_model_dir")
            : ""}
          <h2>Tuning</h2>
          <div class="grid3">
            ${num("Convert workers", "convert_workers", 1, 12, "More than your core count will freeze this computer.")}
            ${num("Naming requests at once", "slm_parallel", 1, 8, "Higher uses more memory.")}
            ${num("Evidence tokens", "evidence_token_budget", 400, 4000, "How much of each document is read.")}
            ${num("Manifests per minute", "manifest_emit_per_min", 0, null, "0 means as fast as possible.")}
            ${num("Attempts per step", "max_stage_attempts", 1, 5, "Before a file is sent to review.")}
            ${num("Seconds per file", "per_file_wall_clock_secs", 30, null, "Time budget for one document.")}
          </div>
          <div class="card-actions">
            <button type="button" class="ghost" id="reset-tuning">Reset to recommended</button>
          </div>
        </div>
      </details>

      <div class="card-actions">
        <button type="submit" class="primary">Save settings</button>
        <span class="err" role="alert"></span>
        <span class="ok-msg" role="status"></span>
      </div>
    </form>`);

  form.querySelectorAll<HTMLElement>("[data-pick]").forEach((b) =>
    b.addEventListener("click", async (ev) => {
      ev.preventDefault();
      const key = b.dataset.pick!;
      const isFile = key.includes("gguf");
      try {
        const sel = await open({ directory: !isFile, multiple: false });
        if (typeof sel === "string") {
          q<HTMLInputElement>(form, `[name="${key}"]`).value = sel;
        }
      } catch (e) {
        showError(e);
      }
    })
  );

  form.querySelector("#reset-tuning")?.addEventListener("click", () => {
    for (const [key, value] of Object.entries(RECOMMENDED_TUNING)) {
      const input = form.querySelector<HTMLInputElement>(`[name="${key}"]`);
      if (input) input.value = String(value);
    }
  });

  // Clamp visibly, as it is typed, rather than silently at save time.
  form.querySelectorAll<HTMLInputElement>('input[type="number"]').forEach((input) =>
    input.addEventListener("change", () => {
      const clamped = clampNumber(input);
      if (String(clamped) !== input.value) input.value = String(clamped);
    })
  );

  form.addEventListener("submit", (ev) => {
    ev.preventDefault();
    void saveSettings(form);
  });

  nodes.push(form);
  return nodes;
}

function clampNumber(input: HTMLInputElement): number {
  const min = input.min !== "" ? parseInt(input.min, 10) : Number.NEGATIVE_INFINITY;
  const max = input.max !== "" ? parseInt(input.max, 10) : Number.POSITIVE_INFINITY;
  let x = parseInt(input.value, 10);
  if (Number.isNaN(x)) x = Number.isFinite(min) ? min : 0;
  return Math.min(max, Math.max(min, x));
}

async function saveSettings(form: HTMLElement): Promise<void> {
  if (!cfg) return;
  const text = (n: string) => {
    const input = form.querySelector<HTMLInputElement>(`[name="${n}"]`);
    return input ? normalizePath(input.value) : null;
  };
  const number = (n: string) => {
    const input = form.querySelector<HTMLInputElement>(`[name="${n}"]`);
    return input ? clampNumber(input) : null;
  };
  const next: Config = { ...cfg };
  for (const key of ["processing_dir", "outbox_dir", "quarantine_dir", "slm_primary_gguf",
    "slm_escalation_gguf", "ettin_model_dir"] as const) {
    const value = text(key);
    if (value !== null) next[key] = value;
  }
  for (const key of ["convert_workers", "slm_parallel", "evidence_token_budget",
    "manifest_emit_per_min", "max_stage_attempts", "per_file_wall_clock_secs"] as const) {
    const value = number(key);
    if (value !== null) next[key] = value;
  }

  const err = q<HTMLElement>(form, ".err");
  const ok = q<HTMLElement>(form, ".ok-msg");
  err.textContent = "";
  ok.textContent = "";
  try {
    await invoke("set_config", { cfg: next });
    // Read back rather than trust the local copy: the backend normalizes paths
    // and may clamp values, and a user whose displayed settings differ from
    // what was stored has a wrong mental model of their own configuration.
    cfg = await invoke<Config>("get_config");
    for (const [key, value] of Object.entries(cfg)) {
      const input = form.querySelector<HTMLInputElement>(`[name="${key}"]`);
      if (input && input.value !== String(value)) input.value = String(value);
    }
    // The backend drops its cached preflight result on every save (paths may
    // have just changed underneath it); pick that up and swap only the panel
    // in, so the confirmation the user is reading stays on screen.
    await refreshRuntime(false);
    replaceReadinessPanel();
    paintHeader();
    ok.textContent = "Saved. Check this computer to verify it before starting.";
    setTimeout(() => (ok.textContent = ""), 6000);
  } catch (e) {
    err.textContent = friendlyError(String(e)).message;
  }
}

// ---------------------------------------------------------------------------
// Readiness panel
// ---------------------------------------------------------------------------

function replaceReadinessPanel(): void {
  const old = document.querySelector(".preflight-panel");
  if (old) old.replaceWith(renderReadinessPanel());
}

function renderReadinessPanel(): HTMLElement {
  // Tri-state. On a fresh install every one of these flags is false because
  // nothing has been examined — rendering that as ten red BLOCKED rows is the
  // first thing a non-technical user ever sees, and it is a lie.
  const rows = READINESS_CHECKS.map(([label, key]) => {
    const cls = !runtime.checked ? "check-unknown" : runtime[key] ? "check-pass" : "check-fail";
    const word = !runtime.checked ? "Not checked" : runtime[key] ? "Ready" : "Blocked";
    return `<li class="check-row ${cls}"><span>${esc(label)}</span><strong>${word}</strong></li>`;
  }).join("");

  const lastChecked = runtime.checked_at
    ? `Last checked ${esc(new Date(runtime.checked_at).toLocaleString())}`
    : "This computer has not been checked yet. Press Check this computer before starting BackLog.";

  const panel = el(`
    <section class="preflight-panel" aria-label="Readiness">
      <div class="section-head">
        <div>
          <h2>Readiness</h2>
          <p class="dim-note">${lastChecked}</p>
        </div>
        <div class="section-actions">
          <button type="button" id="preflight-button" class="ghost">Check this computer</button>
          <button type="button" id="update-check-button" class="ghost">Check for updates</button>
        </div>
      </div>
      <ul class="check-list">${rows}</ul>
      ${renderProcessingPeek()}
      ${renderModelDownloadSection()}
      <div class="problem-slot"></div>
      <div class="versions" id="versions"></div>
    </section>`);

  q<HTMLElement>(panel, ".problem-slot").replaceChildren(...renderProblems());
  paintVersions(q<HTMLElement>(panel, "#versions"));

  q<HTMLButtonElement>(panel, "#preflight-button").addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    button.disabled = true;
    button.textContent = "Checking…";
    await refreshRuntime(true);
    // Only the panel and the header change — the folder fields the user may be
    // halfway through typing into are not in either.
    replaceReadinessPanel();
    paintHeader();
    // Re-probe rather than reuse a cached failure: "check this computer" is
    // exactly the action that is supposed to clear a stale unavailable line.
    void loadDiagnostics(true);
  });
  panel.querySelector("#update-check-button")?.addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    button.disabled = true;
    await checkForUpdates(true);
    button.disabled = false;
  });
  panel.querySelector("#download-models-button")?.addEventListener("click", onDownloadModelsClick);
  return panel;
}

function renderProcessingPeek(): string {
  if (!runtime.checked || !runtime.processing_dir_ready || runtime.processing_entry_count === null) {
    return "";
  }
  const n = runtime.processing_entry_count;
  const capped = runtime.processing_entry_count_capped ? "at least " : "";
  const sample = runtime.processing_sample.length
    ? ` For example: ${runtime.processing_sample.map(esc).join(", ")}.`
    : "";
  return `<p class="processing-peek">BackLog can see ${capped}${formatCount(n)} item${n === 1 ? "" : "s"}
    in the Processing folder.${sample}</p>`;
}

function renderProblems(): Node[] {
  // "Nothing has been examined yet" is not a fault. The backend fails closed by
  // shipping a `preflight_required` problem, and rendering that in the red
  // Action-needed box is the first thing a new user sees.
  const problems = runtime.checked
    ? runtime.problems
    : runtime.problems.filter((p) => p.code !== "preflight_required");
  if (!problems.length) {
    if (runtime.checked) {
      return [el(`<div class="ready-box">All checks passed. BackLog is ready to start.</div>`)];
    }
    return [el(`<div class="next-box"><strong>Start here.</strong> Fill in your three folders
      below, then press <b>Check this computer</b>. BackLog will tell you what, if anything, is
      still missing.</div>`)];
  }
  const box = el(`<div class="problem-box"><strong>Action needed</strong><ul></ul></div>`);
  const list = q<HTMLElement>(box, "ul");
  for (const problem of problems) {
    const item = el(`<li><span class="msg"></span></li>`);
    q<HTMLElement>(item, ".msg").textContent = problem.message;
    if (problem.detail) {
      const detail = el(`<span class="detail"></span>`);
      detail.textContent = problem.detail;
      item.appendChild(detail);
    }
    if (problem.action === "create_folder") {
      const button = el(`<button type="button">Create this folder for me</button>`);
      button.addEventListener("click", async () => {
        (button as HTMLButtonElement).disabled = true;
        try {
          await invoke("create_missing_dir", { field: problem.field });
          await refreshRuntime(true);
          replaceReadinessPanel();
          paintHeader();
        } catch (e) {
          showError(e);
          (button as HTMLButtonElement).disabled = false;
        }
      });
      item.appendChild(button);
    } else if (problem.action === "download_models") {
      const button = el(`<button type="button">Download the model files</button>`);
      button.addEventListener("click", onDownloadModelsClick);
      item.appendChild(button);
    }
    list.appendChild(item);
  }
  return [box];
}

/** The pilot runbook asks the operator to record the version of the candidate
 *  they froze; until now it could not be read anywhere in the running app. */
function paintVersions(host: HTMLElement): void {
  const parts: string[] = [];
  if (diagnosticsError) {
    host.textContent = "Version information is unavailable until the document reader answers.";
    return;
  }
  if (diagnostics) {
    parts.push(`BackLog ${esc(diagnostics.app_version)}`);
    const versions = diagnostics.sidecar_versions ?? {};
    for (const [key, value] of Object.entries(versions)) {
      if (key === "error" || value === null || typeof value === "object") continue;
      parts.push(`${esc(key)} ${esc(String(value))}`);
    }
    if (typeof versions["error"] === "string") {
      parts.push(`document reader unavailable (${esc(versions["error"])})`);
    }
    parts.push(esc(diagnostics.platform));
  } else {
    parts.push("Reading version information…");
  }
  host.innerHTML = parts.join(" &middot; ");
}

/** `force` is what "Check this computer" passes. Without it one transient
 *  probe failure latched `diagnosticsRequested` for the rest of the session,
 *  so the version line the pilot runbook asks the operator to record stayed
 *  unreadable until the app was restarted. */
async function loadDiagnostics(force = false): Promise<void> {
  if (diagnosticsRequested && !force) return;
  diagnosticsRequested = true;
  try {
    diagnostics = await invoke<Diagnostics>("get_diagnostics");
    diagnosticsError = false;
  } catch {
    // Not a toast: nobody asked for this, it just fills a line in the panel.
    diagnostics = null;
    diagnosticsError = true;
  }
  const host = document.getElementById("versions");
  if (host) paintVersions(host);
}

// ---------------------------------------------------------------------------
// Model download
// ---------------------------------------------------------------------------

/** One-time setup egress: fetches the two Qwen GGUF model files so a
 *  non-technical user never runs models/download_models.py in a terminal. */
async function onDownloadModelsClick(): Promise<void> {
  if (modelsDownloading) return;
  modelsDownloading = true;
  modelDownloadProgress = null;
  replaceReadinessPanel();
  try {
    await invoke("download_models");
  } catch (e) {
    // The model-download-done listener normally already surfaced this; this
    // only fires for an IPC-level failure the event itself missed.
    if (modelsDownloading) {
      modelsDownloading = false;
      showError(e);
      replaceReadinessPanel();
    }
  }
}

function downloadCaption(p: ModelDownloadProgress | null): string {
  if (!p) return "Starting…";
  const pct = Math.round(p.overall_percent);
  const bytes = p.file_bytes_total > 0
    ? ` · ${formatBytes(p.file_bytes_done)} of ${formatBytes(p.file_bytes_total)}`
    : "";
  return `File ${p.files_done + 1} of ${p.files_total}: ${p.current_file} (${pct}%)${bytes}`;
}

function renderModelDownloadSection(): string {
  const needsDownload = !runtime.primary_model_found || !runtime.escalation_model_found;
  if (!needsDownload && !modelsDownloading) return "";
  const pct = modelDownloadProgress ? Math.round(modelDownloadProgress.overall_percent) : 0;
  const progress = modelsDownloading
    ? `<div class="progress-track" id="dl-track" role="progressbar" aria-valuemin="0"
         aria-valuemax="100" aria-valuenow="${pct}" aria-label="Model download progress">
         <div class="progress-fill" id="dl-fill" style="width:${pct}%"></div>
       </div>
       <p class="dim-note" id="dl-caption" aria-live="polite">${esc(downloadCaption(modelDownloadProgress))}</p>`
    : `<p class="dim-note">Fetches the two Qwen model files from Hugging Face once
        (public repos, no account needed). BackLog stays fully offline for document
        processing afterwards. You can carry on filling in your folders while it runs.</p>`;
  return `
    <div class="model-download">
      <button type="button" id="download-models-button" ${modelsDownloading ? "disabled" : ""}>
        ${modelsDownloading ? "Downloading models…" : "Download models (~2.4 GB)"}
      </button>
      ${progress}
    </div>`;
}

// ---------------------------------------------------------------------------
// Events from the backend
//
// None of these re-render. A backfill emits job-updated continuously and the
// model download emits progress five times a second for twenty minutes; both
// used to blow away the DOM — and with it any correction being typed, the
// focus ring, and the scroll position — on a timer.
// ---------------------------------------------------------------------------

let headerRefreshQueued = false;
function scheduleHeaderRefresh(): void {
  if (headerRefreshQueued) return;
  headerRefreshQueued = true;
  setTimeout(async () => {
    headerRefreshQueued = false;
    await loadStats();
    paintHeader();
  }, 1200);
}

const TERMINAL_STATES = new Set(["emitted", "flagged", "dismissed"]);

function applyJobUpdate(job: Job): void {
  if (!TERMINAL_STATES.has(job.state)) activeJob = job;
  else if (activeJob?.sha256 === job.sha256) activeJob = null;

  if (view === "queue") {
    const row = queueRows.get(job.sha256);
    if (row) {
      const matches = queueState === null || queueState === job.state;
      if (matches) fillQueueRow(row, job);
      else bumpPending();
    } else {
      bumpPending();
    }
  } else if (view === "flagged") {
    const card = reviewCards.get(job.sha256);
    if (!card) {
      if (job.state === "flagged") bumpPending();
      return;
    }
    const held = card.dirty || card.busy || pendingApprovals.has(job.sha256);
    if (job.state !== "flagged") {
      if (held) {
        card.markStale("This file has moved on since you started editing it — your changes can no "
          + "longer be filed. Refresh to see where it went.");
      } else {
        card.root.remove();
        reviewCards.delete(job.sha256);
            }
      return;
    }
    // A card the reviewer is working in is NEVER rewritten underneath them.
    if (!held) card.patch(job);
    else bumpPending();
  }
  scheduleHeaderRefresh();
}

function bumpPending(): void {
  pendingChanges += 1;
  paintRefreshChip();
}

listen<Job>("job-updated", (event) => applyJobUpdate(event.payload));

// Backend throttles these to ~200ms/file for ~2.4 GB. Only two nodes move.
listen<ModelDownloadProgress>("model-download-progress", (event) => {
  modelDownloadProgress = event.payload;
  const pct = Math.round(event.payload.overall_percent);
  const fill = document.getElementById("dl-fill");
  const track = document.getElementById("dl-track");
  const caption = document.getElementById("dl-caption");
  if (fill) fill.style.width = `${pct}%`;
  track?.setAttribute("aria-valuenow", String(pct));
  if (caption) caption.textContent = downloadCaption(event.payload);
});

listen<ModelDownloadDone>("model-download-done", async (event) => {
  modelsDownloading = false;
  modelDownloadProgress = null;
  if (!event.payload.ok) showError(event.payload.error ?? "Model download failed.");
  else showSuccess("The model files are downloaded. BackLog can name documents now.");
  // Flip Readiness back to green (or show what's still missing) now that the
  // model files may have just landed on disk.
  await refreshRuntime(true);
  replaceReadinessPanel();
  paintHeader();
});

window.addEventListener("beforeunload", flushPendingApprovals);

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

(async () => {
  loadTheme();
  try {
    cfg = await invoke<Config>("get_config");
  } catch (e) {
    // Without config we can render nothing meaningful; show a recoverable
    // fatal state instead of a blank white window.
    const fatal = el(
      `<div class="fatal"><strong>BackLog failed to start.</strong><div class="msg"></div>
       <button type="button">Reload</button></div>`
    );
    q<HTMLElement>(fatal, ".msg").textContent = String(e);
    fatal.querySelector("button")!.addEventListener("click", () => location.reload());
    app.replaceChildren(fatal);
    return;
  }
  // Cheap, cached read (never spawns the sidecar or touches disk) so startup
  // stays fast; the fail-closed backend default keeps Start disabled until an
  // explicit check passes.
  await refreshRuntime(false);
  if (!cfg.processing_dir || !runtime.configured) view = "settings";
  await render();
  // Both non-blocking and after the first paint, so neither a slow update
  // endpoint nor a convertd probe can delay startup.
  void checkForUpdates();
  if (view === "settings") void loadDiagnostics();
})();
