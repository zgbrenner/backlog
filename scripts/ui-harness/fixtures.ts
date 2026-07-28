// Scenario fixtures for the browser UI harness. Each scenario is the set of
// Tauri command responses that puts the real frontend into one specific state.
//
// Keep these HONEST: shapes must match what the Rust commands actually return
// (src-tauri/src/lib.rs, preflight.rs, ledger.rs), or the harness will happily
// show a UI that cannot exist. When a command's return type changes, change it
// here too — a drifting fixture is worse than no fixture.
//
// The list commands are implemented as functions rather than constants because
// list_jobs/count_jobs/list_flagged now take query, state, limit and offset:
// a fixture that ignored them would let a broken search or pager screenshot
// clean.

export type CommandTable = Record<string, unknown | ((args?: unknown) => unknown)>;

export type Scenario = {
  label: string;
  commands: CommandTable;
  pickedPath?: string;
  update?: unknown;
  /** Which tab to click before screenshotting. The app itself decides where to
   *  land on boot (Settings until preflight passes), so a scenario that wants a
   *  different screen has the shoot script press the real nav button rather
   *  than the frontend growing a test-only URL parameter. */
  view?: "queue" | "flagged" | "settings";
};

// Mirrors src-tauri/src/config.rs::Config.
const CONFIG = {
  processing_dir: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing",
  outbox_dir: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Outbox",
  quarantine_dir: "C:\\ProgramData\\BackLog\\Quarantine",
  cache_dir: "C:\\Users\\dana\\AppData\\Roaming\\ai.sonomos.backlog\\cache",
  llama_port: 8137,
  slm_primary_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-0.6B-Q8_0.gguf",
  slm_escalation_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-1.7B-Q8_0.gguf",
  slm_parallel: 4,
  evidence_token_budget: 1500,
  ettin_model_dir: "",
  convert_workers: 4,
  sidecar_timeout_secs: 45,
  manifest_emit_per_min: 0,
  max_head_pages: 10,
  max_tail_pages: 3,
  max_filename_len: 120,
  max_stage_attempts: 3,
  per_file_wall_clock_secs: 90,
  retain_cache: false,
  cache_ttl_days: 7,
};

// Mirrors src-tauri/src/preflight.rs::RuntimeStatus.
const READY_RUNTIME = {
  configured: true,
  checked: true,
  running: false,
  paused: false,
  processing_dir_ready: true,
  outbox_writable: true,
  quarantine_writable: true,
  cache_writable: true,
  sidecar_found: true,
  sidecar_ok: true,
  llama_server_found: true,
  llama_server_ok: true,
  grammar_found: true,
  primary_model_found: true,
  escalation_model_found: true,
  offline_runtime: true,
  processing_entry_count: 4993,
  processing_entry_count_capped: false,
  processing_sample: ["scan0417.pdf", "Q3 board pack FINAL v2.docx", "Invoice 88213.pdf"],
  checked_at: "2026-07-28T09:14:00Z",
  problems: [] as unknown[],
};

// Codes and the `detail` / `action` fields are exactly what preflight.rs emits.
const BLOCKED_RUNTIME = {
  ...READY_RUNTIME,
  configured: false,
  primary_model_found: false,
  escalation_model_found: false,
  sidecar_ok: false,
  processing_dir_ready: false,
  processing_entry_count: null,
  processing_sample: [],
  problems: [
    {
      field: "processing_dir",
      code: "processing_missing",
      message:
        "The folder BackLog watches for new documents does not exist yet: " +
        "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing. Create it, or choose a " +
        "different folder.",
      detail: null,
      severity: "error",
      action: "create_folder",
    },
    {
      field: "models",
      code: "models_missing",
      message:
        "BackLog still needs to download the two model files it uses to name documents. " +
        "Press Download models below; it is a one-time download of about 2.5 GB.",
      detail:
        "expected C:\\ProgramData\\BackLog\\models\\Qwen3-0.6B-Q8_0.gguf and " +
        "C:\\ProgramData\\BackLog\\models\\Qwen3-1.7B-Q8_0.gguf",
      severity: "error",
      action: "download_models",
    },
    {
      field: "sidecar",
      code: "sidecar_ping_failed",
      message:
        "The part of BackLog that reads your documents did not answer. Restart BackLog; if it " +
        "keeps happening, reinstall it.",
      detail: "convertd did not respond within 5s",
      severity: "error",
      action: null,
    },
    {
      field: "llama_port",
      code: "llama_port_busy",
      message:
        "Another program on this computer is already using the network port BackLog reserves " +
        "for naming documents. BackLog will keep working only if that program stops; otherwise " +
        "ask IT to change llama_port in backlog.config.json.",
      detail: "127.0.0.1:8137 is already bound",
      severity: "warning",
      action: null,
    },
  ],
};

// Fresh install: nothing examined. Every boolean is false because nothing has
// been looked at — which the panel must render as "Not checked", not "Blocked".
const UNCHECKED_RUNTIME = {
  ...READY_RUNTIME,
  configured: false,
  checked: false,
  processing_dir_ready: false,
  outbox_writable: false,
  quarantine_writable: false,
  cache_writable: false,
  sidecar_found: false,
  sidecar_ok: false,
  llama_server_found: false,
  llama_server_ok: false,
  grammar_found: false,
  primary_model_found: false,
  escalation_model_found: false,
  processing_entry_count: null,
  processing_entry_count_capped: false,
  processing_sample: [] as string[],
  checked_at: null,
  problems: [
    {
      field: "preflight",
      code: "preflight_required",
      message: "Press Check again to test this computer before starting BackLog.",
      detail: null,
      severity: "error",
      action: null,
    },
  ],
};

const EMPTY_CONFIG = {
  ...CONFIG,
  processing_dir: "",
  outbox_dir: "",
  quarantine_dir: "",
};

// Mirrors src-tauri/src/ledger.rs::Job — the full row, which is also the
// `job-updated` event payload.
type Job = Record<string, unknown>;

function job(over: Job): Job {
  return {
    sha256: "0".repeat(64),
    original_path: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\scan0417.pdf",
    original_name: "scan0417.pdf",
    original_relpath: "scan0417.pdf",
    ext: "pdf",
    detected_type: "pdf",
    route: "pdf_text",
    state: "emitted",
    attempts: 0,
    last_stage: null,
    active_stage: null,
    stage_started_at: null,
    claimed_at: null,
    flag_reason: null,
    quarantine_path: null,
    proposed_date: "2026-03-11",
    date_source: "document",
    proposed_subject: "Termination Notice for John Smith",
    description: "Letter from Acme Corporation notifying John Smith of employment termination.",
    final_filename: "2026-03-11 Termination Notice for John Smith.pdf",
    doc_type: "letter",
    language: "en",
    duplicate_of: null,
    soft_flags: null,
    model_versions: '{"slm":"qwen3-0.6b"}',
    created_at: "2026-07-28T09:10:00.000Z",
    updated_at: "2026-07-28T09:12:04.000Z",
    ...over,
  };
}

const QUEUE: Job[] = [
  job({ sha256: "a".repeat(64), original_name: "scan0417.pdf" }),
  job({
    sha256: "b".repeat(64),
    original_name: "Q3 board pack FINAL v2.docx",
    original_path: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\2026\\Q3 board pack FINAL v2.docx",
    ext: "docx",
    state: "converted",
    final_filename: null,
    description: null,
    doc_type: "report",
  }),
  job({
    sha256: "c".repeat(64),
    original_name: "IMG_20260214_113355.jpg",
    ext: "jpg",
    state: "flagged",
    flag_reason: "DATE_NOT_IN_EVIDENCE:2026-02-14",
    final_filename: null,
    description: null,
    doc_type: "scan",
  }),
  job({
    sha256: "d".repeat(64),
    original_name: "Invoice 88213.pdf",
    ext: "pdf",
    state: "emitted",
    proposed_date: "2026-01-30",
    proposed_subject: "Invoice 88213 from Northwind Traders",
    description: "Invoice 88213 issued by Northwind Traders for March consultancy work.",
    final_filename: "2026-01-30 Invoice 88213 from Northwind Traders.pdf",
    doc_type: "invoice",
    soft_flags: "DATE_FROM_BODY",
  }),
  job({
    sha256: "e".repeat(64),
    original_name: "policy-handbook.pdf",
    ext: "pdf",
    state: "named",
    final_filename: null,
    description: null,
    date_source: "metadata",
    doc_type: "policy",
    updated_at: new Date().toISOString(),
  }),
  job({
    sha256: "9".repeat(64),
    original_name: "thumbs.db",
    ext: "db",
    state: "dismissed",
    final_filename: null,
    description: null,
    proposed_subject: null,
    proposed_date: null,
    flag_reason: "DISMISSED:left in quarantine by reviewer",
  }),
];

const FLAGGED: Job[] = [
  job({
    sha256: "c".repeat(64),
    original_name: "IMG_20260214_113355.jpg",
    ext: "jpg",
    state: "flagged",
    flag_reason: "DATE_NOT_IN_EVIDENCE:2026-02-14",
    quarantine_path: "C:\\ProgramData\\BackLog\\Quarantine\\cccccccccccc__IMG_20260214_113355.jpg",
    proposed_date: "",
    proposed_subject: "Signed Lease Agreement Riverside Unit",
    description: "Signed lease agreement for the Riverside unit between Contoso and A. Patel.",
    final_filename: null,
  }),
  job({
    sha256: "f".repeat(64),
    original_name: "notes.docx",
    ext: "docx",
    state: "flagged",
    flag_reason: "BAD_SUBJECT:generic subject 'Document'",
    quarantine_path: "C:\\ProgramData\\BackLog\\Quarantine\\ffffffffffff__notes.docx",
    proposed_subject: "Document",
    description: "",
    final_filename: null,
  }),
  job({
    sha256: "1".repeat(64),
    original_name: "kontrakt-2026.pdf",
    ext: "pdf",
    state: "flagged",
    flag_reason: "UNREADABLE:all conversion attempts exhausted",
    quarantine_path: "C:\\ProgramData\\BackLog\\Quarantine\\111111111111__kontrakt-2026.pdf",
    proposed_subject: "",
    description: "",
    proposed_date: "",
    final_filename: null,
  }),
  job({
    sha256: "2".repeat(64),
    original_name: "payroll-run-locked.pdf",
    ext: "pdf",
    state: "flagged",
    flag_reason: "ENCRYPTED:password protected",
    quarantine_path: "C:\\ProgramData\\BackLog\\Quarantine\\222222222222__payroll-run-locked.pdf",
    proposed_subject: "",
    description: "",
    proposed_date: "",
    final_filename: null,
  }),
];

/** A backfill big enough to expose anything that only breaks at size. */
const BIG_QUEUE: Job[] = Array.from({ length: 5000 }, (_, i) =>
  job({
    sha256: i.toString(16).padStart(64, "0"),
    original_name: `batch-${String(i).padStart(4, "0")}.pdf`,
    original_path: `C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\2024\\batch-${String(i).padStart(4, "0")}.pdf`,
    state: i % 17 === 0 ? "flagged" : i % 5 === 0 ? "converted" : "emitted",
    flag_reason: i % 17 === 0 ? "DATE_NOT_IN_EVIDENCE:2024-06-01" : null,
    final_filename: i % 17 === 0 || i % 5 === 0 ? null : `2024-06-01 Batch record ${i}.pdf`,
    description: i % 5 === 0 ? null : `Scanned batch record number ${i} from the 2024 archive.`,
  })
);

/** Same backfill, but with a genuinely in-flight file at the head so the
 *  "Working on:" line is exercised. BIG_QUEUE's rows are all hours old, which
 *  is the *stalled* case — worth shooting too, just not as the only one. */
const RUNNING_QUEUE: Job[] = [
  job({
    sha256: "7".repeat(64),
    original_name: "2019 payroll batch 041.pdf",
    original_path: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\2019\\2019 payroll batch 041.pdf",
    state: "filtered",
    final_filename: null,
    description: null,
    updated_at: new Date().toISOString(),
  }),
  ...BIG_QUEUE.slice(1),
];

const EVIDENCE =
  "# Termination Notice\n\nAcme Corporation\n1 Industrial Way\n\n11 March 2026\n\n" +
  "Dear Mr Smith,\n\nThis letter confirms the termination of your employment with effect from\n" +
  "2026-03-31, following the consultation meeting held on 4 March 2026.\n\n" +
  "Yours sincerely,\nH. Okonkwo\nHead of People\n";

const EVENTS = [
  { id: 9, sha256: "c".repeat(64), at: "2026-07-28T09:12:04.101Z", stage: "flag", detail: "DATE_NOT_IN_EVIDENCE:2026-02-14" },
  { id: 8, sha256: "c".repeat(64), at: "2026-07-28T09:12:03.882Z", stage: "name", detail: "escalation model proposed 2026-02-14, not found in evidence" },
  { id: 7, sha256: "c".repeat(64), at: "2026-07-28T09:12:01.400Z", stage: "name", detail: "span mismatch, re-prompting" },
  { id: 6, sha256: "c".repeat(64), at: "2026-07-28T09:11:58.220Z", stage: "convert", detail: "attempt 2: ocr conf 0.44 below floor 0.60" },
  { id: 5, sha256: "c".repeat(64), at: "2026-07-28T09:11:50.010Z", stage: "convert", detail: "attempt 1: no embedded text layer" },
  { id: 4, sha256: "c".repeat(64), at: "2026-07-28T09:11:49.900Z", stage: "ingest", detail: "ingested" },
];

const DIAGNOSTICS = {
  app_version: "0.2.0",
  platform: "windows x86_64",
  sidecar_versions: {
    convertd: "0.4.1",
    markitdown: "0.0.1a3",
    rapidocr: "1.3.24",
    lingua: "2.0.2",
  },
};

const STATS_BUSY = { ingested: 12, converted: 8, named: 4, emitted: 1841, flagged: 4, per_hour: 0 };
const STATS_EMPTY = {};

type ListArgs = {
  query?: string | null;
  jobState?: string | null;
  job_state?: string | null;
  limit?: number;
  offset?: number;
};

function filtered(rows: Job[], args?: unknown): Job[] {
  const a = (args ?? {}) as ListArgs;
  const query = (a.query ?? "").toString().trim().toLowerCase();
  const state = a.jobState ?? a.job_state ?? null;
  return rows.filter((row) => {
    if (state && row["state"] !== state) return false;
    if (!query) return true;
    const name = String(row["original_name"] ?? "").toLowerCase();
    const final = String(row["final_filename"] ?? "").toLowerCase();
    return name.includes(query) || final.includes(query);
  });
}

function page(rows: Job[], args?: unknown): Job[] {
  const a = (args ?? {}) as ListArgs;
  const offset = a.offset ?? 0;
  const limit = a.limit ?? 500;
  return rows.slice(offset, offset + limit);
}

function base(runtime: unknown, stats: unknown, jobs: Job[], flagged: Job[]): CommandTable {
  return {
    get_config: CONFIG,
    get_runtime_status: runtime,
    run_preflight: runtime,
    get_stats: stats,
    list_jobs: (args?: unknown) => page(filtered(jobs, args), args),
    count_jobs: (args?: unknown) => filtered(jobs, args).length,
    list_flagged: (args?: unknown) => page(flagged, args),
    set_config: null,
    set_paused: null,
    start_pipeline: null,
    resubmit: null,
    dismiss: null,
    reprocess: null,
    reveal_quarantined: null,
    create_missing_dir: null,
    open_logs_folder: null,
    download_models: null,
    get_diagnostics: DIAGNOSTICS,
    get_events: EVENTS,
    get_evidence: EVIDENCE,
  };
}

/** count_jobs for the review screen is asked with state=flagged, which the
 *  shared `filtered` helper answers off the queue rows; the review fixtures are
 *  a separate list, so scenarios that show both wire the count explicitly. */
function withFlaggedCount(table: CommandTable, jobs: Job[], flagged: Job[]): CommandTable {
  return {
    ...table,
    count_jobs: (args?: unknown) => {
      const a = (args ?? {}) as ListArgs;
      const state = a.jobState ?? a.job_state ?? null;
      if (state === "flagged") return filtered(flagged, args).length;
      return filtered(jobs, args).length;
    },
  };
}

function scenario(
  label: string,
  runtime: unknown,
  stats: unknown,
  jobs: Job[],
  flagged: Job[],
  over: Partial<Scenario> = {}
): Scenario {
  return {
    label,
    commands: withFlaggedCount(base(runtime, stats, jobs, flagged), jobs, flagged),
    ...over,
  };
}

export const SCENARIOS: Record<string, Scenario> = {
  /** Steady state: configured, preflight green, a real queue behind it. */
  ready: scenario("Ready, queue populated", READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),

  /** Very first launch: nothing configured, nothing checked, no files. */
  "first-run": {
    ...scenario("First run, nothing configured", UNCHECKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(UNCHECKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      get_config: EMPTY_CONFIG,
      // Nothing is installed yet on a fresh machine, so the convertd probe
      // behind the version line fails rather than answering.
      get_diagnostics: () => new Error("convertd is not installed"),
    },
  },

  /** Preflight has run and failed: the "why can't I start?" state. */
  blocked: scenario("Preflight failed, models missing", BLOCKED_RUNTIME, STATS_EMPTY, [], []),

  /** The review backlog — the screen a user actually spends time in. */
  review: scenario("Needs Review backlog", READY_RUNTIME, { ...STATS_BUSY, flagged: 4 }, QUEUE, FLAGGED, {
    view: "flagged",
  }),

  /** Backend is down: every list command rejects. */
  errors: {
    label: "Backend errors on every read",
    commands: {
      ...base(READY_RUNTIME, STATS_BUSY, [], []),
      list_jobs: () => new Error("ledger is locked by another process (code 5)"),
      list_flagged: () => new Error("ledger is locked by another process (code 5)"),
      count_jobs: () => new Error("ledger is locked by another process (code 5)"),
    },
  },

  /** Every read hangs: the first-paint state, before any data has resolved. */
  loading: {
    label: "Still loading",
    commands: {
      ...base(READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),
      get_stats: () => new Promise(() => {}),
      list_jobs: () => new Promise(() => {}),
      count_jobs: () => new Promise(() => {}),
    },
  },

  /** An update is waiting — exercises the banner. */
  update: scenario("Update available", READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED, {
    update: {
      version: "0.3.0",
      downloadAndInstall: async () => {
        /* left pending so the banner's busy state can be screenshotted */
        await new Promise(() => {});
      },
    },
  }),

  /** Mid-download. The shoot script presses Download and drives the progress
   *  events, so this is the real listener path rather than a faked-up DOM. */
  downloading: {
    ...scenario("Downloading the models", BLOCKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(BLOCKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      // Never resolves: the real command only returns when the whole 2.4 GB is
      // down, and the panel is driven by the progress events meanwhile.
      download_models: () => new Promise(() => {}),
    },
  },

  /** The evidence pane and the per-attempt diagnosis, both expanded. */
  "review-detail": scenario("Review card, everything expanded", READY_RUNTIME,
    { ...STATS_BUSY, flagged: 4 }, QUEUE, FLAGGED, { view: "flagged" }),

  /** Several failures at once — the case where toasts used to land exactly on
   *  top of each other and only the last one was legible. */
  toasts: {
    ...scenario("Three errors at once", READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),
    commands: {
      ...withFlaggedCount(base(READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED), QUEUE, FLAGGED),
      start_pipeline: () =>
        new Error(
          "BackLog is not ready yet: the naming engine's start-up and the backup model file " +
            "could not be verified."
        ),
    },
  },

  /** Running: the activity bar, throughput and ETA the backfill needs. */
  running: scenario(
    "Running a backfill",
    { ...READY_RUNTIME, running: true },
    { ingested: 210, converted: 90, named: 40, emitted: 4611, flagged: 49, per_hour: 612 },
    RUNNING_QUEUE,
    FLAGGED
  ),

  /** Paused mid-run, reached by reloading the webview: running/paused have to
   *  come from the backend or the button lies about which one it is. */
  paused: scenario(
    "Paused",
    { ...READY_RUNTIME, running: true, paused: true },
    { ingested: 210, converted: 90, emitted: 4611, flagged: 49, per_hour: 0 },
    BIG_QUEUE,
    FLAGGED
  ),

  /** A 5,000-file backfill, to expose problems that only appear at size. */
  scale: scenario(
    "5,000-file backfill",
    { ...READY_RUNTIME, running: true },
    { ingested: 210, converted: 90, named: 40, emitted: 4611, flagged: 49, per_hour: 340 },
    BIG_QUEUE,
    FLAGGED
  ),
};
