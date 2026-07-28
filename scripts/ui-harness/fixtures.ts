// Scenario fixtures for the browser UI harness. Each scenario is the set of
// Tauri command responses that puts the real frontend into one specific state.
//
// Keep these HONEST: shapes must match what the Rust commands actually return
// (src-tauri/src/lib.rs, preflight.rs, ledger.rs), or the harness will happily
// show a UI that cannot exist. When a command's return type changes, change it
// here too — a drifting fixture is worse than no fixture.

export type CommandTable = Record<string, unknown | ((args?: unknown) => unknown)>;

export type Scenario = {
  label: string;
  commands: CommandTable;
  pickedPath?: string;
  update?: unknown;
};

const CONFIG = {
  processing_dir: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Processing",
  outbox_dir: "C:\\Users\\dana\\OneDrive - Contoso\\BackLog\\Outbox",
  quarantine_dir: "C:\\ProgramData\\BackLog\\Quarantine",
  cache_dir: "C:\\Users\\dana\\AppData\\Roaming\\ai.sonomos.backlog\\cache",
  llama_port: 871,
  slm_primary_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-0.6B-Q8_0.gguf",
  slm_escalation_gguf: "C:\\ProgramData\\BackLog\\models\\Qwen3-1.7B-Q8_0.gguf",
  slm_parallel: 2,
  evidence_token_budget: 1600,
  ettin_model_dir: "",
  convert_workers: 4,
  manifest_emit_per_min: 120,
  max_head_pages: 3,
  max_tail_pages: 1,
  max_filename_len: 120,
  max_stage_attempts: 3,
  per_file_wall_clock_secs: 180,
  retain_cache: false,
  cache_ttl_days: 7,
};

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
  grammar_found: true,
  primary_model_found: true,
  escalation_model_found: true,
  offline_runtime: true,
  checked_at: "2026-07-28T09:14:00Z",
  problems: [] as unknown[],
};

const BLOCKED_RUNTIME = {
  ...READY_RUNTIME,
  configured: false,
  primary_model_found: false,
  escalation_model_found: false,
  sidecar_ok: false,
  problems: [
    {
      field: "slm_primary_gguf",
      code: "MODEL_MISSING",
      message: "The primary model file is not on this machine yet. Use Download models below.",
      severity: "error",
    },
    {
      field: "convertd",
      code: "SIDECAR_NO_PING",
      message: "The conversion sidecar did not answer within 5 seconds.",
      severity: "error",
    },
  ],
};

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
  grammar_found: false,
  primary_model_found: false,
  escalation_model_found: false,
  checked_at: null,
  problems: [] as unknown[],
};

function job(over: Record<string, unknown>) {
  return {
    sha256: "0".repeat(64),
    original_name: "scan0417.pdf",
    ext: "pdf",
    state: "emitted",
    flag_reason: null,
    proposed_date: "2026-03-11",
    date_source: "document",
    proposed_subject: "Termination Notice for John Smith",
    description: "Letter from Acme Corporation notifying John Smith of employment termination.",
    final_filename: "2026-03-11 Termination Notice for John Smith.pdf",
    doc_type: "letter",
    soft_flags: null,
    updated_at: "2026-07-28T09:12:04Z",
    ...over,
  };
}

const QUEUE = [
  job({ sha256: "a".repeat(64), original_name: "scan0417.pdf" }),
  job({
    sha256: "b".repeat(64),
    original_name: "Q3 board pack FINAL v2.docx",
    ext: "docx",
    state: "converted",
    final_filename: null,
    doc_type: "report",
  }),
  job({
    sha256: "c".repeat(64),
    original_name: "IMG_20260214_113355.jpg",
    ext: "jpg",
    state: "flagged",
    flag_reason: "DATE_NOT_IN_EVIDENCE",
    final_filename: null,
    doc_type: "scan",
  }),
  job({
    sha256: "d".repeat(64),
    original_name: "Invoice 88213.pdf",
    ext: "pdf",
    state: "emitted",
    proposed_date: "2026-01-30",
    proposed_subject: "Invoice 88213 from Northwind Traders",
    final_filename: "2026-01-30 Invoice 88213 from Northwind Traders.pdf",
    doc_type: "invoice",
    soft_flags: "DATE_SOURCE_CORRECTED:metadata->document",
  }),
  job({
    sha256: "e".repeat(64),
    original_name: "policy-handbook.pdf",
    ext: "pdf",
    state: "named",
    final_filename: null,
    date_source: "metadata",
    doc_type: "policy",
  }),
];

const FLAGGED = [
  job({
    sha256: "c".repeat(64),
    original_name: "IMG_20260214_113355.jpg",
    ext: "jpg",
    state: "flagged",
    flag_reason: "DATE_NOT_IN_EVIDENCE",
    proposed_date: "",
    proposed_subject: "Signed Lease Agreement Riverside Unit",
    final_filename: null,
  }),
  job({
    sha256: "f".repeat(64),
    original_name: "notes.docx",
    ext: "docx",
    state: "flagged",
    flag_reason: "BAD_SUBJECT",
    proposed_subject: "Document",
    description: "",
    final_filename: null,
  }),
  job({
    sha256: "1".repeat(64),
    original_name: "kontrakt-2026.pdf",
    ext: "pdf",
    state: "flagged",
    flag_reason: "OCR_LOW_CONFIDENCE",
    proposed_subject: "",
    description: "",
    proposed_date: "",
    final_filename: null,
  }),
];

const STATS_BUSY = { ingested: 12, converted: 8, named: 4, emitted: 1841, flagged: 3 };
const STATS_EMPTY = {};

function base(runtime: unknown, stats: unknown, jobs: unknown[], flagged: unknown[]): CommandTable {
  return {
    get_config: CONFIG,
    get_runtime_status: runtime,
    run_preflight: runtime,
    get_stats: stats,
    list_jobs: jobs,
    list_flagged: flagged,
    set_config: null,
    set_paused: null,
    start_pipeline: null,
    resubmit: null,
    download_models: null,
    get_evidence:
      "# Termination Notice\n\nAcme Corporation\n1 Industrial Way\n\n11 March 2026\n\n" +
      "Dear Mr Smith,\n\nThis letter confirms the termination of your employment...\n",
  };
}

export const SCENARIOS: Record<string, Scenario> = {
  /** Steady state: configured, preflight green, a real queue behind it. */
  ready: {
    label: "Ready, queue populated",
    commands: base(READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),
  },
  /** Very first launch: nothing configured, nothing checked, no files. */
  "first-run": {
    label: "First run, nothing configured",
    commands: base(
      { ...UNCHECKED_RUNTIME, configured: false },
      STATS_EMPTY,
      [],
      []
    ),
  },
  /** Preflight has run and failed: the "why can't I start?" state. */
  blocked: {
    label: "Preflight failed, models missing",
    commands: base(BLOCKED_RUNTIME, STATS_EMPTY, [], []),
  },
  /** The review backlog — the screen a user actually spends time in. */
  review: {
    label: "Needs Review backlog",
    commands: base(READY_RUNTIME, { ...STATS_BUSY, flagged: 3 }, QUEUE, FLAGGED),
  },
  /** Backend is down: every list command rejects. */
  errors: {
    label: "Backend errors on every read",
    commands: {
      ...base(READY_RUNTIME, STATS_BUSY, [], []),
      list_jobs: () => new Error("ledger is locked by another process (code 5)"),
      list_flagged: () => new Error("ledger is locked by another process (code 5)"),
    },
  },
  /** An update is waiting — exercises the banner. */
  update: {
    label: "Update available",
    commands: base(READY_RUNTIME, STATS_BUSY, QUEUE, FLAGGED),
    update: {
      version: "0.3.0",
      downloadAndInstall: async () => {
        /* left pending so the banner's busy state can be screenshotted */
        await new Promise(() => {});
      },
    },
  },
  /** A queue big enough to expose scale problems (no search/sort/paging). */
  scale: {
    label: "5,000-file backfill",
    commands: base(
      { ...READY_RUNTIME, running: true },
      { ingested: 210, converted: 90, named: 40, emitted: 4611, flagged: 149 },
      Array.from({ length: 500 }, (_, i) =>
        job({
          sha256: i.toString(16).padStart(64, "0"),
          original_name: `batch-${String(i).padStart(4, "0")}.pdf`,
          state: i % 17 === 0 ? "flagged" : i % 5 === 0 ? "converted" : "emitted",
          flag_reason: i % 17 === 0 ? "DATE_NOT_IN_EVIDENCE" : null,
        })
      ),
      FLAGGED
    ),
  },
};
