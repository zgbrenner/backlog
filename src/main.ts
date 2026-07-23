import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

type Job = {
  sha256: string;
  original_name: string;
  ext: string;
  state: string;
  flag_reason: string | null;
  proposed_date: string | null;
  date_source: string | null;
  proposed_subject: string | null;
  description: string | null;
  final_filename: string | null;
  doc_type: string | null;
  soft_flags: string | null;
  updated_at: string;
};

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
  manifest_emit_per_min: number;
  max_head_pages: number;
  max_tail_pages: number;
  max_filename_len: number;
  max_stage_attempts: number;
  per_file_wall_clock_secs: number;
  retain_cache: boolean;
  cache_ttl_days: number;
};

type RuntimeProblem = {
  field: string;
  code: string;
  message: string;
  severity: "error" | "warning";
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
  grammar_found: boolean;
  primary_model_found: boolean;
  escalation_model_found: boolean;
  offline_runtime: boolean;
  checked_at: string | null;
  problems: RuntimeProblem[];
};

// Boolean pass/fail checks surfaced in the Readiness panel, in display order.
const READINESS_CHECKS: Array<[label: string, key: keyof RuntimeStatus]> = [
  ["Processing folder is readable", "processing_dir_ready"],
  ["Outbox folder is writable", "outbox_writable"],
  ["Quarantine folder is writable", "quarantine_writable"],
  ["Cache folder is writable", "cache_writable"],
  ["Conversion sidecar (convertd) is installed", "sidecar_found"],
  ["Conversion sidecar answers ping", "sidecar_ok"],
  ["llama-server is installed", "llama_server_found"],
  ["Naming grammar is installed", "grammar_found"],
  ["Primary SLM model file is present", "primary_model_found"],
  ["Escalation SLM model file is present", "escalation_model_found"],
];

function uncheckedRuntime(): RuntimeStatus {
  return {
    configured: false,
    checked: false,
    running: false,
    paused: false,
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
    offline_runtime: true,
    checked_at: null,
    problems: [],
  };
}

const app = document.getElementById("app")!;
let cfg: Config | null = null;
let running = false;
let paused = false;
let runtime: RuntimeStatus = uncheckedRuntime();
let view: "queue" | "flagged" | "settings" = "queue";

const STATE_BADGE: Record<string, string> = {
  ingested: "b-wait", converted: "b-wait", filtered: "b-wait", named: "b-wait",
  validated: "b-wait", emitted: "b-ok", flagged: "b-flag",
};

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

// Non-blocking, CSP-safe error surface for user-initiated actions.
function showError(msg: string): void {
  const toast = el(`<div class="toast" role="alert"></div>`);
  toast.textContent = msg;
  document.body.appendChild(toast);
  setTimeout(() => toast.remove(), 6000);
}

// Refreshes the in-memory runtime status from the backend. `live` runs the
// full machine check (folders, binaries, models, a bounded sidecar ping);
// otherwise this just re-reads whatever was last cached there. On failure
// the previous in-memory status is kept rather than reset, so a transient
// IPC hiccup can't make a Ready panel flash back to unchecked.
async function refreshRuntime(live: boolean): Promise<void> {
  try {
    runtime = await invoke<RuntimeStatus>(live ? "run_preflight" : "get_runtime_status");
  } catch (e) {
    showError(String(e));
  }
}

async function render() {
  const stats = await invoke<Record<string, number>>("get_stats").catch(
    (): Record<string, number> => ({})
  );
  const total = Object.values(stats).reduce((a, b) => a + (b as number), 0);
  app.innerHTML = "";
  app.appendChild(el(`
    <div class="shell">
      <header>
        <div class="brand">Back<span>Log</span></div>
        <nav>
          <button data-v="queue" class="${view === "queue" ? "on" : ""}">Queue</button>
          <button data-v="flagged" class="${view === "flagged" ? "on" : ""}">Needs Review
            ${stats["flagged"] ? `<span class="pill">${stats["flagged"]}</span>` : ""}</button>
          <button data-v="settings" class="${view === "settings" ? "on" : ""}">Settings</button>
        </nav>
        <div class="run">
          <span class="readiness-chip ${runtime.configured ? "ready" : "blocked"}">${runtime.configured ? "Ready" : runtime.checked ? "Blocked" : "Unchecked"}</span>
          <span class="stats">${total} files · ${stats["emitted"] ?? 0} done · ${stats["flagged"] ?? 0} flagged</span>
          <button id="runbtn" class="${running ? (paused ? "paused" : "live") : "start"}"
            ${!running && !runtime.configured ? 'disabled title="Run preflight in Settings before starting."' : ""}>
            ${running ? (paused ? "Resume" : "Pause") : "Start"}
          </button>
        </div>
      </header>
      <main id="content"></main>
    </div>
  `));

  app.querySelectorAll("nav button").forEach((b) =>
    b.addEventListener("click", () => { view = (b as HTMLElement).dataset.v as typeof view; render(); })
  );
  document.getElementById("runbtn")!.addEventListener("click", onRunButton);

  const content = document.getElementById("content")!;
  if (view === "queue") await renderQueue(content);
  else if (view === "flagged") await renderFlagged(content);
  else renderSettings(content);
}

async function onRunButton() {
  if (!running) {
    try {
      await invoke("start_pipeline");
      running = true;
    } catch (e) {
      showError(String(e));
      view = "settings";
    }
    // start_pipeline runs and caches its own preflight check even on
    // failure, so pick up whatever it found for the Readiness panel.
    await refreshRuntime(false);
  } else {
    // Only flip local state once the backend confirms, so the button never
    // desyncs from the pipeline on failure.
    const next = !paused;
    try {
      await invoke("set_paused", { paused: next });
      paused = next;
    } catch (e) {
      showError(String(e));
    }
  }
  render();
}

async function renderQueue(root: HTMLElement) {
  let jobs: Job[];
  try {
    jobs = await invoke<Job[]>("list_jobs", { limit: 500 });
  } catch (e) {
    const err = el(`<div class="empty err-state"></div>`);
    err.textContent = `Couldn't load the queue: ${String(e)}`;
    root.appendChild(err);
    return;
  }
  if (!jobs.length) {
    root.appendChild(el(`<div class="empty">No files yet. Configure folders in Settings, hit Start, and drop files into the Processing folder (or let Flow 1 do it).</div>`));
    return;
  }
  const rows = jobs.map((j) => `
    <tr>
      <td class="mono" title="${esc(j.sha256)}">${esc(j.original_name)}</td>
      <td>${esc(j.final_filename ?? "")}</td>
      <td>${esc(j.doc_type ?? "")}</td>
      <td><span class="badge ${STATE_BADGE[j.state] ?? "b-wait"}">${esc(j.state)}</span>
          ${j.soft_flags ? `<span class="soft" title="${esc(j.soft_flags)}">!</span>` : ""}</td>
    </tr>`).join("");
  root.appendChild(el(`
    <table>
      <thead><tr><th>Original</th><th>New name</th><th>Type</th><th>State</th></tr></thead>
      <tbody>${rows}</tbody>
    </table>`));
}

async function renderFlagged(root: HTMLElement) {
  let jobs: Job[];
  try {
    jobs = await invoke<Job[]>("list_flagged");
  } catch (e) {
    const err = el(`<div class="empty err-state"></div>`);
    err.textContent = `Couldn't load the review queue: ${String(e)}`;
    root.appendChild(err);
    return;
  }
  if (!jobs.length) {
    root.appendChild(el(`<div class="empty">Nothing needs review. As it should be.</div>`));
    return;
  }
  for (const j of jobs) {
    const card = el(`
      <div class="card">
        <div class="card-head">
          <strong>${esc(j.original_name)}</strong>
          <code class="reason">${esc(j.flag_reason ?? "unknown")}</code>
        </div>
        <div class="fields">
          <label>Date <input type="date" name="date" value="${esc(j.proposed_date ?? "")}"></label>
          <label>Subject <input name="subject" placeholder="3-8 words" value="${esc(j.proposed_subject ?? "")}"></label>
          <label class="wide">Description <input name="description" placeholder="One sentence." value="${esc(j.description ?? "")}"></label>
        </div>
        <div class="card-actions">
          <button class="ghost" data-act="evidence">View text</button>
          <button data-act="resubmit">Approve and re-emit</button>
          <span class="err"></span>
        </div>
        <pre class="evidence" hidden></pre>
      </div>`);
    card.querySelector('[data-act="evidence"]')!.addEventListener("click", async () => {
      const pre = card.querySelector(".evidence") as HTMLElement;
      if (pre.hidden) {
        pre.textContent = await invoke<string>("get_evidence", { sha256: j.sha256 })
          .catch(() => "(no cached text; file failed before conversion)");
      }
      pre.hidden = !pre.hidden;
    });
    card.querySelector('[data-act="resubmit"]')!.addEventListener("click", async () => {
      const get = (n: string) => (card.querySelector(`[name="${n}"]`) as HTMLInputElement).value.trim();
      const err = card.querySelector(".err") as HTMLElement;
      err.textContent = "";
      try {
        await invoke("resubmit", {
          sha256: j.sha256, date: get("date"), subject: get("subject"), description: get("description"),
        });
        card.remove();
      } catch (e) {
        err.textContent = String(e);
      }
    });
    root.appendChild(card);
  }
}

function renderReadinessPanel(): HTMLElement {
  const rows = READINESS_CHECKS.map(([label, key]) => {
    const passed = Boolean(runtime[key]);
    return `
      <li class="check-row ${passed ? "check-pass" : "check-fail"}">
        <span>${esc(label)}</span>
        <strong>${passed ? "Ready" : "Blocked"}</strong>
      </li>`;
  }).join("");

  const problemsHtml = runtime.problems.length
    ? `<div class="problem-box">
        <strong>Action needed</strong>
        <ul>${runtime.problems
          .map((p) => `<li><code>${esc(p.field)}</code><span>${esc(p.message)}</span></li>`)
          .join("")}</ul>
      </div>`
    : runtime.checked
      ? `<div class="ready-box">All checks passed. BackLog is ready to start.</div>`
      : "";

  const lastChecked = runtime.checked_at
    ? `Last checked ${esc(new Date(runtime.checked_at).toLocaleString())}`
    : "Not checked yet. Run preflight before starting BackLog.";

  const panel = el(`
    <section class="preflight-panel">
      <div class="section-head">
        <div>
          <h2>Readiness</h2>
          <p class="dim-note">${lastChecked}</p>
        </div>
        <button type="button" id="preflight-button" class="ghost">Run preflight</button>
      </div>
      <ul class="check-list">${rows}</ul>
      ${problemsHtml}
    </section>`);

  panel.querySelector("#preflight-button")!.addEventListener("click", async (ev) => {
    const button = ev.currentTarget as HTMLButtonElement;
    button.disabled = true;
    button.textContent = "Checking…";
    await refreshRuntime(true);
    render();
  });

  return panel;
}

function renderSettings(root: HTMLElement) {
  if (!cfg) return;
  const c = cfg;
  root.appendChild(renderReadinessPanel());
  const folder = (label: string, key: keyof Config) => `
    <label class="wide">${label}
      <div class="pick"><input name="${key}" value="${esc(String(c[key] ?? ""))}">
      <button class="ghost" data-pick="${key}">Browse</button></div>
    </label>`;
  const root2 = el(`
    <form class="settings">
      <h2>Folders</h2>
      ${folder("Processing folder (OneDrive-synced; Flow 1 target)", "processing_dir")}
      ${folder("Outbox folder (OneDrive-synced; manifests go to _manifests)", "outbox_dir")}
      ${folder("Quarantine folder (local)", "quarantine_dir")}
      <h2>Models</h2>
      ${folder("Primary GGUF (LFM2.5-350M)", "slm_primary_gguf")}
      ${folder("Escalation GGUF (LFM2.5-1.2B-Instruct)", "slm_escalation_gguf")}
      ${folder("Ettin model dir (blank = disabled)", "ettin_model_dir")}
      <h2>Tuning</h2>
      <div class="grid3">
        <label>Convert workers <input name="convert_workers" type="number" min="1" max="12" value="${c.convert_workers}"></label>
        <label>SLM parallel <input name="slm_parallel" type="number" min="1" max="8" value="${c.slm_parallel}"></label>
        <label>Evidence tokens <input name="evidence_token_budget" type="number" min="400" max="4000" value="${c.evidence_token_budget}"></label>
        <label>Manifests/min (0 = unlimited) <input name="manifest_emit_per_min" type="number" min="0" value="${c.manifest_emit_per_min}"></label>
        <label>Max attempts/stage <input name="max_stage_attempts" type="number" min="1" max="5" value="${c.max_stage_attempts}"></label>
        <label>Wall clock/file (s) <input name="per_file_wall_clock_secs" type="number" min="30" value="${c.per_file_wall_clock_secs}"></label>
      </div>
      <div class="card-actions">
        <button type="submit">Save settings</button>
        <span class="err"></span>
      </div>
    </form>`);
  root2.querySelectorAll("[data-pick]").forEach((b) =>
    b.addEventListener("click", async (ev) => {
      ev.preventDefault();
      const key = (b as HTMLElement).dataset.pick!;
      const isFile = key.includes("gguf");
      const sel = await open({ directory: !isFile, multiple: false });
      if (typeof sel === "string") {
        (root2.querySelector(`[name="${key}"]`) as HTMLInputElement).value = sel;
      }
    })
  );
  root2.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const val = (n: string) => (root2.querySelector(`[name="${n}"]`) as HTMLInputElement).value;
    // Clamp numeric fields to the input's own min/max so a cleared or
    // out-of-range field can't silently persist a pipeline-stalling 0.
    const num = (n: string) => {
      const input = root2.querySelector(`[name="${n}"]`) as HTMLInputElement;
      const min = input.min !== "" ? parseInt(input.min, 10) : Number.NEGATIVE_INFINITY;
      const max = input.max !== "" ? parseInt(input.max, 10) : Number.POSITIVE_INFINITY;
      let x = parseInt(input.value, 10);
      if (Number.isNaN(x)) x = Number.isFinite(min) ? min : 0;
      return Math.min(max, Math.max(min, x));
    };
    const next: Config = {
      ...c,
      processing_dir: val("processing_dir"),
      outbox_dir: val("outbox_dir"),
      quarantine_dir: val("quarantine_dir"),
      slm_primary_gguf: val("slm_primary_gguf"),
      slm_escalation_gguf: val("slm_escalation_gguf"),
      ettin_model_dir: val("ettin_model_dir"),
      convert_workers: num("convert_workers"),
      slm_parallel: num("slm_parallel"),
      evidence_token_budget: num("evidence_token_budget"),
      manifest_emit_per_min: num("manifest_emit_per_min"),
      max_stage_attempts: num("max_stage_attempts"),
      per_file_wall_clock_secs: num("per_file_wall_clock_secs"),
    };
    const err = root2.querySelector(".err") as HTMLElement;
    try {
      await invoke("set_config", { cfg: next });
      cfg = next;
      // The backend drops its cached preflight result on every settings
      // save (paths may have just changed underneath it); pick up that
      // fail-closed "unchecked" state and swap the panel in without a full
      // re-render, so the "Saved." message stays visible.
      await refreshRuntime(false);
      const oldPanel = root.querySelector(".preflight-panel");
      if (oldPanel) oldPanel.replaceWith(renderReadinessPanel());
      err.textContent = "Saved. Run preflight to verify this machine before starting.";
      setTimeout(() => (err.textContent = ""), 4000);
    } catch (e) {
      err.textContent = String(e);
    }
  });
  root.appendChild(root2);
}

let renderQueued = false;
listen("job-updated", () => {
  // Coalesce bursts; a 4-worker pipeline can emit dozens of events a second.
  if (renderQueued) return;
  renderQueued = true;
  setTimeout(() => { renderQueued = false; if (view !== "settings") render(); }, 400);
});

(async () => {
  try {
    cfg = await invoke<Config>("get_config");
  } catch (e) {
    // Without config we can render nothing meaningful; show a recoverable
    // fatal state instead of a blank white window.
    app.innerHTML = "";
    const fatal = el(
      `<div class="fatal"><strong>BackLog failed to start.</strong><div class="msg"></div><button type="button">Reload</button></div>`
    );
    (fatal.querySelector(".msg") as HTMLElement).textContent = String(e);
    fatal.querySelector("button")!.addEventListener("click", () => location.reload());
    app.appendChild(fatal);
    return;
  }
  // Cheap, cached read (never spawns the sidecar or touches disk) so
  // startup stays fast; the fail-closed backend default keeps Start
  // disabled until an explicit "Run preflight" passes.
  await refreshRuntime(false);
  if (!cfg.processing_dir || !runtime.configured) view = "settings";
  render();
})();
