//! Local structured-output naming lane backed by llama.cpp.
//!
//! The primary Qwen3-0.6B server starts on demand. The Qwen3-1.7B escalation
//! server starts only after a rejected primary attempt and remains resident
//! until `escalation_idle_secs` of no completed request, or for the batch
//! when that is disabled. Both bind to loopback and use the model's embedded
//! chat template through llama.cpp's OpenAI-compatible chat-completions
//! endpoint.
//!
//! A Qwen3-1.7B primary with a Qwen3-4B-Q4_K_M escalation was built and
//! measured against this pair and **rejected**, so that neither the idea nor
//! the reason for dropping it has to be reconstructed. On the same corpus,
//! same code, same context and evidence budget, `slm_parallel: 1`,
//! `convert_workers: 3`, at matched evidence coverage: **40.11 s/file against
//! 20.03**. Twice the wall clock, and the only quality signal that favoured it
//! was two documents of subject faithfulness at n=26 — inside the run-to-run
//! variance `docs/SIZING.md` warns about. It was not a memory problem; the
//! 1.7B primary fits the 16 GB class comfortably. It simply did not earn its
//! seconds.
//!
//! What did earn them was upstream of the model: `filter.rs`'s
//! `semantic_top_k` had been a hardcoded 12 paragraphs unrelated to
//! `evidence_token_budget`, and deriving it made this same 0.6B pair **19%
//! faster** (24.69 -> 20.03 s/file) by putting better evidence in front of it
//! and provoking fewer rejected proposals. When naming quality is the
//! question, look at what reaches the model before reaching for a bigger one.
//!
//! At or below 9 GiB `Config::normalize` leaves the escalation GGUF equal to
//! the primary and rung 3 runs on the server that is already up — see
//! `escalation_collapsed`.
//!
//! Qwen3 and its GGUF conversions are Apache-2.0; this replaces the prior
//! Liquid-licensed LFM2.5 lane so the app can be redistributed without a
//! non-standard model license.

use crate::checker::SlmOutput;
use crate::sidecar::log_child_stderr;
use serde_json::{json, Value};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// `--ctx-size` granted per llama-server slot; the server is asked for
/// `SLM_CTX_PER_SLOT * parallel` (see `server_args`).
///
/// Derived, not chosen. Rung 3 asks the filter for 4000 evidence "tokens" in
/// the codebase's optimistic chars/4 unit (`Config::escalation_evidence_token_budget`),
/// which is 16000 characters of real document text. Qwen3's tokenizer does not
/// achieve chars/4 on what an evidence bundle is made of — ISO dates, party
/// names, case captions, currency, table fragments and this module's own label
/// prefixes — so the slot is sized at a pessimistic 3 real chars/token: 5334
/// tokens. Add [`SLM_PROMPT_RESERVE_TOKENS`] (640) and [`SLM_MAX_OUTPUT_TOKENS`]
/// (220) and one slot must hold 6194 tokens; rounded up to the next multiple of
/// 256, because llama.cpp allocates KV in blocks and a ragged number buys
/// nothing, that is 6656.
///
/// The cost is paid in resident memory on every install: at 6656 the 28-layer
/// KV cache the shipped 0.6B and 1.7B share is 728 MiB per slot, against
/// 448 MiB at the old 4096 — which is why `slm_parallel` tops out at 2 rather
/// than the 4 it allowed when slots were cheaper. Do not raise this to "leave
/// headroom" without redoing the RAM tiering in `config.rs` — nothing here
/// fails loudly when the sum stops fitting; the machine just swaps.
///
/// Overflow in the other direction is not slow, it is wrong: llama.cpp does not
/// refuse an over-length prompt, it drops what does not fit, and the model then
/// names the document from evidence with a hole in it. `filter::max_bundle_chars`
/// derives an enforced ceiling from this constant for exactly that reason.
pub const SLM_CTX_PER_SLOT: u32 = 6656;

/// Tokens reserved inside each slot for everything that is not evidence: the
/// system prompt, the operator's `custom_naming_notes` section, the retry's
/// violation note and Qwen3's chat-template wrapper.
///
/// `build_system_prompt`'s core rules render to ~1.6 kB, which is ~530 tokens
/// at the same pessimistic 3 chars/token this lane sizes everything against and
/// closer to 400 at a realistic rate; the chat template and the retry's
/// violation note are tens of tokens each. 640 therefore covers what the app
/// ships with, and the operator's notes are the variable part —
/// `Config::validate` caps them at 600 characters, which at 3 chars/token is
/// another 200. Worst case (a maxed-out notes block, everything tokenizing at
/// the pessimistic rate) overruns this reserve by roughly 135 tokens, and that
/// is deliberately survivable: rounding [`SLM_CTX_PER_SLOT`] up to a multiple
/// of 256 left ~460 tokens of slack in the slot, which is where the overrun is
/// spent. This number is the sizing assumption, not a limit anything enforces —
/// nothing measures the rendered prompt at runtime.
pub const SLM_PROMPT_RESERVE_TOKENS: u32 = 640;

/// `max_tokens` for one naming response (see `request_body`). The answer is a
/// small JSON object — date, subject, description — so 220 is generous for it;
/// the value is unchanged and only named here because the slot ceiling has to
/// reserve it, and a literal buried inside a `json!` body cannot be derived
/// from.
pub const SLM_MAX_OUTPUT_TOKENS: u32 = 220;

/// How long a freshly spawned llama-server gets to answer `/health` before
/// the child is killed and the slot cleared. Being wedged forever is not
/// acceptable; neither is killing a server that is merely still loading.
///
/// 60s was sized when the largest GGUF this lane loaded was 1.8 GB into a
/// 4096-token slot. The escalation tier is now a 2.33 GB Q4_K_M read off a
/// possibly-cold disk, followed by a ~936 MiB KV allocation at
/// [`SLM_CTX_PER_SLOT`] — both of which happen before
/// `/health` answers at all. Losing that race is not a slow first document, it
/// is a permanent one: the deadline kills the child and clears the slot, so the
/// next attempt starts the same multi-gigabyte load from scratch and fails the
/// same way forever. 180s is a ceiling, not a wait — the loop polls every
/// `HEALTH_POLL_INTERVAL` and a warm start still returns in under a second, so
/// raising this costs a healthy machine nothing and only widens what a
/// genuinely slow first load is allowed to take.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(180);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long one `/v1/chat/completions` call may take before the client gives up.
///
/// Must stay at or above `Config::per_file_wall_clock_secs` (default 180),
/// which is what `pipeline.rs`'s `wall_clock_cap` budgets for a single naming
/// rung. The pair was once inverted — a 60s client against a 90s budget — so
/// the two disagreed about who gets to end a slow request and the tighter one
/// silently won. That never showed up on a workstation, where naming takes
/// seconds; it matters on the no-GPU laptops this ships to, where the whole
/// point of the wall-clock budget is to tolerate a slow-but-succeeding
/// document. Losing the race turns such a file into
/// `SLM_FAIL:no valid output after escalation` — a message that blames the
/// model for a deadline the HTTP client imposed.
///
/// 300s keeps the ordering correct with room for the operator to raise the
/// knob: the configured budget, not this constant, is what ends a slow naming
/// request, so the number in `backlog.config.json` means what it says and the
/// timeout that fires is the one whose expiry the pipeline can attribute. Past
/// 300 the ordering inverts again and this constant has to move with it.
const NAMING_HTTP_TIMEOUT: Duration = Duration::from_secs(300);

/// How far above the configured base port `reserve_port` will look for a free
/// one before giving up.
const PORT_SCAN_RANGE: u16 = 20;

/// llama-server logs its whole startup banner before serving; that plus any
/// failure is what is worth keeping out of a backfill's request narration.
const MAX_LOGGED_SERVER_LINES: usize = 400;

/// One running llama-server and the port it was given. Killing on drop is
/// what makes "clear the slot" a real recovery rather than an orphan factory.
struct Server {
    child: Child,
    port: u16,
    _stderr: std::thread::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Which physical slot a request actually landed on. Needed because collapse
/// (`escalation_gguf == primary_gguf`, the 8 GB single-model install) routes
/// `Tier::Escalation` calls onto `self.primary` — recycling and idle-reap
/// must key off the physical slot, not the nominal `Tier`, or a collapsed
/// install would never recycle and a non-collapsed one would double-count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedSlot {
    Primary,
    Escalation,
}

impl ResolvedSlot {
    /// What a log line calls this slot. The physical slot, deliberately, not
    /// the nominal `Tier` — a collapsed install routes `Tier::Escalation` onto
    /// the primary child, and a recycle line that named the tier would blame
    /// the wrong server.
    fn name(self) -> &'static str {
        match self {
            ResolvedSlot::Primary => "primary",
            ResolvedSlot::Escalation => "escalation",
        }
    }
}

/// Per-tier server tuning. Grouped so adding a knob touches one call site's
/// field list, not every positional `SlmLane::new` call's argument order.
pub struct SlmTuning {
    pub escalation_parallel: u8,
    /// 0 disables request-count recycling of the primary server.
    pub recycle_after_requests: u32,
    /// 0 disables escalation idle-reaping (resident for process lifetime,
    /// today's behavior).
    pub escalation_idle_secs: u64,
}

impl Default for SlmTuning {
    fn default() -> Self {
        Self {
            escalation_parallel: 1,
            recycle_after_requests: 0,
            escalation_idle_secs: 0,
        }
    }
}

/// How often the idle reaper polls. Independent of the idle window itself so
/// the check granularity stays coarse regardless of what an operator
/// configures for `slm_escalation_idle_secs`.
pub(crate) const ESCALATION_REAP_POLL_INTERVAL: Duration = Duration::from_secs(30);

/// Reserves one in-flight dispatch against a resolved slot for the lifetime
/// of one naming attempt (one `name_document` call, including the
/// span-mismatch re-prompt, which reuses the same tier/slot). Constructed
/// only once a request has fully committed to a specific, healthy child —
/// see `ensure_up`. Drop is the only place that decrements, so it fires
/// symmetrically on success, on any `?`-propagated error inside
/// `name_document`, and on task cancellation (wall-clock-cap drop).
struct ServeGuard<'a> {
    lane: &'a SlmLane,
    slot: ResolvedSlot,
}

impl Drop for ServeGuard<'_> {
    fn drop(&mut self) {
        match self.slot {
            ResolvedSlot::Primary => {
                self.lane.primary_inflight.fetch_sub(1, Ordering::SeqCst);
            }
            ResolvedSlot::Escalation => {
                self.lane.escalation_inflight.fetch_sub(1, Ordering::SeqCst);
                // "Idle" is measured from last COMPLETION, not last start —
                // this is what makes reaping a saturated, legitimately-busy
                // server structurally impossible: the clock only ever resets
                // when a request actually finishes.
                *self.lane.escalation_last_completion.lock().unwrap() = Some(Instant::now());
            }
        }
    }
}

pub struct SlmLane {
    // Retained as a compatibility input while older installations still bundle
    // the GBNF resource. Current chat requests use JSON Schema directly.
    _fallback_grammar: String,
    llama_server_exe: PathBuf,
    primary_gguf: PathBuf,
    escalation_gguf: PathBuf,
    primary_parallel: u8,
    escalation_parallel: u8,
    /// `--threads` for each spawned server. Never left to llama.cpp's
    /// default (all logical cores): two resident servers plus the convertd
    /// pool oversubscribing every core is the measured order-of-magnitude
    /// batch slowdown. See `Config::slm_threads`.
    threads: usize,
    primary_port: u16,
    escalation_port: u16,
    /// Per-run bearer token handed to llama-server via `--api-key`. Without
    /// it the naming lane is an unauthenticated inference endpoint on
    /// loopback that any local process can post harvested document text to.
    api_key: String,
    /// Once set, no request may create another child. Tauri exits through
    /// `std::process::exit`, so relying on Drop alone can otherwise leave a
    /// request racing the exit path and respawning a multi-GB model server.
    shutting_down: AtomicBool,
    primary: Mutex<Option<Server>>,
    escalation: Mutex<Option<Server>>,
    http: reqwest::Client,
    /// 0 disables request-count recycling. Applies to BOTH slots: llama.cpp's
    /// Windows RSS growth is a property of the server process, not of the tier
    /// it happens to be serving, and the escalation server is now the larger
    /// of the two. See `Config::slm_recycle_after_requests`.
    recycle_after_requests: u32,
    /// 0 disables escalation idle-reaping (resident for process lifetime).
    escalation_idle_secs: u64,
    /// Operator naming preferences (`Config::custom_naming_notes`), set via
    /// `with_naming_notes` and appended to the system prompt as a
    /// subordinate section by `build_system_prompt`. Empty — the default —
    /// leaves the measured core prompt byte-identical. Not a `SlmTuning`
    /// field: that struct is built with exhaustive literals in `pipeline.rs`
    /// e2e tests, which a new field would break.
    naming_notes: String,
    /// Reset to 0 on every successful (re)spawn of the primary server.
    primary_requests_served: AtomicU64,
    /// Same, for a genuinely separate (non-collapsed) escalation child. A
    /// collapsed install never resolves to `ResolvedSlot::Escalation`, so this
    /// stays 0 there and the primary's counter does all the work — which is
    /// correct, because collapsed means there is only one process to grow.
    escalation_requests_served: AtomicU64,
    /// Requests currently dispatched to the live primary child.
    primary_inflight: AtomicU64,
    /// Same, for a genuinely separate (non-collapsed) escalation child.
    escalation_inflight: AtomicU64,
    /// `None` until the first escalation request completes.
    escalation_last_completion: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Primary,
    Escalation,
}

/// Pick a loopback port this process has just proved is free.
///
/// The fixed, shared default port is the whole problem: `ensure_up` used to
/// spawn and then trust whatever answered `/health` on it. That is an orphan
/// llama-server from a crashed previous session — silently binding the batch
/// to a different GGUF than every manifest's `model_versions` records — or
/// any unprivileged local process that bound the port first, which then sees
/// the evidence text of every document and dictates the date and subject the
/// checker goes on to validate. Binding it ourselves first means the
/// responder is the child we spawned unless something wins a microsecond race
/// on a port it could not have predicted.
fn reserve_port(preferred: u16) -> anyhow::Result<u16> {
    for offset in 0..PORT_SCAN_RANGE {
        let Some(candidate) = preferred.checked_add(offset) else {
            break;
        };
        if TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "no free loopback port for llama-server in {preferred}..{}; something else is using them",
        preferred.saturating_add(PORT_SCAN_RANGE)
    )
}

impl SlmLane {
    #[allow(clippy::too_many_arguments)] // same shape as filter.rs/ledger.rs precedents
    pub fn new(
        llama_server_exe: PathBuf,
        grammar: String,
        primary_gguf: PathBuf,
        escalation_gguf: PathBuf,
        base_port: u16,
        primary_parallel: u8,
        threads: usize,
        tuning: SlmTuning,
    ) -> Self {
        let mut token = [0u8; 32];
        getrandom::getrandom(&mut token).expect("CSPRNG for the llama-server API key");
        Self {
            _fallback_grammar: grammar,
            llama_server_exe,
            primary_gguf,
            escalation_gguf,
            primary_parallel,
            escalation_parallel: tuning.escalation_parallel,
            threads,
            primary_port: base_port,
            escalation_port: base_port + 1,
            api_key: hex::encode(token),
            shutting_down: AtomicBool::new(false),
            primary: Mutex::new(None),
            escalation: Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(NAMING_HTTP_TIMEOUT)
                // Localhost only; the app makes zero outbound calls at runtime.
                .no_proxy()
                .build()
                .expect("localhost HTTP client"),
            recycle_after_requests: tuning.recycle_after_requests,
            escalation_idle_secs: tuning.escalation_idle_secs,
            naming_notes: String::new(),
            primary_requests_served: AtomicU64::new(0),
            escalation_requests_served: AtomicU64::new(0),
            primary_inflight: AtomicU64::new(0),
            escalation_inflight: AtomicU64::new(0),
            escalation_last_completion: Mutex::new(None),
        }
    }

    /// Attach the operator's `custom_naming_notes`. Builder-style so the
    /// positional `SlmLane::new` calls in tests stay untouched; the two real
    /// pipelines in `lib.rs` chain this with the value from `Config`.
    pub fn with_naming_notes(mut self, notes: String) -> Self {
        self.naming_notes = notes;
        self
    }

    /// Pure argument construction, split out so ctx-size/parallel-per-tier is
    /// unit-testable without spawning a process (mirrors `rung()`'s "provable
    /// without a live llama-server" split in `pipeline.rs`).
    fn server_args(
        gguf: &Path,
        port: u16,
        parallel: u8,
        threads: usize,
        api_key: &str,
    ) -> Vec<String> {
        vec![
            "--model".to_string(),
            gguf.to_str().unwrap_or_default().to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--port".to_string(),
            port.to_string(),
            "--parallel".to_string(),
            parallel.to_string(),
            "--threads".to_string(),
            threads.to_string(),
            "--ctx-size".to_string(),
            // llama.cpp splits --ctx-size evenly across --parallel slots, so
            // the total has to scale with the slot count or two concurrent
            // documents would each get half a slot's worth of context.
            (SLM_CTX_PER_SLOT * parallel as u32).to_string(),
            // Required so llama-server renders Qwen3's embedded chat
            // template for the /v1/chat/completions endpoint below.
            "--jinja".to_string(),
            "--no-webui".to_string(),
            // Reject requests from anything that is not this process.
            "--api-key".to_string(),
            api_key.to_string(),
        ]
    }

    /// The slot's served-request counter. Split from `inflight` because the
    /// two answer different questions — "how much has this child grown" and
    /// "may it be killed right now" — and the recycle decision needs both.
    fn requests_served(&self, slot: ResolvedSlot) -> &AtomicU64 {
        match slot {
            ResolvedSlot::Primary => &self.primary_requests_served,
            ResolvedSlot::Escalation => &self.escalation_requests_served,
        }
    }

    fn inflight(&self, slot: ResolvedSlot) -> &AtomicU64 {
        match slot {
            ResolvedSlot::Primary => &self.primary_inflight,
            ResolvedSlot::Escalation => &self.escalation_inflight,
        }
    }

    fn parallel_for(&self, slot: ResolvedSlot) -> u8 {
        match slot {
            ResolvedSlot::Primary => self.primary_parallel,
            ResolvedSlot::Escalation => self.escalation_parallel,
        }
    }

    fn spawn_server(&self, gguf: &Path, port: u16, parallel: u8) -> anyhow::Result<Server> {
        anyhow::ensure!(gguf.is_file(), "GGUF model not found: {}", gguf.display());
        let mut command = Command::new(&self.llama_server_exe);
        command
            .args(Self::server_args(
                gguf,
                port,
                parallel,
                self.threads,
                &self.api_key,
            ))
            .stdout(Stdio::null())
            // Piped, not null: llama-server explains a refused port, a bad
            // GGUF or an OOM here and nowhere else. See `log_child_stderr`.
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn()?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no llama-server stderr"))?;
        let stderr_thread = log_child_stderr("llama-server", stderr, MAX_LOGGED_SERVER_LINES)?;
        Ok(Server {
            child,
            port,
            _stderr: stderr_thread,
        })
    }

    /// When both tiers name the same weights, escalation is a second pass
    /// over a wider evidence bundle rather than a bigger model, so it runs on
    /// the server that is already up. Standing a second llama-server on a
    /// second port over the same GGUF would double the resident cost — and
    /// the KV cache, not the weights, is the expensive half — to buy nothing.
    ///
    /// This is the shape an 8 GB machine ships in: the installer carries only
    /// the 0.6B, `Config::normalize` points both tiers at it, and the
    /// escalation rung still happens. Above 9 GiB the 1.7B is fetched by the
    /// in-app downloader and gets a server of its own.
    ///
    /// Collapse is cheaper than it was, and worth knowing before widening it.
    /// At `SLM_CTX_PER_SLOT` 6656 a second 0.6B slot costs 728 MiB of KV
    /// rather than the 448 MiB it cost at 4096, so the memory saved by
    /// collapsing grew with the context — but what collapse costs in quality
    /// did not change: rung 3 still runs, on a wider evidence bundle, against
    /// the server already up.
    pub fn escalation_collapsed(&self) -> bool {
        self.escalation_gguf == self.primary_gguf
    }

    /// Why the child is re-checked and the slot cleared rather than polled to
    /// exhaustion: a llama-server that died on startup (missing CUDA runtime,
    /// corrupt GGUF, port taken) used to cost a full `HEALTH_TIMEOUT` of
    /// polling followed by a message that named the port and nothing else —
    /// and every later call paid that same wait against the same dead child
    /// forever. Now that the timeout is 180s to accommodate a multi-gigabyte
    /// cold load, detecting the corpse rather than waiting it out matters
    /// three times as much as it did.
    async fn ensure_up(&self, tier: Tier) -> anyhow::Result<(u16, ServeGuard<'_>)> {
        anyhow::ensure!(
            !self.shutting_down.load(Ordering::SeqCst),
            "llama-server is shutting down"
        );
        let collapse = tier == Tier::Escalation && self.escalation_collapsed();
        let (slot, gguf, preferred_port, resolved) = match tier {
            Tier::Primary => (
                &self.primary,
                &self.primary_gguf,
                self.primary_port,
                ResolvedSlot::Primary,
            ),
            Tier::Escalation if collapse => (
                &self.primary,
                &self.primary_gguf,
                self.primary_port,
                ResolvedSlot::Primary,
            ),
            Tier::Escalation => (
                &self.escalation,
                &self.escalation_gguf,
                self.escalation_port,
                ResolvedSlot::Escalation,
            ),
        };

        let port = {
            let mut guard = slot.lock().unwrap();
            anyhow::ensure!(
                !self.shutting_down.load(Ordering::SeqCst),
                "llama-server is shutting down"
            );

            // Recycle check runs BEFORE the live/dead check and under the
            // same lock as the respawn decision below — this is the only
            // place two concurrent ensure_up calls for one slot are
            // serialized, and it is what makes "never race an in-flight
            // request" provable: the inflight read here can never be stale
            // relative to another task's reservation, because reservations
            // (below) are also made inside this same critical section.
            //
            // Both slots recycle, not just the primary. Until the 1.7B/4B
            // tiers this was primary-only, on the reasoning that the
            // escalation server is woken rarely and idle-reaped anyway. That
            // no longer holds: the escalation server is now the LARGER of the
            // two, and measurement on this workload put the 4B at a 5,068 MiB
            // working set at rest and 9,258 MiB after sixteen requests. An
            // escalation server that is busy enough never to go idle is
            // exactly the one that grows, and idle-reaping by definition never
            // fires on it.
            if guard.is_some() {
                let served = self.requests_served(resolved).load(Ordering::SeqCst);
                let inflight = self.inflight(resolved).load(Ordering::SeqCst);
                if self.recycle_after_requests > 0
                    && served >= self.recycle_after_requests as u64
                    && inflight == 0
                {
                    log::info!(
                        "recycling {} llama-server after {served} requests",
                        resolved.name()
                    );
                    *guard = None; // Server::drop kills the outgoing child
                }
                // inflight > 0: defer. The next ensure_up call that finds
                // inflight == 0 will recycle instead.
            }

            let live = match guard.as_mut() {
                Some(server) => match server.child.try_wait() {
                    Ok(None) => Some(server.port),
                    _ => None,
                },
                None => None,
            };
            let port = match live {
                Some(port) => port,
                None => {
                    // Drops (and therefore kills) whatever was there before.
                    *guard = None;
                    let port = reserve_port(preferred_port)?;
                    let server = self.spawn_server(gguf, port, self.parallel_for(resolved))?;
                    *guard = Some(server);
                    self.requests_served(resolved).store(0, Ordering::SeqCst);
                    port
                }
            };

            // Reserve the dispatch while still holding the lock (see comment
            // above). This does NOT yet count toward the slot's served count
            // — that only happens once health is confirmed, so a server that
            // never comes up doesn't inflate the count that gates the NEXT
            // recycle decision.
            self.inflight(resolved).fetch_add(1, Ordering::SeqCst);
            port
        };

        // Every exit from here must either return a ServeGuard (which owns
        // the reservation made above) or explicitly release it — a
        // health-check failure that leaked the reservation would freeze
        // `primary_inflight` at "busy" forever and permanently disable
        // recycling.
        let release = |lane: &Self| {
            lane.inflight(resolved).fetch_sub(1, Ordering::SeqCst);
        };

        let health_url = format!("http://127.0.0.1:{port}/health");
        let deadline = Instant::now() + HEALTH_TIMEOUT;
        loop {
            if self.shutting_down.load(Ordering::SeqCst) {
                *slot.lock().unwrap() = None;
                release(self);
                anyhow::bail!("llama-server is shutting down");
            }
            if let Some(exit) = self.child_exit(slot)? {
                *slot.lock().unwrap() = None;
                release(self);
                anyhow::bail!(
                    "llama-server exited during startup ({exit}); its output is in the log file"
                );
            }
            if let Ok(response) = self
                .http
                .get(&health_url)
                .bearer_auth(&self.api_key)
                .send()
                .await
            {
                if response.status().is_success() {
                    self.requests_served(resolved)
                        .fetch_add(1, Ordering::SeqCst);
                    return Ok((
                        port,
                        ServeGuard {
                            lane: self,
                            slot: resolved,
                        },
                    ));
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        }
        // Clear the slot so the next call gets a clean spawn instead of
        // re-paying the full wait against the same wedged child.
        *slot.lock().unwrap() = None;
        release(self);
        anyhow::bail!(
            "llama-server on port {port} did not become healthy within {}s; it has been stopped",
            HEALTH_TIMEOUT.as_secs()
        )
    }

    /// `Some(status)` once the spawned child has exited, `None` while it is
    /// still running. Split out so the health loop never holds the slot lock
    /// across an `await`.
    fn child_exit(&self, slot: &Mutex<Option<Server>>) -> anyhow::Result<Option<String>> {
        let mut guard = slot.lock().unwrap();
        let Some(server) = guard.as_mut() else {
            anyhow::bail!("the llama-server slot was cleared while it was starting");
        };
        Ok(match server.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("could not be waited on: {e}")),
        })
    }

    fn response_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["date", "date_source", "subject", "description"],
            "properties": {
                "date": {
                    "type": "string",
                    "pattern": "^(?:none|[12][0-9]{3}-[0-9]{2}-[0-9]{2})$"
                },
                "date_source": {
                    "type": "string",
                    "enum": ["document", "metadata", "none"]
                },
                // Headroom above what `checker.rs` accepts, on purpose.
                //
                // llama.cpp's grammar enforces `maxLength` by refusing to emit
                // another character, so a cap set *at* the checker's limit stops
                // the model mid-word and hands the checker a fragment. Measured
                // with subject capped at 80 and description at 200: every
                // proposal came back at exactly the cap — subjects ending
                // `"Cobalt Ridge Analyt,"` and `"Taxpayer / "`, descriptions
                // ending `"The return was "`. The second sentence-fragment then
                // failed the very "exactly one sentence" rule the model had been
                // told to satisfy, so the document was quarantined for obeying
                // its own schema.
                //
                // With room to finish, the model produces a whole last word and a
                // whole last sentence, and `checker.rs` trims to its own limits at
                // a safe boundary — a word break for the subject, a sentence end
                // for the description — which it can only do if nothing was
                // already lost mid-token.
                // `subject` gets the same headroom, and 0.4.2 shipped without it
                // by reasoning that a character cap could stand in for a word
                // count. It cannot, and the paragraph above says why: a cap set
                // where the answer wants to end cuts mid-word.
                //
                // 0.4.1 lowered this to 64 to stop `SUBJECT_TRUNCATED` firing on
                // 39 of 40 documents, on the grounds that "64 characters is about
                // ten words of ordinary English". Measured on the 0.4.2 run, that
                // trade was the wrong way round: **18 of 40 subjects came back at
                // exactly 64 characters**, cut mid-word — `"... for Yolanda Bea"`
                // (Beaumont), `"... - Internal 11"`, and
                // `"Tax Return - Supplemental Income and Loss (Rental Real
                // Estate) -"`, where the party was next and never arrived. None
                // of them carried a flag, because the word count was still under
                // ten so the checker's trimmer never engaged. A silent mid-word
                // cut on 45% of documents is strictly worse than a flagged trim
                // at a word boundary, which is all `SUBJECT_TRUNCATED` ever was.
                //
                // 95 is not a guess: it is the whole filename budget. `compose`
                // builds `"YYYY-MM-DD " + subject` and needs
                // `FILENAME_TAIL_RESERVE` on top, so with `max_filename_len` at
                // 120 the subject can be 120 - 11 - 14 = 95 characters and still
                // never hit `TooLong`. The word ceiling in `checker.rs` is what
                // actually constrains the answer now; this is the backstop that
                // keeps a pathological one composable.
                "subject": {
                    "type": "string",
                    "minLength": 8,
                    "maxLength": 95
                },
                "description": {
                    "type": "string",
                    "minLength": 15,
                    "maxLength": 320
                }
            }
        })
    }

    fn request_body(system: &str, evidence: &str) -> Value {
        json!({
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": evidence}
            ],
            "temperature": 0.0,
            "max_tokens": SLM_MAX_OUTPUT_TOKENS,
            "stream": false,
            "cache_prompt": false,
            "chat_template_kwargs": {"enable_thinking": false},
            // llama.cpp (and OpenAI) require the schema nested under
            // `json_schema`; a bare `schema` key is silently ignored, leaving
            // the output unconstrained (it dropped required fields -> parse
            // failure -> SLM_FAIL).
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "backlog_document_name",
                    "strict": true,
                    "schema": Self::response_schema()
                }
            }
        })
    }

    fn chat_content(response: &Value) -> anyhow::Result<String> {
        let content = &response["choices"][0]["message"]["content"];
        if let Some(text) = content.as_str() {
            return Ok(text.trim().to_string());
        }
        if let Some(parts) = content.as_array() {
            let joined = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if !joined.trim().is_empty() {
                return Ok(joined.trim().to_string());
            }
        }
        anyhow::bail!("llama-server chat response did not contain assistant content")
    }

    fn json_object(text: &str) -> anyhow::Result<&str> {
        let trimmed = text.trim();
        let start = trimmed
            .find('{')
            .ok_or_else(|| anyhow::anyhow!("assistant content contained no JSON object"))?;
        let end = trimmed
            .rfind('}')
            .ok_or_else(|| anyhow::anyhow!("assistant content contained incomplete JSON"))?;
        anyhow::ensure!(end >= start, "assistant JSON boundaries were invalid");
        Ok(&trimmed[start..=end])
    }

    /// The naming system prompt, split out as a pure helper so tests can
    /// assert its exact bytes without a live llama-server (mirrors the
    /// `request_body` and `server_args` splits above).
    ///
    /// `naming_notes` is the operator's `Config::custom_naming_notes`: when
    /// non-empty after trimming it is appended AFTER the core rules as an
    /// explicitly subordinate section, and BEFORE the violation note so the
    /// correction stays the last, most salient instruction. When empty the
    /// core prompt stays byte-identical — its shape is measured (see below)
    /// and must not drift for operators who configured nothing.
    fn build_system_prompt(
        language: &str,
        doc_type: &str,
        naming_notes: &str,
        violation_note: Option<&str>,
    ) -> String {
        // The two `subject` rules below are the measured shape, and the length of
        // this prompt is a throughput decision as much as a quality one.
        //
        // Three configurations over the same 40 documents, scoring the party in
        // the filename against the document's own `Taxpayer / Entity:` line:
        //
        //   subject rule            party exact   named ok   s/file
        //   0.4.2 (one bullet)         18 of 40     40/40      9.58
        //   four explicit bullets      37 of 40     39/40     22.95
        //   these two bullets          38 of 40     40/40     11.61
        //
        // The four-bullet version stated every prohibition separately and read as
        // the safer prompt. It cost 2x the wall clock for no gain in party
        // accuracy — it was slightly better on dates (11 run-dated against 14, and
        // 6 of 8 deep dates against 4) and slightly worse on everything else. A
        // longer system prompt is re-sent on every naming attempt and every
        // escalation, so prompt words are not free here.
        //
        // Worth knowing before rewriting this: judging the four-bullet version on
        // its first ten documents said it was clearly better, and scoring all 40
        // said the opposite. Re-run `e2e_real_batch` over the whole sample and
        // compare the party buckets; ten documents will mislead you.
        //
        // Known noise this leaves behind: `SUBJECT_TRUNCATED` fires on 35 of 40,
        // because the model writes past eight words and the checker trims at a
        // word boundary. That is the flagged, clean outcome replacing a silent
        // mid-word cut, but it is loud. See docs/KNOWN_ISSUES.md item 0h.
        // No "Today's date" line, deliberately. Nothing downstream consumes it —
        // the checker computes its own now() for the future-date ceiling, and the
        // metadata fallback comes from the file's mtime — so the only thing the
        // line ever did was hand a weak model a concrete, salient date string
        // right next to an instruction not to use it. On dateless documents the
        // 0.6B reached for it anyway, DATE_NOT_IN_EVIDENCE rejected it, and the
        // ladder escalated to the 1.7B: removing the line cut escalations ~3x in
        // the 2026-07 stress campaign.
        //
        // No concrete example names in the subject rules, for the same reason.
        // The former example `Form 8829 - Marcus Alvarez` (plus the form-number
        // enumeration next to it) was copied verbatim into the subject of 34 of
        // 54 files in the 2026-08-05 v0.8.2 validation run — the 0.6B parrots
        // any salient literal it is handed. The checker cannot catch this: the
        // subject is well-formed, only unfaithful. Keep these rules structural;
        // the one surviving example lives in the description rule.
        //
        // That surviving example DOES parrot, and saying otherwise here was
        // wrong. The 2026-08-06 1.7B/4B batch named `v090b_edge_garbage.pdf` —
        // a deliberately contentless fixture — as
        // `2026-08-06 Shareholder's register - John Smith.pdf`, description
        // `Shareholder's register transferring 40,000 shares to John Smith.`,
        // which is this rule's example copied whole. It shipped `ok`, not
        // flagged: the date fell back to mtime, and the subject is
        // well-formed, so nothing downstream had grounds to reject it.
        //
        // Deliberately not fixed here. Removing the example is a one-line
        // change with a measured cost — the example is what stops descriptions
        // opening `The document...`, and 0.4.3's numbers were bought partly by
        // it — so it needs its own A/B over the full sample rather than a
        // reflex during a model-tier change. The failure mode is confined to
        // documents with no extractable content, where every possible name is
        // wrong; the real defect is that such a document is named at all
        // instead of being flagged for having nothing to name it from.
        // Recorded in docs/KNOWN_ISSUES.md.
        let mut system = format!(
            "You name business and legal documents from evidence excerpts.\n\
             Document language: {language}. Classified type: {doc_type}.\n\
             Do not reveal reasoning. Return only the requested JSON object.\n\
             Rules:\n\
             - date: extract the date written IN the document body (for example a letter date, filing date, or effective date), formatted YYYY-MM-DD. Never invent a date that is not present in the text. Use none only if the body contains no date at all.\n\
             - date_source: use document when the date appears in the body text; use metadata only when the body has no date of its own; use none when no date exists.\n\
             - subject: exactly `<short form> - <party>`, at most 8 words. <short form> is the document's own short identifier: its form number if it shows one, otherwise its document type in a few words, never a full legal title. <party> is the one party the document belongs to, copied exactly from the document text, named once and never omitted.\n\
             - subject: every word of the subject must come from this document. Add nothing else — no tax year, no EIN, no address, no generic word such as Document or Scan, and never the labels Taxpayer or Entity.\n\
             - description: exactly ONE sentence, 15 to 200 characters, adding useful information beyond the subject. It must end with a single full stop. Do not write a second sentence, and do not stop mid-sentence.\n\
             - description: begin with the document type or action itself, for example `Shareholder's register transferring 40,000 shares to John Smith.` — never open with `The document`, `This document`, or `The file`.\n\
             Never invent dates, parties, or facts."
        );
        let notes = naming_notes.trim();
        if !notes.is_empty() {
            // Subordinate by construction: the section names itself as
            // preferences and defers to the rules above, and it carries no
            // concrete example filenames of its own unless the operator
            // typed them — the parrot risk documented above is theirs to
            // weigh, the core rules stay structural.
            system.push_str(&format!(
                "\nOperator preferences (apply them only where they do not conflict with the rules above):\n{notes}"
            ));
        }
        if let Some(violation) = violation_note {
            system.push_str(&format!(
                "\nA prior proposal was rejected by the deterministic validator: {violation}. Correct that exact problem."
            ));
        }
        system
    }

    pub async fn name_document(
        &self,
        tier: Tier,
        evidence: &str,
        doc_type: &str,
        language: &str,
        violation_note: Option<&str>,
    ) -> anyhow::Result<SlmOutput> {
        let (port, _serve_guard) = self.ensure_up(tier).await?;
        let system =
            Self::build_system_prompt(language, doc_type, &self.naming_notes, violation_note);

        let body = Self::request_body(&system, evidence);
        let url = format!("http://127.0.0.1:{port}/v1/chat/completions");
        let response: Value = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let content = Self::chat_content(&response)?;
        let object = Self::json_object(&content)?;
        serde_json::from_str(object).map_err(|error| {
            anyhow::anyhow!(
                "SLM output failed JSON parsing despite schema constraint: {error}; raw: {content}"
            )
        })
    }

    pub fn shutdown(&self) {
        // `Server`'s Drop kills and reaps; taking the slot is the whole job.
        for slot in [&self.primary, &self.escalation] {
            drop(slot.lock().unwrap_or_else(|e| e.into_inner()).take());
        }
    }

    /// Permanently stop this lane for the lifetime of the process.
    ///
    /// The latch is set before either slot is taken so an in-flight request
    /// that reaches `ensure_up` after shutdown began fails closed instead of
    /// replacing the child that the exit path just killed.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown();
    }

    #[cfg(test)]
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Spawns the periodic escalation reaper, using this lane's own
    /// `escalation_idle_secs` (a no-op task if that is 0). Must be called
    /// from a live Tokio runtime — `SlmLane::new` stays synchronous and
    /// reactor-free on purpose (it is constructed from plain `#[test] fn`s
    /// with no runtime, and from `spawn_blocking` closures). Uses a `Weak`
    /// reference: the task exits on its own once every `Arc<SlmLane>` clone
    /// is dropped, so the caller does not need to hold or abort the returned
    /// handle. It ALSO checks `shutting_down` every tick, because in the
    /// shipped app `Drop` never runs at all (`App::run` exits via
    /// `std::process::exit`) and `stop_pipeline_inner` sets `shutting_down`
    /// explicitly instead. Either signal alone is sufficient; both are
    /// present because only one of them fires in production.
    pub fn spawn_idle_reaper(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let idle_secs = self.escalation_idle_secs;
        self.spawn_idle_reaper_with_poll(idle_secs, ESCALATION_REAP_POLL_INTERVAL)
    }

    pub(crate) fn spawn_idle_reaper_with_poll(
        self: Arc<Self>,
        idle_secs: u64,
        poll: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Self> = Arc::downgrade(&self);
        tokio::spawn(async move {
            if idle_secs == 0 {
                return;
            }
            let mut ticker = tokio::time::interval(poll);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(lane) = weak.upgrade() else {
                    return;
                };
                if lane.shutting_down.load(Ordering::SeqCst) {
                    return;
                }
                lane.reap_idle_escalation(idle_secs);
            }
        })
    }

    fn reap_idle_escalation(&self, idle_secs: u64) {
        // Guards itself even in collapsed mode: escalation_inflight and
        // escalation_last_completion are only ever touched by a request that
        // resolved to ResolvedSlot::Escalation, which never happens when
        // collapsed (Tier::Escalation resolves to Primary). So in collapsed
        // mode `last` stays None forever and this is a no-op — no explicit
        // `escalation_collapsed()` check needed here.
        if self.escalation_inflight.load(Ordering::SeqCst) > 0 {
            return;
        }
        let mut last = self.escalation_last_completion.lock().unwrap();
        let Some(completed_at) = *last else { return };
        if completed_at.elapsed() >= Duration::from_secs(idle_secs) {
            *last = None;
            drop(last);
            let mut guard = self.escalation.lock().unwrap();
            if guard.is_some() {
                *guard = None; // Server::drop kills the child
                log::info!("escalation llama-server reaped after {idle_secs}s idle");
            }
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn primary_child_pid(&self) -> Option<u32> {
        self.primary.lock().unwrap().as_ref().map(|s| s.child.id())
    }

    #[cfg(test)] // used by the platform-agnostic collapsed-reap test too
    pub(crate) fn escalation_child_pid(&self) -> Option<u32> {
        self.escalation
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.child.id())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn primary_inflight(&self) -> u64 {
        self.primary_inflight.load(Ordering::SeqCst)
    }
}

impl Drop for SlmLane {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_latch_prevents_a_server_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let lane = SlmLane::new(
            dir.path().join("missing-llama-server"),
            String::new(),
            dir.path().join("missing-primary.gguf"),
            dir.path().join("missing-escalation.gguf"),
            28_137,
            1,
            2,
            SlmTuning::default(),
        );

        lane.begin_shutdown();
        assert!(lane.is_shutting_down());
        // `ServeGuard` (the Ok side) is deliberately not `Debug`, so this
        // goes through `.err()` rather than `unwrap_err()`.
        let error = lane.ensure_up(Tier::Primary).await.err().unwrap();
        assert!(
            error.to_string().contains("shutting down"),
            "shutdown must fail before inspecting or spawning any binary: {error}"
        );
    }

    #[test]
    fn request_uses_chat_template_and_schema_without_raw_prompt() {
        let body = SlmLane::request_body("system", "evidence");
        assert!(body.get("prompt").is_none());
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn extracts_content_from_openai_style_response() {
        let response = json!({
            "choices": [{"message": {"content": "{\"date\":\"none\"}"}}]
        });
        assert_eq!(
            SlmLane::chat_content(&response).unwrap(),
            "{\"date\":\"none\"}"
        );
    }

    #[test]
    fn isolates_json_from_template_noise() {
        let value = "<think></think>\n```json\n{\"date\":\"none\"}\n```";
        assert_eq!(SlmLane::json_object(value).unwrap(), "{\"date\":\"none\"}");
    }

    #[test]
    fn system_prompt_includes_operator_section_verbatim_when_notes_are_set() {
        let notes = "Prefer the client surname over the firm name.";
        let prompt = SlmLane::build_system_prompt("en", "invoice", notes, None);
        assert!(prompt.contains(
            "\nOperator preferences (apply them only where they do not \
             conflict with the rules above):\nPrefer the client surname over \
             the firm name."
        ));
        // Notes only ever append: everything before them is the untouched
        // core prompt, byte for byte.
        let core = SlmLane::build_system_prompt("en", "invoice", "", None);
        assert!(prompt.starts_with(&core));
        assert!(prompt.ends_with(notes));
    }

    #[test]
    fn system_prompt_is_byte_identical_when_notes_are_empty() {
        let prompt = SlmLane::build_system_prompt("en", "invoice", "", None);
        // No trace of the operator section — not even its header.
        assert!(!prompt.contains("Operator preferences"));
        // The measured core prompt still ends on its own closing rule.
        assert!(prompt.ends_with("Never invent dates, parties, or facts."));
        // Whitespace-only notes are unset notes, not an empty section.
        assert_eq!(
            SlmLane::build_system_prompt("en", "invoice", " \n\t ", None),
            prompt
        );
    }

    #[test]
    fn operator_notes_precede_the_violation_note() {
        let prompt = SlmLane::build_system_prompt(
            "en",
            "invoice",
            "Use short client names.",
            Some("subject exceeded eight words"),
        );
        let notes_at = prompt
            .find("\nOperator preferences (apply them only where they do not conflict with the rules above):\nUse short client names.")
            .expect("operator section present");
        let violation_at = prompt
            .find("\nA prior proposal was rejected by the deterministic validator: subject exceeded eight words. Correct that exact problem.")
            .expect("violation note present");
        assert!(
            notes_at < violation_at,
            "the correction must stay the last, most salient instruction"
        );
    }

    #[test]
    fn with_naming_notes_stores_the_operator_notes_on_the_lane() {
        let with_notes = lane(19_337).with_naming_notes("prefer surnames".to_string());
        assert_eq!(with_notes.naming_notes, "prefer surnames");
        assert_eq!(lane(19_338).naming_notes, "");
    }

    fn lane(base_port: u16) -> SlmLane {
        SlmLane::new(
            PathBuf::from("/nonexistent/llama-server"),
            String::new(),
            PathBuf::from("/nonexistent/primary.gguf"),
            PathBuf::from("/nonexistent/escalation.gguf"),
            base_port,
            1,
            2,
            SlmTuning::default(),
        )
    }

    /// Two lanes must never share a token, and a token must be long enough
    /// that a local process cannot simply guess its way onto the endpoint.
    #[test]
    fn each_lane_gets_its_own_unguessable_api_key() {
        let (a, b) = (lane(19_137), lane(19_137));
        assert_eq!(a.api_key.len(), 64);
        assert!(a.api_key.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.api_key, b.api_key, "the token must be per-run");
    }

    /// The squatter case: something already owns the configured port, so the
    /// lane must move rather than spawn into it and then trust whatever
    /// answers there.
    #[test]
    fn reserve_port_skips_a_port_someone_else_already_holds() {
        let squatter = TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = squatter.local_addr().unwrap().port();

        let chosen = reserve_port(taken).expect("a free port above the taken one");
        assert_ne!(chosen, taken, "must not hand back an occupied port");
        assert!(chosen > taken && chosen <= taken + PORT_SCAN_RANGE);

        // And the port it did hand back is genuinely bindable.
        drop(TcpListener::bind(("127.0.0.1", chosen)).expect("chosen port must be free"));
    }

    #[test]
    fn reserve_port_returns_the_preferred_port_when_it_is_free() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let free = probe.local_addr().unwrap().port();
        drop(probe);
        assert_eq!(reserve_port(free).unwrap(), free);
    }

    /// A missing or dead llama-server must fail fast with something a support
    /// ticket can act on, not after a full `HEALTH_TIMEOUT` poll against a
    /// corpse.
    #[tokio::test]
    async fn ensure_up_fails_immediately_when_the_binary_cannot_be_spawned() {
        let lane = lane(19_237);
        let started = Instant::now();
        let error = lane
            .ensure_up(Tier::Primary)
            .await
            .err()
            .expect("a nonexistent GGUF must not be waited on");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not burn the health timeout first"
        );
        assert!(
            error.to_string().contains("GGUF model not found"),
            "{error}"
        );
        assert!(
            lane.primary.lock().unwrap().is_none(),
            "a failed spawn must not leave a slot behind"
        );
    }

    /// Expectations are computed from `SLM_CTX_PER_SLOT` rather than restated
    /// as literals: the constant is sized against the escalation evidence
    /// budget and will move again when that is remeasured, and a test that
    /// hardcoded 8192/4096 would keep passing while asserting the wrong thing
    /// — the one failure mode this test exists to catch. What it does pin is
    /// the *relationship*: per-slot context times that tier's own parallel.
    #[test]
    fn spawn_server_uses_the_resolved_tiers_own_parallel_and_ctx_size() {
        let get = |args: &[String], flag: &str| -> Option<String> {
            args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
        };
        let expected_ctx = |parallel: u32| (SLM_CTX_PER_SLOT * parallel).to_string();

        let primary_args =
            SlmLane::server_args(Path::new("/models/x.gguf"), 18_137, 2, 4, "secret-token");
        assert_eq!(get(&primary_args, "--parallel").as_deref(), Some("2"));
        assert_eq!(get(&primary_args, "--ctx-size"), Some(expected_ctx(2)));
        assert_eq!(get(&primary_args, "--threads").as_deref(), Some("4"));
        assert_eq!(get(&primary_args, "--port").as_deref(), Some("18137"));
        assert_eq!(
            get(&primary_args, "--api-key").as_deref(),
            Some("secret-token")
        );

        // A different tier's own parallel changes both --parallel and
        // --ctx-size together, independent of --threads.
        let escalation_args =
            SlmLane::server_args(Path::new("/models/x.gguf"), 18_138, 1, 4, "secret-token");
        assert_eq!(get(&escalation_args, "--parallel").as_deref(), Some("1"));
        assert_eq!(get(&escalation_args, "--ctx-size"), Some(expected_ctx(1)));
        assert_ne!(
            get(&primary_args, "--ctx-size"),
            get(&escalation_args, "--ctx-size"),
            "a per-slot size that ignored --parallel would give both tiers the same total"
        );
    }

    /// The slot has to hold the widest bundle the filter can hand it plus the
    /// prompt and the answer. That is how `SLM_CTX_PER_SLOT` was derived, and
    /// it is only true while all four constants agree — one of them being
    /// edited alone is precisely the silent overflow (llama.cpp truncates
    /// rather than refuses) that `filter::max_bundle_chars` exists to prevent.
    #[test]
    fn one_slot_holds_the_largest_bundle_plus_the_prompt_and_the_answer() {
        // The ceiling is expressed in characters; convert back at the same
        // pessimistic rate it was derived with.
        let evidence_tokens = crate::filter::max_bundle_chars()
            .div_ceil(crate::filter::CONSERVATIVE_CHARS_PER_TOKEN)
            as u32;
        assert!(
            evidence_tokens + SLM_PROMPT_RESERVE_TOKENS + SLM_MAX_OUTPUT_TOKENS <= SLM_CTX_PER_SLOT,
            "{evidence_tokens} evidence + {SLM_PROMPT_RESERVE_TOKENS} prompt + \
             {SLM_MAX_OUTPUT_TOKENS} answer must fit {SLM_CTX_PER_SLOT}"
        );
        assert_eq!(
            SLM_CTX_PER_SLOT % 256,
            0,
            "llama.cpp allocates KV in blocks; a ragged slot size buys nothing"
        );
    }

    /// §7.4 regression: nothing ever resolves to `ResolvedSlot::Escalation`
    /// when collapsed, so `escalation_last_completion` stays `None` forever
    /// and the reap sweep must be a no-op regardless of `idle_secs`.
    #[test]
    fn escalation_idle_reap_is_a_no_op_when_collapsed() {
        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared.gguf");
        let lane = SlmLane::new(
            dir.path().join("missing-llama-server"),
            String::new(),
            shared.clone(),
            shared,
            29_401,
            1,
            2,
            SlmTuning {
                escalation_idle_secs: 1,
                ..SlmTuning::default()
            },
        );
        assert!(lane.escalation_collapsed());
        lane.reap_idle_escalation(0);
        assert!(lane.escalation_child_pid().is_none());
    }

    // ---- fake-server infra (unix only: shebang script + chmod) -----------
    //
    // These tests drive a real, spawned llama-server stand-in through
    // `ensure_up` / the idle reaper, so — unlike the pure tests above — they
    // need a script this OS can exec. They mirror `pipeline.rs`'s
    // `FAKE_LLAMA_SERVER` pattern: `/health` only, since the recycle and
    // idle-reap logic under test never reaches `name_document`'s POST.

    #[cfg(unix)]
    const FAKE_LLAMA_SERVER_HEALTH_ONLY: &str = r##"#!/usr/bin/env python3
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        body = b'{"status":"ok"}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

port = int(sys.argv[sys.argv.index("--port") + 1])
server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
server.daemon_threads = True
server.serve_forever()
"##;

    #[cfg(unix)]
    fn write_fake_llama_server(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake-llama-server");
        std::fs::write(&p, FAKE_LLAMA_SERVER_HEALTH_ONLY).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    fn fake_lane(base_port: u16, dir: &Path, tuning: SlmTuning) -> SlmLane {
        let primary_gguf = dir.join("primary.gguf");
        let escalation_gguf = dir.join("escalation.gguf");
        std::fs::write(&primary_gguf, b"").unwrap();
        std::fs::write(&escalation_gguf, b"").unwrap();
        SlmLane::new(
            write_fake_llama_server(dir),
            String::new(),
            primary_gguf,
            escalation_gguf,
            base_port,
            2,
            2,
            tuning,
        )
    }

    /// The slot generalization itself, provable without spawning anything:
    /// each slot must own a distinct pair of counters. A `requests_served`
    /// that returned the primary's counter for both slots would make the
    /// escalation server recycle on the primary's traffic — and, worse, look
    /// correct in the unix-only spawn tests below, which only ever drive one
    /// slot at a time.
    #[test]
    fn each_slot_counts_its_own_requests_and_its_own_in_flight_work() {
        let lane = lane(19_437);
        for slot in [ResolvedSlot::Primary, ResolvedSlot::Escalation] {
            lane.requests_served(slot).store(0, Ordering::SeqCst);
            lane.inflight(slot).store(0, Ordering::SeqCst);
        }

        lane.requests_served(ResolvedSlot::Primary)
            .fetch_add(7, Ordering::SeqCst);
        lane.inflight(ResolvedSlot::Escalation)
            .fetch_add(3, Ordering::SeqCst);

        assert_eq!(
            lane.requests_served(ResolvedSlot::Primary)
                .load(Ordering::SeqCst),
            7
        );
        assert_eq!(
            lane.requests_served(ResolvedSlot::Escalation)
                .load(Ordering::SeqCst),
            0,
            "the escalation server must not inherit the primary's request count"
        );
        assert_eq!(
            lane.inflight(ResolvedSlot::Primary).load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            lane.inflight(ResolvedSlot::Escalation)
                .load(Ordering::SeqCst),
            3
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escalation_recycles_after_the_configured_request_count() {
        let dir = tempfile::tempdir().unwrap();
        let lane = fake_lane(
            29_501,
            dir.path(),
            SlmTuning {
                recycle_after_requests: 3,
                ..SlmTuning::default()
            },
        );

        // The escalation server is the larger of the two tiers now, so it is
        // the one whose unbounded growth actually threatens the machine. Until
        // the 1.7B/4B change it was never recycled at all: idle-reaping was
        // the only bound, and it by definition never fires on the busy server
        // that is doing the growing.
        let mut first_pid = None;
        for call in 1..=4u32 {
            let (_port, guard) = lane.ensure_up(Tier::Escalation).await.unwrap();
            drop(guard);
            let pid = lane.escalation_child_pid().expect("server must be up");
            match call {
                1 => first_pid = Some(pid),
                3 => assert_eq!(
                    Some(pid),
                    first_pid,
                    "must not recycle before the configured threshold"
                ),
                4 => assert_ne!(
                    Some(pid),
                    first_pid,
                    "the 4th call must recycle after 3 served requests"
                ),
                _ => {}
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn primary_recycles_after_the_configured_request_count() {
        let dir = tempfile::tempdir().unwrap();
        let lane = fake_lane(
            29_101,
            dir.path(),
            SlmTuning {
                recycle_after_requests: 3,
                ..SlmTuning::default()
            },
        );

        let mut first_pid = None;
        for call in 1..=4u32 {
            let (_port, guard) = lane.ensure_up(Tier::Primary).await.unwrap();
            drop(guard);
            let pid = lane.primary_child_pid().expect("server must be up");
            match call {
                1 => first_pid = Some(pid),
                3 => assert_eq!(
                    Some(pid),
                    first_pid,
                    "must not recycle before the configured threshold"
                ),
                4 => assert_ne!(
                    Some(pid),
                    first_pid,
                    "the 4th call must recycle after 3 served requests"
                ),
                _ => {}
            }
        }
    }

    /// The "two concurrent calls" from the design are modelled by holding
    /// both `ServeGuard`s open at once rather than racing real tasks: the
    /// reservation each `ensure_up` call makes inside its critical section
    /// (§5 lock-ordering rule 1) is exactly what the recycle check reads,
    /// so this proves the same invariant deterministically, regardless of
    /// how two real concurrent callers happen to interleave.
    #[cfg(unix)]
    #[tokio::test]
    async fn primary_recycle_never_kills_an_in_flight_request() {
        let dir = tempfile::tempdir().unwrap();
        let lane = fake_lane(
            29_201,
            dir.path(),
            SlmTuning {
                recycle_after_requests: 1,
                ..SlmTuning::default()
            },
        );

        let (port1, guard1) = lane.ensure_up(Tier::Primary).await.unwrap();
        assert_eq!(lane.primary_inflight(), 1);
        let (port2, guard2) = lane.ensure_up(Tier::Primary).await.unwrap();
        assert_eq!(port1, port2, "both in-flight calls must share the server");
        assert_eq!(lane.primary_inflight(), 2);

        let pid_while_in_flight = lane.primary_child_pid().expect("server must be up");

        drop(guard1);
        drop(guard2);
        assert_eq!(lane.primary_inflight(), 0);

        let (_port3, guard3) = lane.ensure_up(Tier::Primary).await.unwrap();
        let pid_after_third_call = lane.primary_child_pid().expect("server must be up");
        assert_ne!(
            pid_while_in_flight, pid_after_third_call,
            "the 3rd call must recycle now that nothing is in flight"
        );
        drop(guard3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escalation_server_is_reaped_after_idle_and_never_while_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let lane = Arc::new(fake_lane(
            29_301,
            dir.path(),
            SlmTuning {
                escalation_idle_secs: 1,
                ..SlmTuning::default()
            },
        ));
        let _reaper = lane
            .clone()
            .spawn_idle_reaper_with_poll(1, Duration::from_millis(50));

        // Phase 1: a completed request followed by silence must be reaped
        // once the idle window elapses.
        let (_port, guard) = lane.ensure_up(Tier::Escalation).await.unwrap();
        drop(guard); // marks the completion timestamp the reaper reads
        assert!(
            lane.escalation_child_pid().is_some(),
            "the escalation server must be up right after completion"
        );
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            lane.escalation_child_pid().is_none(),
            "an idle escalation server must be reaped"
        );

        // Phase 2: a request still in flight when the idle window would
        // otherwise have elapsed must never be reaped mid-flight — the clock
        // only ever starts on COMPLETION, never on start.
        let (_port2, guard2) = lane.ensure_up(Tier::Escalation).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            lane.escalation_child_pid().is_some(),
            "a server serving an in-flight request must never be reaped"
        );
        drop(guard2);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            lane.escalation_child_pid().is_none(),
            "the same server must be reaped once the request actually completes and goes idle"
        );
    }
}
