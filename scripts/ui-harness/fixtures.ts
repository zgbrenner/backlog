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
  output_mode: "power_automate",
  local_output_dir: "C:\\Users\\dana\\Documents\\BackLog\\Filed",
  quarantine_dir: "C:\\ProgramData\\BackLog\\Quarantine",
  custom_naming_notes: "",
  cache_dir: "C:\\Users\\dana\\AppData\\Roaming\\ai.sonomos.backlog\\cache",
  llama_port: 8137,
  slm_primary_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-0.6B-Q8_0.gguf",
  slm_escalation_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-1.7B-Q8_0.gguf",
  slm_parallel: 2,
  evidence_token_budget: 2500,
  ettin_model_dir: "",
  convert_workers: 4,
  sidecar_timeout_secs: 45,
  manifest_emit_per_min: 0,
  max_head_pages: 10,
  max_tail_pages: 3,
  max_filename_len: 120,
  max_stage_attempts: 3,
  per_file_wall_clock_secs: 180,
  retain_cache: false,
  cache_ttl_days: 7,
};

// Mirrors src-tauri/src/preflight.rs::RuntimeStatus.
const READY_RUNTIME = {
  output_mode: "power_automate",
  configured: true,
  checked: true,
  running: false,
  paused: false,
  processing_dir_ready: true,
  outbox_writable: true,
  local_output_writable: true,
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

/** Normal installed state: the bundled primary works, while the optional
 * backup model has not been fetched yet. */
const OPTIONAL_MODEL_RUNTIME = {
  ...READY_RUNTIME,
  escalation_model_found: false,
  problems: [
    {
      field: "slm_escalation_gguf",
      code: "escalation_model_missing_using_primary",
      message: "The optional backup model is not installed. BackLog is ready to work using the everyday model for backup naming attempts.",
      detail: null,
      severity: "warning",
      action: "download_models",
    },
  ],
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
        "BackLog still needs to download its local model bundle. " +
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
  local_output_writable: false,
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
  local_output_dir: "",
  quarantine_dir: "",
};

/** Old installations have neither additive field on disk. The frontend must
 * still visibly choose the established Power Automate handoff. */
const LEGACY_EMPTY_CONFIG = { ...EMPTY_CONFIG } as Record<string, unknown>;
delete LEGACY_EMPTY_CONFIG.output_mode;
delete LEGACY_EMPTY_CONFIG.local_output_dir;

const LOCAL_CONFIG = {
  ...CONFIG,
  output_mode: "local",
  outbox_dir: "",
  local_output_dir: "C:\\Users\\dana\\Documents\\BackLog\\Filed",
};

const LOCAL_READY_RUNTIME = {
  ...READY_RUNTIME,
  output_mode: "local",
  outbox_writable: false,
  local_output_writable: true,
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
    delivery_mode: "power_automate",
    delivery_root: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Outbox",
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

/** These rows were accepted while Local folder delivery was selected. Their
 * immutable delivery contract must survive a later Settings change. */
const LOCAL_FLAGGED: Job[] = FLAGGED.map((row) => ({
  ...row,
  delivery_mode: "local",
  delivery_root: LOCAL_CONFIG.local_output_dir,
}));

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

// Evidence is the converted markdown at cache_dir/<sha>.md. It exists ONLY for
// files that got past conversion, and get_evidence (lib.rs:552) surfaces the
// std::fs read error when it does not — so this is keyed per file rather than
// being one constant every card shares. A shared constant put "Dates found in
// the document: 2026-03-31, …" on a card whose flag reason is "The text could
// not be read", which is a state the backend cannot produce, and it meant a bug
// harvesting date chips from the wrong document would have screenshotted clean.
/** One file in flight, last touched just under the stall threshold
 *  (per_file_wall_clock_secs is 180, so paintActivity calls it stalled at 540s).
 *  Evaluated when the module loads, i.e. immediately before the app boots, so
 *  the bar reads "Working on" on the first paint and can only turn into
 *  "Stalled" if something re-evaluates it against the clock. */
const STALLING_QUEUE: Job[] = [
  job({
    sha256: "8".repeat(64),
    original_name: "2019 payroll batch 041.pdf",
    original_path:
      "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\2019\\2019 payroll batch 041.pdf",
    state: "filtered",
    final_filename: null,
    description: null,
    updated_at: new Date(Date.now() - 532_000).toISOString(),
  }),
];

const EVIDENCE_BY_SHA: Record<string, string> = {
  // scan0417.pdf — filed cleanly, kept here because the queue rows reference it.
  ["a".repeat(64)]:
    "# Termination Notice\n\nAcme Corporation\n1 Industrial Way\n\n11 March 2026\n\n" +
    "Dear Mr Smith,\n\nThis letter confirms the termination of your employment with effect from\n" +
    "2026-03-31, following the consultation meeting held on 4 March 2026.\n\n" +
    "Yours sincerely,\nH. Okonkwo\nHead of People\n",
  // IMG_20260214_113355.jpg — OCR'd, then flagged DATE_NOT_IN_EVIDENCE:2026-02-14.
  // The proposed date is deliberately absent from this text; the dates that ARE
  // here are the ones the chips must offer.
  ["c".repeat(64)]:
    "# Tenancy Agreement\n\nRiverside Court, Unit 4B\n\nBetween Contoso Property Services and\n" +
    "A. Patel.\n\nThis agreement is dated 9 February 2026 and the tenancy begins on 2026-03-01\n" +
    "for a term of twelve months, ending 2027-02-28.\n\nSigned ......................\n",
  // notes.docx — converted fine; the model's subject was the problem, not the text.
  ["f".repeat(64)]:
    "# Meeting notes\n\n4 June 2026\n\nPresent: D. Okafor, R. Lindqvist, S. Bhatt.\n\n" +
    "Agreed the Riverside handover date of 2026-06-30. Actions carried over from the\n" +
    "previous meeting on 21 May 2026 remain open.\n",
};

// Newest first, exactly as events_for returns them. Keyed per file: the six
// events below belong to one specific failure and describing every card with
// them is the same lie as the shared evidence blob.
type Event = { id: number; sha256: string; at: string; stage: string; detail: string };

function events(sha: string, rows: Array<[string, string, string]>): Event[] {
  return rows.map(([at, stage, detail], i) => ({
    id: 100 - i,
    sha256: sha,
    at,
    stage,
    detail,
  }));
}

const EVENTS_BY_SHA: Record<string, Event[]> = {
  ["c".repeat(64)]: events("c".repeat(64), [
    ["2026-07-28T09:12:04.101Z", "flag", "DATE_NOT_IN_EVIDENCE:2026-02-14"],
    ["2026-07-28T09:12:03.882Z", "name", "escalation model proposed 2026-02-14, not found in evidence"],
    ["2026-07-28T09:12:01.400Z", "name", "span mismatch, re-prompting"],
    ["2026-07-28T09:11:58.220Z", "convert", "attempt 2: ocr conf 0.44 below floor 0.60"],
    ["2026-07-28T09:11:50.010Z", "convert", "attempt 1: no embedded text layer"],
    ["2026-07-28T09:11:49.900Z", "ingest", "ingested"],
  ]),
  ["f".repeat(64)]: events("f".repeat(64), [
    ["2026-07-28T09:13:22.700Z", "flag", "BAD_SUBJECT:generic subject 'Document'"],
    ["2026-07-28T09:13:22.310Z", "name", "escalation model proposed subject 'Document'"],
    ["2026-07-28T09:13:20.980Z", "name", "primary model proposed subject 'Document', rejected as generic"],
    ["2026-07-28T09:13:19.640Z", "convert", "docx converted, 1 842 characters"],
    ["2026-07-28T09:13:19.500Z", "ingest", "ingested"],
  ]),
  ["1".repeat(64)]: events("1".repeat(64), [
    ["2026-07-28T09:15:41.220Z", "flag", "UNREADABLE:all conversion attempts exhausted"],
    ["2026-07-28T09:15:40.880Z", "convert", "attempt 3: ocr conf 0.19 below floor 0.60"],
    ["2026-07-28T09:15:31.010Z", "convert", "attempt 2: ocr conf 0.22 below floor 0.60"],
    ["2026-07-28T09:15:22.470Z", "convert", "attempt 1: no embedded text layer"],
    ["2026-07-28T09:15:22.300Z", "ingest", "ingested"],
  ]),
  ["2".repeat(64)]: events("2".repeat(64), [
    ["2026-07-28T09:16:02.900Z", "flag", "ENCRYPTED:password protected"],
    ["2026-07-28T09:16:02.740Z", "convert", "pdfium: document is password protected"],
    ["2026-07-28T09:16:02.600Z", "ingest", "ingested"],
  ]),
};

const DIAGNOSTICS = {
  app_version: "0.6.0",
  platform: "windows x86_64",
  sidecar_versions: {
    convertd: "0.4.1",
    markitdown: "0.0.1a3",
    rapidocr: "1.3.24",
    lingua: "2.0.2",
  },
};

/** What lib.rs:640-682 really returns when convertd is missing: an Ok payload
 *  with a real app_version and platform, and the probe failure folded into
 *  `sidecar_versions.error`. get_diagnostics has no failure path that rejects,
 *  so a fixture that threw was evidence of an unreachable state. */
const DIAGNOSTICS_NO_SIDECAR = {
  app_version: "0.6.0",
  platform: "windows x86_64",
  sidecar_versions: { error: "convertd is not installed" },
};

type ShaArgs = { sha256?: string };

function evidenceFor(args?: unknown): string | Error {
  const sha = ((args ?? {}) as ShaArgs).sha256 ?? "";
  const text = EVIDENCE_BY_SHA[sha];
  // Same shape as the real command: std::fs::read_to_string's error, verbatim.
  return text ?? new Error("No such file or directory (os error 2)");
}

function eventsFor(args?: unknown): Event[] {
  const sha = ((args ?? {}) as ShaArgs).sha256 ?? "";
  return EVENTS_BY_SHA[sha] ?? [];
}

const STATS_BUSY = { ingested: 12, converted: 8, named: 4, emitted: 1841, flagged: 4, per_hour: 0 };
const STATS_EMPTY = {};

type ListArgs = {
  query?: string | null;
  jobState?: string | null;
  job_state?: string | null;
  reason?: string | null;
  flag_reason?: string | null;
  oldestFirst?: boolean;
  oldest_first?: boolean;
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

function flaggedFiltered(rows: Job[], args?: unknown): Job[] {
  const a = (args ?? {}) as ListArgs;
  const reason = (a.reason ?? a.flag_reason ?? "").toString().trim();
  const matching = rows.filter((row) => !reason || String(row["flag_reason"] ?? "").split(":")[0] === reason);
  const oldest = a.oldestFirst ?? a.oldest_first ?? false;
  return [...matching].sort((left, right) => {
    const delta = Date.parse(String(left["updated_at"] ?? "")) - Date.parse(String(right["updated_at"] ?? ""));
    return oldest ? delta : -delta;
  });
}

function flaggedReasons(rows: Job[]): string[] {
  return [...new Set(rows.map((row) => String(row["flag_reason"] ?? "").split(":")[0]).filter(Boolean))].sort();
}

function base(runtime: unknown, stats: unknown, jobs: Job[], flagged: Job[]): CommandTable {
  return {
    get_config: CONFIG,
    get_runtime_status: runtime,
    run_preflight: runtime,
    get_stats: stats,
    list_jobs: (args?: unknown) => page(filtered(jobs, args), args),
    count_jobs: (args?: unknown) => filtered(jobs, args).length,
    list_flagged: (args?: unknown) => page(flaggedFiltered(flagged, args), args),
    count_flagged: (args?: unknown) => flaggedFiltered(flagged, args).length,
    list_flag_reasons: () => flaggedReasons(flagged),
    get_flagged_job: (args?: unknown) => {
      const sha = ((args ?? {}) as ShaArgs).sha256 ?? "";
      return flagged.find((row) => row["sha256"] === sha) ?? null;
    },
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
    // The backend keeps the latest terminal result so Settings can recover a
    // completion, failure, or cancellation after navigation.
    model_download_status: null,
    get_diagnostics: DIAGNOSTICS,
    get_events: eventsFor,
    get_evidence: evidenceFor,
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

/** A flagged list that really shrinks. `resubmit`, `dismiss` and `reprocess`
 *  all move the job out of JobState::Flagged in the real ledger (Emitted,
 *  Dismissed and Ingested respectively), so the next `list_flagged` cannot
 *  return it. A fixture that kept handing the same rows back would let a card
 *  for an already-filed document screenshot clean — a state the backend cannot
 *  produce, and the one that a manifest Power Automate has consumed sits
 *  behind. */
function shrinking(rows: Job[]): { live: Job[]; remove: (args?: unknown) => unknown } {
  const live = [...rows];
  const remove = (args?: unknown) => {
    const sha = ((args ?? {}) as ShaArgs).sha256 ?? "";
    const at = live.findIndex((row) => row["sha256"] === sha);
    if (at === -1) return new Error("That file has already moved on.");
    live.splice(at, 1);
    return null;
  };
  return { live, remove };
}

/** The Needs Review screen against a list that empties as it is worked. */
function reviewScenario(
  label: string,
  stats: Record<string, number>,
  runtime: unknown = READY_RUNTIME,
  rows: Job[] = FLAGGED
): Scenario {
  const { live, remove } = shrinking(rows);
  const s = scenario(label, runtime, stats, QUEUE, live, { view: "flagged" });
  return {
    ...s,
    commands: {
      ...s.commands,
      get_stats: () => ({ ...stats, flagged: live.length }),
      resubmit: remove,
      dismiss: remove,
      reprocess: remove,
    },
  };
}

/** A flagged backlog deeper than one fetch, backed by a list that really
 *  mutates — see `shrinking`. */
function reviewScale(count: number): Scenario {
  const rows: Job[] = Array.from({ length: count }, (_, i) =>
    job({
      sha256: ("d" + i.toString(16)).padStart(64, "0"),
      original_name: `flagged-${String(i).padStart(3, "0")}.pdf`,
      original_path: `C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing\\2021\\flagged-${String(i).padStart(3, "0")}.pdf`,
      state: "flagged",
      flag_reason: "DATE_NOT_IN_EVIDENCE:2021-04-02",
      quarantine_path: `C:\\ProgramData\\BackLog\\Quarantine\\flagged-${String(i).padStart(3, "0")}.pdf`,
      proposed_date: "",
      proposed_subject: `Archive record ${i}`,
      description: "",
      final_filename: null,
    })
  );
  const { live, remove } = shrinking(rows);
  return {
    label: "60 files needing review",
    view: "flagged",
    commands: {
      ...base(READY_RUNTIME, {}, live, live),
      get_stats: () => ({ emitted: 940, flagged: live.length, per_hour: 0 }),
      list_flagged: (args?: unknown) => page(flaggedFiltered(live, args), args),
      count_jobs: (args?: unknown) => {
        const a = (args ?? {}) as ListArgs;
        const state = a.jobState ?? a.job_state ?? null;
        if (state === "flagged") return live.length;
        return filtered(live, args).length;
      },
      dismiss: remove,
      resubmit: remove,
      reprocess: remove,
    },
  };
}

/** A reason beyond the old first 25 rows catches client-only review filtering. */
function reviewReasons(): Scenario {
  const rows: Job[] = Array.from({ length: 30 }, (_, i) =>
    job({
      sha256: ("e" + i.toString(16)).padStart(64, "0"),
      original_name: `reason-${String(i).padStart(2, "0")}.pdf`,
      state: "flagged",
      flag_reason: i === 29 ? "ENCRYPTED:password protected" : "BAD_SUBJECT:generic subject",
      quarantine_path: `C:\\ProgramData\\BackLog\\Quarantine\\reason-${String(i).padStart(2, "0")}.pdf`,
      proposed_subject: "Document",
      final_filename: null,
    })
  );
  return scenario("Review reason beyond initial page", READY_RUNTIME, { emitted: 12, flagged: 30 }, [], rows, {
    view: "flagged",
  });
}

/** A true caught-up queue has only completed history, and its history matches
 * the counters shown in the header rather than pretending the ledger is empty. */
function caughtUpHistory(deliveryMode: "local" | "power_automate" = "power_automate"): Job[] {
  return Array.from({ length: 22 }, (_, i) => {
    const flagged = i >= 18;
    const suffix = i.toString(16).padStart(2, "0");
    return job({
      sha256: (flagged ? "8" : "7").repeat(62) + suffix,
      original_name: flagged ? `needs-a-person-${i - 17}.pdf` : `filed-invoice-${i + 1}.pdf`,
      state: flagged ? "flagged" : "emitted",
      flag_reason: flagged ? "BAD_SUBJECT:generic subject" : null,
      quarantine_path: flagged ? `C:\\ProgramData\\BackLog\\Quarantine\\needs-a-person-${i - 17}.pdf` : null,
      delivery_mode: deliveryMode,
      delivery_root: deliveryMode === "local" ? LOCAL_CONFIG.local_output_dir : CONFIG.outbox_dir,
    });
  });
}

/** Cancellation rejects the active command before the terminal event reaches
 * the page, the ordering that used to turn a normal cancellation into a toast. */
function cancellingDownloadScenario(): Scenario {
  let rejectDownload: ((reason?: unknown) => void) | null = null;
  const s = scenario("Cancelling model download", BLOCKED_RUNTIME, STATS_EMPTY, [], []);
  return {
    ...s,
    commands: {
      ...s.commands,
      download_models: () => new Promise<void>((_resolve, reject) => { rejectDownload = reject; }),
      cancel_model_download: () => {
        rejectDownload?.(new Error("Download cancelled."));
        return null;
      },
    },
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
      get_config: LEGACY_EMPTY_CONFIG,
      // Nothing is installed yet on a fresh machine, so the convertd probe
      // behind the version line fails — but the command itself still succeeds
      // and reports the app version, which is the line the pilot runbook asks
      // the operator to read off this very screen.
      get_diagnostics: DIAGNOSTICS_NO_SIDECAR,
    },
  },

  /** First-run save action: folders are chosen but have not reached disk. */
  "first-run-save": {
    ...scenario("First run, save folders then check", UNCHECKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(UNCHECKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      get_config: EMPTY_CONFIG,
      set_config: null,
      run_preflight: BLOCKED_RUNTIME,
      get_diagnostics: DIAGNOSTICS_NO_SIDECAR,
    },
  },

  "first-run-preflight-error": {
    ...scenario("First run, live check unavailable", UNCHECKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(UNCHECKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      get_config: EMPTY_CONFIG,
      set_config: null,
      run_preflight: () => new Error("the readiness check could not reach BackLog"),
      get_diagnostics: DIAGNOSTICS_NO_SIDECAR,
    },
  },

  /** Direct local delivery has no Outbox requirement or manifest consumer. */
  "local-ready": {
    ...scenario("Local Output ready", LOCAL_READY_RUNTIME, STATS_EMPTY, [], [], { view: "settings" }),
    commands: {
      ...withFlaggedCount(base(LOCAL_READY_RUNTIME, STATS_EMPTY, [], []), [], []),
      get_config: LOCAL_CONFIG,
    },
  },

  "local-review": {
    ...reviewScenario("Local Output correction", { emitted: 12, flagged: 4 }, LOCAL_READY_RUNTIME, LOCAL_FLAGGED),
    commands: {
      ...reviewScenario("Local Output correction", { emitted: 12, flagged: 4 }, LOCAL_READY_RUNTIME, LOCAL_FLAGGED).commands,
      get_config: LOCAL_CONFIG,
    },
  },

  /** Preflight has run and failed: the "why can't I start?" state. */
  blocked: scenario("Preflight failed, models missing", BLOCKED_RUNTIME, STATS_EMPTY, [], []),

  /** The review backlog — the screen a user actually spends time in. */
  review: reviewScenario("Needs Review backlog", { ...STATS_BUSY, flagged: 4 }),

  /** The first evidence read fails transiently, then recovers on retry. */
  "review-evidence-retry": (() => {
    const s = reviewScenario("Evidence read recovers on retry", { ...STATS_BUSY, flagged: 4 });
    let failed = false;
    return {
      ...s,
      commands: {
        ...s.commands,
        get_evidence: (args?: unknown) => {
          const sha = ((args ?? {}) as ShaArgs).sha256 ?? "";
          if (sha === "c".repeat(64) && !failed) {
            failed = true;
            return new Error("temporary evidence read failure");
          }
          return evidenceFor(args);
        },
      },
    };
  })(),

  /** Backend is down: every list command rejects. */
  errors: {
    label: "Backend errors on every read",
    commands: {
      ...base(READY_RUNTIME, STATS_BUSY, [], []),
      list_jobs: () => new Error("ledger is locked by another process (code 5)"),
      list_flagged: () => new Error("ledger is locked by another process (code 5)"),
      count_jobs: () => new Error("ledger is locked by another process (code 5)"),
      count_flagged: () => new Error("ledger is locked by another process (code 5)"),
      list_flag_reasons: () => new Error("ledger is locked by another process (code 5)"),
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
      count_flagged: () => new Promise(() => {}),
      list_flag_reasons: () => new Promise(() => {}),
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

  "download-cancelling": cancellingDownloadScenario(),

  /** A cancelled transfer must be safe to pick up from its retained .part files. */
  "download-cancelled": {
    ...scenario("Model download cancelled", BLOCKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(BLOCKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      model_download_status: {
        ok: false, cancelled: true, error: null, finished_at: "2026-07-30T18:00:00Z",
      },
    },
  },

  /** A transient transfer failure must expose the same resumable path. */
  "download-failed": {
    ...scenario("Model download failed", BLOCKED_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(BLOCKED_RUNTIME, STATS_EMPTY, [], []), [], []),
      model_download_status: {
        ok: false, cancelled: false, error: "The network connection stopped.", finished_at: "2026-07-30T18:00:00Z",
      },
    },
  },

  /** A terminal success remains visible after leaving and returning to Settings. */
  "download-completed": {
    ...scenario("Model download completed", OPTIONAL_MODEL_RUNTIME, STATS_EMPTY, [], []),
    commands: {
      ...withFlaggedCount(base(OPTIONAL_MODEL_RUNTIME, STATS_EMPTY, [], []), [], []),
      // Opening Settings restores the retained completion and must run a live
      // check that discovers the newly installed backup model.
      run_preflight: READY_RUNTIME,
      model_download_status: {
        ok: true, cancelled: false, error: null, finished_at: "2026-07-30T18:00:00Z",
      },
    },
  },

  "optional-backup-model": scenario(
    "Bundled primary with optional backup missing",
    OPTIONAL_MODEL_RUNTIME,
    STATS_EMPTY,
    [],
    []
  ),

  /** Processing has no work, but a reviewer still has documents to resolve. */
  "caught-up-reviews": scenario(
    "Processing caught up with reviews remaining",
    READY_RUNTIME,
    { emitted: 18, flagged: 4, per_hour: 0 },
    caughtUpHistory(),
    FLAGGED
  ),

  /** Local delivery must never render the Power Automate-only queue summary. */
  "local-caught-up-reviews": {
    ...scenario(
      "Local Output caught up with reviews remaining",
      LOCAL_READY_RUNTIME,
      { emitted: 18, flagged: 4, per_hour: 0 },
      caughtUpHistory("local"),
      LOCAL_FLAGGED
    ),
    commands: {
      ...scenario(
        "Local Output caught up with reviews remaining",
        LOCAL_READY_RUNTIME,
        { emitted: 18, flagged: 4, per_hour: 0 },
        caughtUpHistory("local"),
        LOCAL_FLAGGED
      ).commands,
      get_config: LOCAL_CONFIG,
    },
  },

  /** The queue can briefly have active counters before its first row arrives;
   * its empty state must still not direct a Local user to SharePoint intake. */
  "local-queue-awaiting-first-row": {
    ...scenario("Local Output queue awaiting first row", LOCAL_READY_RUNTIME, STATS_BUSY, [], []),
    commands: {
      ...scenario("Local Output queue awaiting first row", LOCAL_READY_RUNTIME, STATS_BUSY, [], []).commands,
      get_config: LOCAL_CONFIG,
    },
  },

  "review-reasons": reviewReasons(),

  /** The evidence pane and the per-attempt diagnosis, both expanded. */
  "review-detail": reviewScenario("Review card, everything expanded", { ...STATS_BUSY, flagged: 4 }),

  /** Several failures at once — the case where toasts used to land exactly on
   *  top of each other and only the last one was legible, and then (once they
   *  stacked) grew off the top of the window and covered the Start button.
   *
   *  start_pipeline reports whichever precondition it hit first, so a machine
   *  with more than one fault gives a different message as each press gets
   *  further; every string below is one lib.rs really returns. Pressing Start
   *  again is what a user does when nothing appears to happen, so both the
   *  repeat (dedupe) and the variety (the cap) are the real case. */
  toasts: {
    ...scenario("Repeated Start failures", READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),
    commands: {
      ...withFlaggedCount(base(READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED), QUEUE, FLAGGED),
      start_pipeline: (() => {
        const messages = [
          "BackLog is not ready yet: the naming engine's start-up and the backup model file " +
            "could not be verified.",
          "The folder BackLog watches for new documents does not exist yet: " +
            "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing.",
          "BackLog's record of processed files is locked by another process (code 5).",
          "The part of BackLog that reads your documents did not answer.",
        ];
        let n = 0;
        return () => new Error(messages[n++ % messages.length]);
      })(),
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

  /** More flagged files than one fetch returns, with a ledger that really
   *  shrinks as they are worked. This is the only way to prove that every file
   *  is reachable: the flagged set is a queue being emptied from the head, and
   *  an offset walked forward over it skips whatever was resolved meanwhile. */
  "review-scale": reviewScale(60),

  /** Running, with the file at the head of the queue about to cross the stall
   *  threshold (per_file_wall_clock_secs * 3). Nothing else happens: no event
   *  ever arrives, so the only thing that can make the bar tell the truth is
   *  the clock. */
  stalling: {
    ...scenario(
      "About to look stalled",
      { ...READY_RUNTIME, running: true },
      { ingested: 3, converted: 1, emitted: 41, flagged: 2, per_hour: 0 },
      STALLING_QUEUE,
      FLAGGED
    ),
  },

  /** The pipeline stops without the run button being touched: it crashed, the
   *  ledger locked, or it was paused from somewhere other than this window.
   *  `get_runtime_status` is the ONLY thing that can ever tell the UI, so this
   *  scenario hands back running:true for the boot read and false for every
   *  read after it — a UI that only re-reads on a button press stays stuck on
   *  "Pause" over a dead pipeline for the rest of the session. */
  "external-stop": (() => {
    let reads = 0;
    const status = () => ({ ...READY_RUNTIME, running: reads++ < 1 });
    const s = scenario(
      "Pipeline stopped from outside",
      { ...READY_RUNTIME, running: true },
      { ingested: 3, converted: 1, emitted: 41, flagged: 2, per_hour: 0 },
      RUNNING_QUEUE,
      FLAGGED
    );
    return { ...s, commands: { ...s.commands, get_runtime_status: status, run_preflight: status } };
  })(),

  /** A 5,000-file backfill, to expose problems that only appear at size. */
  scale: scenario(
    "5,000-file backfill",
    { ...READY_RUNTIME, running: true },
    { ingested: 210, converted: 90, named: 40, emitted: 4611, flagged: 49, per_hour: 340 },
    BIG_QUEUE,
    FLAGGED
  ),
};
