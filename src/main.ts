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
  sidecar_timeout_secs: number;
  manifest_emit_per_min: number;
  max_head_pages: number;
  max_tail_pages: number;
  max_filename_len: number;
  max_stage_attempts: number;
  per_file_wall_clock_secs: number;
};

type RuntimeProblem = {
  field: string;
  code: string;
  message: string;
  severity: "error" | "warning";
};

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

type View = "queue" | "flagged" | "settings";
type PathKey =
  | "processing_dir"
  | "outbox_dir"
  | "quarantine_dir"
  | "cache_dir"
  | "slm_primary_gguf"
  | "slm_escalation_gguf"
  | "ettin_model_dir";

type Notice = { kind: "error" | "success"; text: string };

const appRoot = document.getElementById("app");
if (!(appRoot instanceof HTMLElement)) throw new Error("BackLog application root is missing");
const app: HTMLElement = appRoot;

let cfg: Config | null = null;
let runtime: RuntimeStatus = uncheckedRuntime();
let view: View = "queue";
let notice: Notice | null = null;
let renderVersion = 0;

const STATE_BADGE: Record<string, string> = {
  ingested: "b-wait",
  converted: "b-wait",
  filtered: "b-wait",
  named: "b-wait",
  validated: "b-wait",
  emitted: "b-ok",
  flagged: "b-flag",
};

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

function el(html: string): HTMLElement {
  const template = document.createElement("template");
  template.innerHTML = html.trim();
  const element = template.content.firstElementChild;
  if (!(element instanceof HTMLElement)) throw new Error("Expected one HTML element");
  return element;
}

function esc(value: string | null | undefined): string {
  return (value ?? "").replace(/[&<>"']/g, (character) =>
    ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    })[character] ?? character,
  );
}

function errorText(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return "Unknown error";
  }
}

function runtimeLabel(): string {
  if (runtime.running && runtime.paused) return "Paused";
  if (runtime.running) return "Running";
  if (runtime.configured) return "Ready";
  if (runtime.checked) return "Setup blocked";
  return "Setup unchecked";
}

function runtimeClass(): string {
  if (runtime.running && runtime.paused) return "runtime-paused";
  if (runtime.running || runtime.configured) return "runtime-ready";
  return "runtime-blocked";
}

async function refreshRuntime(activeCheck: boolean): Promise<boolean> {
  try {
    runtime = await invoke<RuntimeStatus>(
      activeCheck ? "run_preflight" : "get_runtime_status",
    );
    return true;
  } catch (error) {
    notice = { kind: "error", text: errorText(error) };
    try {
      runtime = await invoke<RuntimeStatus>("get_runtime_status");
    } catch {
      // Keep the most recent status if the backend is temporarily unavailable.
    }
    return false;
  }
}

async function render(): Promise<void> {
  const version = ++renderVersion;
  const stats: Record<string, number> = await invoke<Record<string, number>>("get_stats").catch(
    (): Record<string, number> => ({}),
  );
  if (version !== renderVersion) return;

  const total = Object.values(stats).reduce((sum, value) => sum + value, 0);
  const runLabel = runtime.running ? (runtime.paused ? "Resume" : "Pause") : "Start";
  const runDisabled = !runtime.running && !runtime.configured;

  app.innerHTML = "";
  app.appendChild(
    el(`
      <div class="shell">
        <header>
          <div class="brand">Back<span>Log</span></div>
          <nav aria-label="Primary navigation">
            <button type="button" data-v="queue" class="${view === "queue" ? "on" : ""}">Queue</button>
            <button type="button" data-v="flagged" class="${view === "flagged" ? "on" : ""}">
              Needs Review
              ${stats.flagged ? `<span class="pill">${stats.flagged}</span>` : ""}
            </button>
            <button type="button" data-v="settings" class="${view === "settings" ? "on" : ""}">Settings</button>
          </nav>
          <div class="run">
            <span class="runtime-chip ${runtimeClass()}">${runtimeLabel()}</span>
            <span class="stats">${total} files · ${stats.emitted ?? 0} done · ${stats.flagged ?? 0} flagged</span>
            <button
              type="button"
              id="runbtn"
              class="${runtime.running ? (runtime.paused ? "paused" : "live") : "start"}"
              ${runDisabled ? 'disabled aria-disabled="true" title="Complete Settings preflight before starting"' : ""}
            >${runLabel}</button>
          </div>
        </header>
        ${
          notice
            ? `<div class="notice notice-${notice.kind}" role="status">${esc(notice.text)}</div>`
            : ""
        }
        <main id="content"></main>
      </div>
    `),
  );

  app.querySelectorAll<HTMLButtonElement>("nav button[data-v]").forEach((button) => {
    button.addEventListener("click", () => {
      const next = button.dataset.v;
      if (next === "queue" || next === "flagged" || next === "settings") {
        view = next;
        notice = null;
        void render();
      }
    });
  });

  const runButton = document.getElementById("runbtn");
  if (runButton instanceof HTMLButtonElement && !runButton.disabled) {
    runButton.addEventListener("click", () => void onRunButton(runButton));
  }

  const content = document.getElementById("content");
  if (!content) return;
  if (view === "queue") await renderQueue(content);
  else if (view === "flagged") await renderFlagged(content);
  else renderSettings(content);
}

async function onRunButton(button: HTMLButtonElement): Promise<void> {
  button.disabled = true;
  notice = null;
  try {
    if (!runtime.running) {
      await invoke("start_pipeline");
    } else {
      await invoke("set_paused", { paused: !runtime.paused });
    }
    await refreshRuntime(false);
  } catch (error) {
    notice = { kind: "error", text: errorText(error) };
    view = "settings";
    await refreshRuntime(false);
  }
  await render();
}

async function renderQueue(root: HTMLElement): Promise<void> {
  const jobs = await invoke<Job[]>("list_jobs", { limit: 500 }).catch(() => []);
  if (!jobs.length) {
    root.appendChild(
      el(`
        <div class="empty">
          No files yet. Complete Settings, start BackLog, then drop files into the Processing folder or let Flow 1 deliver them.
        </div>
      `),
    );
    return;
  }

  const rows = jobs
    .map(
      (job) => `
        <tr>
          <td class="mono" title="${esc(job.sha256)}">${esc(job.original_name)}</td>
          <td>${esc(job.final_filename)}</td>
          <td>${esc(job.doc_type)}</td>
          <td>
            <span class="badge ${STATE_BADGE[job.state] ?? "b-wait"}">${esc(job.state)}</span>
            ${job.soft_flags ? `<span class="soft" title="${esc(job.soft_flags)}">!</span>` : ""}
          </td>
        </tr>
      `,
    )
    .join("");

  root.appendChild(
    el(`
      <div class="table-wrap">
        <table>
          <thead><tr><th>Original</th><th>New name</th><th>Type</th><th>State</th></tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </div>
    `),
  );
}

async function renderFlagged(root: HTMLElement): Promise<void> {
  const jobs = await invoke<Job[]>("list_flagged").catch(() => []);
  if (!jobs.length) {
    root.appendChild(el(`<div class="empty">Nothing needs review.</div>`));
    return;
  }

  jobs.forEach((job, index) => {
    const id = `review-${index}-${job.sha256.slice(0, 8)}`;
    const card = el(`
      <section class="card" aria-labelledby="${id}-title">
        <div class="card-head">
          <strong id="${id}-title">${esc(job.original_name)}</strong>
          <code class="reason">${esc(job.flag_reason ?? "unknown")}</code>
        </div>
        <div class="fields">
          <label for="${id}-date">Date</label>
          <input id="${id}-date" type="date" name="date" value="${esc(job.proposed_date)}" required>
          <label for="${id}-subject">Subject</label>
          <input id="${id}-subject" name="subject" placeholder="3 to 8 words" value="${esc(job.proposed_subject)}" required>
          <label for="${id}-description">Description</label>
          <input id="${id}-description" name="description" placeholder="One sentence, 15 to 200 characters." value="${esc(job.description)}" required>
        </div>
        <div class="card-actions">
          <button type="button" class="ghost" data-act="evidence">View text</button>
          <button type="button" data-act="resubmit">Approve and re-emit</button>
          <span class="err" role="alert" aria-live="polite"></span>
        </div>
        <pre class="evidence" hidden></pre>
      </section>
    `);

    const evidenceButton = card.querySelector<HTMLButtonElement>('[data-act="evidence"]');
    const evidence = card.querySelector<HTMLElement>(".evidence");
    evidenceButton?.addEventListener("click", async () => {
      if (!evidence) return;
      if (evidence.hidden && !evidence.dataset.loaded) {
        evidenceButton.disabled = true;
        evidenceButton.textContent = "Loading text";
        evidence.textContent = await invoke<string>("get_evidence", { sha256: job.sha256 }).catch(
          () => "(No cached text. The file failed before conversion.)",
        );
        evidence.dataset.loaded = "true";
        evidenceButton.disabled = false;
      }
      evidence.hidden = !evidence.hidden;
      evidenceButton.textContent = evidence.hidden ? "View text" : "Hide text";
    });

    const submitButton = card.querySelector<HTMLButtonElement>('[data-act="resubmit"]');
    submitButton?.addEventListener("click", async () => {
      const value = (name: string): string => {
        const input = card.querySelector<HTMLInputElement>(`[name="${name}"]`);
        return input?.value.trim() ?? "";
      };
      const date = value("date");
      const subject = value("subject");
      const description = value("description");
      const errors = validateReview(date, subject, description);
      const errorElement = card.querySelector<HTMLElement>(".err");
      if (errorElement) errorElement.textContent = errors.join(" ");
      if (errors.length) return;

      submitButton.disabled = true;
      submitButton.textContent = "Validating";
      card.setAttribute("aria-busy", "true");
      try {
        await invoke("resubmit", {
          sha256: job.sha256,
          date,
          subject,
          description,
        });
        notice = { kind: "success", text: `${job.original_name} was approved and re-emitted.` };
        card.remove();
      } catch (error) {
        if (errorElement) errorElement.textContent = errorText(error);
        submitButton.disabled = false;
        submitButton.textContent = "Approve and re-emit";
        card.removeAttribute("aria-busy");
      }
    });

    root.appendChild(card);
  });
}

function validateReview(date: string, subject: string, description: string): string[] {
  const errors: string[] = [];
  if (!isIsoDate(date)) errors.push("Enter a valid date in YYYY-MM-DD format.");

  const words = subject.split(/\s+/).filter(Boolean);
  if (words.length < 3 || words.length > 8) {
    errors.push("Subject must contain 3 to 8 words.");
  }

  const descriptionLength = Array.from(description).length;
  if (descriptionLength < 15 || descriptionLength > 200) {
    errors.push("Description must contain 15 to 200 characters.");
  }
  if (!/[.!?]$/.test(description)) {
    errors.push("Description must end with sentence punctuation.");
  }
  if (description.toLocaleLowerCase().replace(/[.!?]$/, "") === subject.toLocaleLowerCase()) {
    errors.push("Description must add information beyond the subject.");
  }
  return errors;
}

function isIsoDate(value: string): boolean {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (!match) return false;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const date = new Date(Date.UTC(year, month - 1, day));
  return (
    date.getUTCFullYear() === year &&
    date.getUTCMonth() === month - 1 &&
    date.getUTCDate() === day
  );
}

function renderSettings(root: HTMLElement): void {
  if (!cfg) return;
  const current = cfg;
  const locked = runtime.running;
  const disabled = locked ? "disabled" : "";

  const pathField = (label: string, key: PathKey, kind: "file" | "directory") => `
    <label class="wide" for="setting-${key}">${label}</label>
    <div class="pick wide">
      <input id="setting-${key}" name="${key}" value="${esc(String(current[key] ?? ""))}" ${disabled}>
      <button type="button" class="ghost" data-pick="${key}" data-kind="${kind}" ${disabled}>Browse</button>
    </div>
  `;

  const checks: Array<[string, boolean, string]> = [
    ["Processing folder is readable", runtime.processing_dir_ready, "processing_dir"],
    ["Manifest outbox is writable", runtime.outbox_writable, "outbox_dir"],
    ["Local quarantine is writable", runtime.quarantine_writable, "quarantine_dir"],
    ["Local cache is writable", runtime.cache_writable, "cache_dir"],
    ["Conversion sidecar is installed", runtime.sidecar_found, "sidecar"],
    ["Conversion sidecar answers ping", runtime.sidecar_ok, "sidecar"],
    ["llama-server is installed", runtime.llama_server_found, "llama_server"],
    ["Naming grammar is installed", runtime.grammar_found, "grammar"],
    ["Primary Qwen3 0.6B model is present", runtime.primary_model_found, "slm_primary_gguf"],
    ["Escalation Qwen3 1.7B model is present", runtime.escalation_model_found, "slm_escalation_gguf"],
    ["Runtime inference stays on this device", runtime.offline_runtime, "offline_runtime"],
  ];

  const checkRows = checks
    .map(
      ([label, passed, field]) => `
        <li class="check-row ${passed ? "check-pass" : "check-fail"}" data-field="${field}">
          <span>${esc(label)}</span>
          <strong>${passed ? "Ready" : "Blocked"}</strong>
        </li>
      `,
    )
    .join("");

  const problemRows = runtime.problems
    .map(
      (problem) => `
        <li>
          <strong>${esc(problem.field)}</strong>
          <span>${esc(problem.message)}</span>
        </li>
      `,
    )
    .join("");

  const section = el(`
    <div class="settings-layout">
      <section class="preflight-panel" aria-labelledby="preflight-title">
        <div class="section-head">
          <div>
            <h2 id="preflight-title">Readiness</h2>
            <p>${
              runtime.checked_at
                ? `Last checked ${esc(new Date(runtime.checked_at).toLocaleString())}`
                : "Not checked since settings changed"
            }</p>
          </div>
          <button type="button" id="preflight-button" class="ghost">Run preflight</button>
        </div>
        <ul class="check-list">${checkRows}</ul>
        ${
          problemRows
            ? `<div class="problem-box" role="status"><strong>Action needed</strong><ul>${problemRows}</ul></div>`
            : `<div class="ready-box" role="status">All hard prerequisites passed. BackLog is ready to start.</div>`
        }
        <p class="privacy-note">
          Document conversion and model inference run locally. Power Automate receives only the final manifest used for SharePoint file operations and indexing.
        </p>
      </section>

      <form class="settings-form">
        <fieldset ${locked ? "disabled" : ""}>
          <legend>Folders</legend>
          ${pathField("Processing folder, OneDrive-synced Flow 1 target", "processing_dir", "directory")}
          ${pathField("Outbox folder, OneDrive-synced manifest root", "outbox_dir", "directory")}
          ${pathField("Quarantine folder, local only", "quarantine_dir", "directory")}
          ${pathField("Cache folder, local only", "cache_dir", "directory")}
        </fieldset>

        <fieldset ${locked ? "disabled" : ""}>
          <legend>Models</legend>
          ${pathField("Primary GGUF, Qwen3 0.6B Q8_0", "slm_primary_gguf", "file")}
          ${pathField("Escalation GGUF, Qwen3 1.7B Q8_0", "slm_escalation_gguf", "file")}
          ${pathField("Ettin model directory, optional", "ettin_model_dir", "directory")}
        </fieldset>

        <fieldset ${locked ? "disabled" : ""}>
          <legend>Tuning</legend>
          <div class="grid3 wide">
            <label for="setting-convert_workers">Convert workers</label>
            <label for="setting-slm_parallel">SLM parallel</label>
            <label for="setting-evidence_token_budget">Evidence tokens</label>
            <input id="setting-convert_workers" name="convert_workers" type="number" min="1" max="64" value="${current.convert_workers}">
            <input id="setting-slm_parallel" name="slm_parallel" type="number" min="1" max="32" value="${current.slm_parallel}">
            <input id="setting-evidence_token_budget" name="evidence_token_budget" type="number" min="1" value="${current.evidence_token_budget}">

            <label for="setting-manifest_emit_per_min">Manifests per minute</label>
            <label for="setting-max_stage_attempts">Attempts per stage</label>
            <label for="setting-sidecar_timeout_secs">Sidecar timeout, seconds</label>
            <input id="setting-manifest_emit_per_min" name="manifest_emit_per_min" type="number" min="0" value="${current.manifest_emit_per_min}">
            <input id="setting-max_stage_attempts" name="max_stage_attempts" type="number" min="1" max="10" value="${current.max_stage_attempts}">
            <input id="setting-sidecar_timeout_secs" name="sidecar_timeout_secs" type="number" min="1" value="${current.sidecar_timeout_secs}">

            <label for="setting-per_file_wall_clock_secs">Wall clock per file, seconds</label>
            <label for="setting-llama_port">Local llama port</label>
            <span aria-hidden="true"></span>
            <input id="setting-per_file_wall_clock_secs" name="per_file_wall_clock_secs" type="number" min="5" max="3600" value="${current.per_file_wall_clock_secs}">
            <input id="setting-llama_port" name="llama_port" type="number" min="1024" max="65534" value="${current.llama_port}">
            <span aria-hidden="true"></span>
          </div>
        </fieldset>

        <div class="card-actions">
          <button type="submit" ${disabled}>Save and check</button>
          <span class="err" role="alert" aria-live="polite">${
            locked ? "Restart BackLog before changing runtime settings." : ""
          }</span>
        </div>
      </form>
    </div>
  `);

  const preflightButton = section.querySelector<HTMLButtonElement>("#preflight-button");
  preflightButton?.addEventListener("click", async () => {
    preflightButton.disabled = true;
    preflightButton.textContent = "Checking";
    notice = null;
    await refreshRuntime(true);
    await render();
  });

  section.querySelectorAll<HTMLButtonElement>("[data-pick]").forEach((button) => {
    button.addEventListener("click", async () => {
      const key = button.dataset.pick as PathKey | undefined;
      const kind = button.dataset.kind;
      if (!key || (kind !== "file" && kind !== "directory")) return;
      const selection = await open({
        directory: kind === "directory",
        multiple: false,
        filters: kind === "file" ? [{ name: "GGUF model", extensions: ["gguf"] }] : undefined,
      });
      if (typeof selection === "string") {
        const input = section.querySelector<HTMLInputElement>(`[name="${key}"]`);
        if (input) input.value = selection;
      }
    });
  });

  const form = section.querySelector<HTMLFormElement>(".settings-form");
  form?.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (runtime.running) return;

    const textValue = (name: string): string =>
      form.querySelector<HTMLInputElement>(`[name="${name}"]`)?.value.trim() ?? "";
    const numberValue = (name: string): number => Number.parseInt(textValue(name), 10) || 0;
    const next: Config = {
      ...current,
      processing_dir: textValue("processing_dir"),
      outbox_dir: textValue("outbox_dir"),
      quarantine_dir: textValue("quarantine_dir"),
      cache_dir: textValue("cache_dir"),
      slm_primary_gguf: textValue("slm_primary_gguf"),
      slm_escalation_gguf: textValue("slm_escalation_gguf"),
      ettin_model_dir: textValue("ettin_model_dir"),
      convert_workers: numberValue("convert_workers"),
      slm_parallel: numberValue("slm_parallel"),
      evidence_token_budget: numberValue("evidence_token_budget"),
      manifest_emit_per_min: numberValue("manifest_emit_per_min"),
      max_stage_attempts: numberValue("max_stage_attempts"),
      sidecar_timeout_secs: numberValue("sidecar_timeout_secs"),
      per_file_wall_clock_secs: numberValue("per_file_wall_clock_secs"),
      llama_port: numberValue("llama_port"),
    };

    const errorElement = form.querySelector<HTMLElement>(".err");
    const submitButton = form.querySelector<HTMLButtonElement>('button[type="submit"]');
    if (errorElement) errorElement.textContent = "";
    if (submitButton) {
      submitButton.disabled = true;
      submitButton.textContent = "Saving";
    }

    try {
      await invoke("set_config", { cfg: next });
      cfg = next;
      await refreshRuntime(true);
      notice = runtime.configured
        ? { kind: "success", text: "Settings saved and every preflight check passed." }
        : { kind: "error", text: "Settings saved, but setup still has blocked checks." };
      await render();
    } catch (error) {
      if (errorElement) errorElement.textContent = errorText(error);
      if (submitButton) {
        submitButton.disabled = false;
        submitButton.textContent = "Save and check";
      }
    }
  });

  root.appendChild(section);
}

let renderQueued = false;
void listen("job-updated", () => {
  if (renderQueued || view === "settings") return;
  renderQueued = true;
  window.setTimeout(() => {
    renderQueued = false;
    void render();
  }, 400);
});

void (async () => {
  try {
    cfg = await invoke<Config>("get_config");
    const hasCoreSetup = Boolean(
      cfg.processing_dir &&
        cfg.outbox_dir &&
        cfg.quarantine_dir &&
        cfg.cache_dir &&
        cfg.slm_primary_gguf &&
        cfg.slm_escalation_gguf,
    );
    await refreshRuntime(hasCoreSetup);
    if (!runtime.running && !runtime.configured) view = "settings";
  } catch (error) {
    notice = { kind: "error", text: `BackLog could not load its configuration: ${errorText(error)}` };
    view = "settings";
  }
  await render();
})();
