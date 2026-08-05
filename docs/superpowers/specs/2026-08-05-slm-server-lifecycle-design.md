# Server Lifecycle Manager — design doc (2026-08-05)

Status: designed, not implemented. Produced by the post-v0.8.1 research pass;
grounded against `slm.rs` (full), `pipeline.rs` (naming path, all Pipeline
literal sites), `config.rs`, `lib.rs` (start/stop/exit paths), `watcher.rs`,
and `docs/SIZING.md`.

Key fact that shapes the whole design: **`stop_pipeline_inner` (lib.rs:1249)
exists because `App::run` exits via `std::process::exit`, which runs no
destructors** (lib.rs:1295-1303 comment). `Drop for SlmLane` (slm.rs:548-552)
never fires on real shutdown — only the explicit `pipeline.slm.begin_shutdown()`
call at lib.rs:1256 does. Any new periodic task must be stoppable by that
explicit call, not by relying on `Drop`.

---

## 1. `slm.rs` changes

### 1.1 New imports
```rust
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};   // Arc/Weak are new
```

### 1.2 New types
```rust
/// Which physical slot a request actually landed on. Needed because
/// collapse (escalation_gguf == primary_gguf, the 8 GB single-model
/// install) routes Tier::Escalation calls onto `self.primary` — recycling
/// and idle-reap must key off the physical slot, not the nominal Tier, or
/// a collapsed install would never recycle and a non-collapsed one would
/// double-count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ResolvedSlot { Primary, Escalation }

/// Per-tier server tuning. Grouped so adding a knob touches one call
/// site's field list, not every positional `SlmLane::new` call's argument
/// order (8 call sites today: lib.rs x2, pipeline.rs x4, slm.rs x2).
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
        Self { escalation_parallel: 1, recycle_after_requests: 0, escalation_idle_secs: 0 }
    }
}

/// Reserves one in-flight dispatch against a resolved slot for the
/// lifetime of one naming attempt (one `name_document` call, including the
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
                // this is what makes the July-campaign bug (reaping a
                // saturated, legitimately-busy server) structurally
                // impossible: the clock only ever resets when a request
                // actually finishes.
                *self.lane.escalation_last_completion.lock().unwrap() = Some(Instant::now());
            }
        }
    }
}
```

### 1.3 New consts
```rust
/// How often the idle reaper polls. Independent of the idle window itself
/// so the check granularity stays coarse regardless of what an operator
/// configures for `slm_escalation_idle_secs`.
pub(crate) const ESCALATION_REAP_POLL_INTERVAL: Duration = Duration::from_secs(30);
```
The recycle threshold itself is `Config::slm_recycle_after_requests`
(operator-tunable); no const needed for the number. Rationale for existence:
unfixed Windows RSS growth in upstream llama.cpp (ggml-org/llama.cpp#24356;
observed 3.45→4.45 GB over 21 files).

### 1.4 `SlmLane` struct diff
```rust
pub struct SlmLane {
    _fallback_grammar: String,
    llama_server_exe: PathBuf,
    primary_gguf: PathBuf,
    escalation_gguf: PathBuf,
-   parallel: u8,
+   primary_parallel: u8,
+   escalation_parallel: u8,
    threads: usize,
    primary_port: u16,
    escalation_port: u16,
    api_key: String,
    shutting_down: AtomicBool,
    primary: Mutex<Option<Server>>,
    escalation: Mutex<Option<Server>>,
    http: reqwest::Client,
+   recycle_after_requests: u32,
+   escalation_idle_secs: u64,
+   primary_requests_served: AtomicU64,   // reset to 0 on every successful (re)spawn
+   primary_inflight: AtomicU64,          // requests currently dispatched to the live primary child
+   escalation_inflight: AtomicU64,       // same, for a genuinely separate (non-collapsed) escalation child
+   escalation_last_completion: Mutex<Option<Instant>>, // None until the first escalation request completes
}
```
`Server` (slm.rs:51-62) is **unchanged** — counters live on `SlmLane`, not
per-process, so they survive across the respawn they help trigger and reset
explicitly rather than by construction.

### 1.5 Constructor signature
```rust
pub fn new(
    llama_server_exe: PathBuf,
    grammar: String,
    primary_gguf: PathBuf,
    escalation_gguf: PathBuf,
    base_port: u16,
    primary_parallel: u8,   // was `parallel`; same position, renamed only
    threads: usize,
    tuning: SlmTuning,      // NEW, trailing — minimizes call-site diff
) -> Self
```
Body: set the new fields from `tuning`; counters zeroed;
`escalation_last_completion: Mutex::new(None)`.

### 1.6 `spawn_server` — per-tier `--parallel` / `--ctx-size`
```rust
fn spawn_server(&self, gguf: &Path, port: u16, parallel: u8) -> anyhow::Result<Server> {
    ...
    "--parallel", &parallel.to_string(),
    ...
    "--ctx-size", &(4096u32 * parallel as u32).to_string(),
    ...
}
```
Recommend also extracting the arg construction into a pure
`fn server_args(gguf: &Path, port: u16, parallel: u8, threads: usize, api_key: &str) -> Vec<String>`
(mirrors `rung()`'s "provable without a live llama-server" split at
pipeline.rs:1090-1100) so ctx-size/parallel-per-tier is unit-testable without
spawning a process.

```rust
fn parallel_for(&self, slot: ResolvedSlot) -> u8 {
    match slot {
        ResolvedSlot::Primary => self.primary_parallel,
        ResolvedSlot::Escalation => self.escalation_parallel,
    }
}
```

### 1.7 `ensure_up` — recycle + inflight tracking (replaces slm.rs:211-296)
```rust
pub fn escalation_collapsed(&self) -> bool {
    self.escalation_gguf == self.primary_gguf
}

async fn ensure_up(&self, tier: Tier) -> anyhow::Result<(u16, ServeGuard<'_>)> {
    anyhow::ensure!(!self.shutting_down.load(Ordering::SeqCst), "llama-server is shutting down");
    let collapse = tier == Tier::Escalation && self.escalation_collapsed();
    let (slot, gguf, preferred_port, resolved) = match tier {
        Tier::Primary => (&self.primary, &self.primary_gguf, self.primary_port, ResolvedSlot::Primary),
        Tier::Escalation if collapse =>
            (&self.primary, &self.primary_gguf, self.primary_port, ResolvedSlot::Primary),
        Tier::Escalation =>
            (&self.escalation, &self.escalation_gguf, self.escalation_port, ResolvedSlot::Escalation),
    };

    let port = {
        let mut guard = slot.lock().unwrap();
        anyhow::ensure!(!self.shutting_down.load(Ordering::SeqCst), "llama-server is shutting down");

        // Recycle check runs BEFORE the live/dead check and under the same
        // lock as the respawn decision below — this is the only place two
        // concurrent ensure_up calls for the primary slot are serialized,
        // and it is what makes "never race an in-flight request" provable:
        // the inflight read here can never be stale relative to another
        // task's reservation, because reservations (below) are also made
        // inside this same critical section.
        if resolved == ResolvedSlot::Primary {
            if guard.is_some() {
                let served = self.primary_requests_served.load(Ordering::SeqCst);
                let inflight = self.primary_inflight.load(Ordering::SeqCst);
                if self.recycle_after_requests > 0
                    && served as u64 >= self.recycle_after_requests as u64
                    && inflight == 0
                {
                    log::info!("recycling primary llama-server after {served} requests");
                    *guard = None; // Server::drop kills the outgoing child
                }
                // inflight > 0: defer. The next ensure_up call that finds
                // inflight == 0 will recycle instead. See §7 for the
                // starvation tradeoff this accepts.
            }
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
                *guard = None;
                let port = reserve_port(preferred_port)?;
                let server = self.spawn_server(gguf, port, self.parallel_for(resolved))?;
                *guard = Some(server);
                if resolved == ResolvedSlot::Primary {
                    self.primary_requests_served.store(0, Ordering::SeqCst);
                }
                port
            }
        };

        // Reserve the dispatch while still holding the lock (see comment
        // above). This does NOT yet count toward `primary_requests_served`
        // — that only happens once health is confirmed, so a server that
        // never comes up doesn't inflate the "served" count that gates the
        // NEXT recycle decision.
        match resolved {
            ResolvedSlot::Primary => { self.primary_inflight.fetch_add(1, Ordering::SeqCst); }
            ResolvedSlot::Escalation => { self.escalation_inflight.fetch_add(1, Ordering::SeqCst); }
        }
        port
    };

    // Every exit from here must either return a ServeGuard (which owns the
    // reservation made above) or explicitly release it — a health-check
    // failure that leaked the reservation would freeze `primary_inflight`
    // at "busy" forever and permanently disable recycling.
    let release = |lane: &Self| match resolved {
        ResolvedSlot::Primary => { lane.primary_inflight.fetch_sub(1, Ordering::SeqCst); }
        ResolvedSlot::Escalation => { lane.escalation_inflight.fetch_sub(1, Ordering::SeqCst); }
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
            anyhow::bail!("llama-server exited during startup ({exit}); its output is in the log file");
        }
        if let Ok(response) = self.http.get(&health_url).bearer_auth(&self.api_key).send().await {
            if response.status().is_success() {
                if resolved == ResolvedSlot::Primary {
                    self.primary_requests_served.fetch_add(1, Ordering::SeqCst);
                }
                return Ok((port, ServeGuard { lane: self, slot: resolved }));
            }
        }
        if Instant::now() >= deadline { break; }
        tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
    }
    *slot.lock().unwrap() = None;
    release(self);
    anyhow::bail!("llama-server on port {port} did not become healthy within {}s; it has been stopped", HEALTH_TIMEOUT.as_secs())
}
```

`name_document` (slm.rs:440-523) changes one line:
```rust
- let port = self.ensure_up(tier).await?;
+ let (port, _serve_guard) = self.ensure_up(tier).await?;
```
`_serve_guard`'s natural drop point (end of fn, or the `?` early-return from
`.error_for_status()?`/`.json().await?`/parse) is exactly "request completion"
— success or failure — which is what both the recycle-inflight gate and the
escalation idle clock need.

### 1.8 Idle reaper
```rust
impl SlmLane {
    /// Spawns the periodic escalation reaper. Must be called from a live
    /// Tokio runtime — `SlmLane::new` stays synchronous and reactor-free on
    /// purpose (it is constructed from plain `#[test] fn`s with no runtime,
    /// e.g. `lane()` at slm.rs:611, and from `spawn_blocking` closures at
    /// lib.rs:1116). Uses a Weak reference: the task exits on its own once
    /// every `Arc<SlmLane>` clone is dropped, so the caller does not need
    /// to hold or abort the returned handle. It ALSO checks `shutting_down`
    /// every tick, because in the shipped app `Drop` never runs at all
    /// (`App::run` exits via `std::process::exit` — see lib.rs:1295-1303)
    /// and `stop_pipeline_inner` (lib.rs:1249) sets `shutting_down`
    /// explicitly instead. Either signal alone is sufficient; both are
    /// present because only one of them fires in production.
    pub fn spawn_idle_reaper(self: &Arc<Self>, idle_secs: u64) -> tokio::task::JoinHandle<()> {
        self.spawn_idle_reaper_with_poll(idle_secs, ESCALATION_REAP_POLL_INTERVAL)
    }

    pub(crate) fn spawn_idle_reaper_with_poll(
        self: &Arc<Self>,
        idle_secs: u64,
        poll: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            if idle_secs == 0 { return; }
            let mut ticker = tokio::time::interval(poll);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(lane) = weak.upgrade() else { return };
                if lane.shutting_down.load(Ordering::SeqCst) { return; }
                lane.reap_idle_escalation(idle_secs);
            }
        })
    }

    fn reap_idle_escalation(&self, idle_secs: u64) {
        // Guards itself even in collapsed mode: escalation_inflight and
        // escalation_last_completion are only ever touched by a request
        // that resolved to ResolvedSlot::Escalation, which never happens
        // when collapsed (Tier::Escalation resolves to Primary). So in
        // collapsed mode `last` stays None forever and this is a no-op —
        // no explicit `escalation_collapsed()` check needed here.
        if self.escalation_inflight.load(Ordering::SeqCst) > 0 { return; }
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

    #[cfg(test)]
    pub(crate) fn primary_child_pid(&self) -> Option<u32> {
        self.primary.lock().unwrap().as_ref().map(|s| s.child.id())
    }
    #[cfg(test)]
    pub(crate) fn escalation_child_pid(&self) -> Option<u32> {
        self.escalation.lock().unwrap().as_ref().map(|s| s.child.id())
    }
    #[cfg(test)]
    pub(crate) fn primary_inflight(&self) -> u64 { self.primary_inflight.load(Ordering::SeqCst) }
}
```

Update the module doc comment (slm.rs:1-10): "remains resident for the batch"
is no longer accurate once `escalation_idle_secs > 0`; reword to "remains
resident until `escalation_idle_secs` of no completed request, or for the
batch when that is disabled."

### 1.9 `shutdown()` / `begin_shutdown()` — unchanged
No change needed: `shutdown()` (slm.rs:525-530) already clears both slots,
which kills whatever child is present regardless of how it got there. The
reaper task doesn't need explicit cancellation (§1.8 above).

---

## 2. `config.rs` changes

### 2.1 New fields (insert after `slm_parallel: u8,` at config.rs:50)
```rust
pub slm_escalation_parallel: u8,
/// Requests a primary llama-server serves before being killed and
/// respawned — llama.cpp Windows RSS growth is unfixed upstream
/// (ggml-org/llama.cpp#24356; measured 3.45->4.45 GB over 21 files).
/// 0 disables recycling.
pub slm_recycle_after_requests: u32,
/// Seconds since the escalation server's last request COMPLETED (never
/// mid-request — see `SlmLane::reap_idle_escalation`) before it is
/// dropped. 0 disables idle-reaping (resident for the process lifetime).
pub slm_escalation_idle_secs: u64,
```

### 2.2 Defaults (add to `Default for Config`)
```rust
slm_escalation_parallel: default_slm_escalation_parallel(),
slm_recycle_after_requests: 64,
slm_escalation_idle_secs: 600,
```
No per-field `#[serde(default=...)]` needed — the struct already carries
container-level `#[serde(default)]` (config.rs:29); old `backlog.config.json`
files pick these up transparently.

### 2.3 Default-from-RAM fn (mirrors `slm_parallel_for_ram`, config.rs:336-353)
```rust
fn default_slm_escalation_parallel() -> u8 {
    slm_escalation_parallel_for_ram(total_ram_gib())
}

/// Deliberately never inherits `slm_parallel_for_ram`'s 4 — the whole
/// point of a separate knob is decoupling escalation's KV cost from
/// primary's. SIZING.md measures 2,262 MB (parallel 1) vs 3,609 MB
/// (parallel 4) for the 1.7B; this keeps it at the low end always.
fn slm_escalation_parallel_for_ram(gib: Option<u64>) -> u8 {
    match gib {
        Some(g) if g <= 9 => 1,
        _ => 2, // >9 GiB and unknown RAM both get the small ceiling
    }
}
```

### 2.4 `clamp_resources_to_machine` (config.rs:426-450) — add block mirroring
the `slm_parallel` one, clamping `slm_escalation_parallel` down to
`slm_escalation_parallel_for_ram(gib)` with the same warn-log shape.

### 2.5 `validate()` — add after the `slm_parallel` check (config.rs:640-645):
`slm_escalation_parallel` in 1..=4; `slm_recycle_after_requests` 0 or ≤100000;
`slm_escalation_idle_secs` 0 or ≤86400 — same error-message style as
neighbors.

### 2.6 `redacted_config` (lib.rs:1027-1047) — add the three new fields for
parity with existing `slm_parallel`/`convert_workers` entries (diagnostics
payload only, not load-bearing).

---

## 3. `pipeline.rs` changes

### 3.1 New field (Pipeline struct, insert after `slm_slots` at line 35)
```rust
/// Gates concurrent Tier::Escalation HTTP calls independently of
/// `slm_slots`. Never acquired when `slm.escalation_collapsed()` — see
/// §7.4 for why gating a collapsed (single-server) install with this
/// would over-throttle it for no memory benefit.
escalation_slots: Arc<Semaphore>,
```

### 3.2 Construction — 6 struct-literal sites, all need
`escalation_slots: Arc::new(Semaphore::new(cfg.slm_escalation_parallel.max(1) as usize)),`
(or `Semaphore::new(1)` in fixed-config test harnesses):
- `Pipeline::new`, pipeline.rs:267-268 (after `slm_slots`)
- `Harness::with`, pipeline.rs:3363-3364
- `restarted_with`, pipeline.rs:3387-3388 — **clone, not construct**:
  `escalation_slots: self.pipeline.escalation_slots.clone(),`
- `e2e_real_batch`, pipeline.rs:5405-5406
- `a_file_queued_past_the_whole_cap_still_reaches_emitted`, pipeline.rs:5693-5694
- `a_timed_out_file_is_quarantined_with_a_timeout_reason`, pipeline.rs:5767-5768

### 3.3 `name_with_retries` loop (pipeline.rs:1134-1237) — acquire the
escalation permit per attempt, not for the whole ladder:
```rust
for attempt in 1..=self.cfg.max_stage_attempts.max(1) {
    let (tier, bundle) = self.rung(attempt, ev);
    let _escalation_permit = if tier == Tier::Escalation && !self.slm.escalation_collapsed() {
        Some(clock.parked(self.escalation_slots.acquire()).await.unwrap())
    } else {
        None
    };
    let out = self.slm.name_document(tier, &bundle, doc_type_hint, &ev.language, violation.as_deref()).await;
    // ... unchanged, including the span-mismatch re-prompt's second
    // name_document call at pipeline.rs:1167-1176, which reuses `tier` and
    // therefore correctly stays under the same `_escalation_permit`.
}
// _escalation_permit drops at end of each loop iteration — released
// before the next attempt, same `clock.parked` exclusion from the
// wall-clock cap that `slm_slots.acquire()` already gets at pipeline.rs:852.
```

---

## 4. `lib.rs` changes

### 4.1 `start_pipeline` (lib.rs:1213-1222) — SlmLane construction + reaper
```rust
let slm = Arc::new(SlmLane::new(
    binary(&app, "llama-server")?,
    grammar,
    cfg.slm_primary_gguf.clone(),
    cfg.effective_escalation_gguf().to_path_buf(),
    cfg.llama_port,
    cfg.slm_parallel,
    cfg.slm_threads(),
    SlmTuning {
        escalation_parallel: cfg.slm_escalation_parallel,
        recycle_after_requests: cfg.slm_recycle_after_requests,
        escalation_idle_secs: cfg.slm_escalation_idle_secs,
    },
));
let _idle_reaper = slm.clone().spawn_idle_reaper(cfg.slm_escalation_idle_secs);
let pipeline = Pipeline::new(cfg.clone(), state.ledger.clone(), sidecar, slm, app);
```
`start_pipeline` is itself `async fn` and calls this directly (not
`spawn_blocking`), so `tokio::spawn` inside `spawn_idle_reaper` has a live
reactor — this is the answer to "spawned where": **owned by `SlmLane` (the
method), spawned by the caller that already has both an `Arc<SlmLane>` and a
running Tokio context**, which is `start_pipeline` only. `_idle_reaper`'s
`JoinHandle` is intentionally dropped (detached) — per §1.8, the task
self-terminates via `Weak::upgrade` / `shutting_down`.

### 4.2 `review_only_pipeline` (lib.rs:1105-1137, inside `spawn_blocking`) —
construct with real tuning, **do not** spawn the reaper. This pipeline is
throwaway (one resubmit, then dropped), and its `Arc<SlmLane>`'s natural
`Drop` (this path DOES exit normally) already runs `begin_shutdown`.

### 4.3 `stop_pipeline_inner` — **no change**. `begin_shutdown()` at line 1256
already sets `shutting_down`, which the reaper's tick loop checks.

---

## 5. Lock-ordering rules

1. `slot.lock()` (the `Mutex<Option<Server>>` for the resolved physical slot)
   is the only lock ever held across the recycle-decision + respawn-decision +
   inflight-reservation sequence. **Both the read of counters and the
   `fetch_add` reservation happen inside the same critical section** (§1.7) —
   this is the entire correctness argument for "never race an in-flight
   request." Never move the reservation increment outside that block.
2. `ServeGuard`'s decrement (Drop) happens **without** the slot lock. Safe
   because a decrement can only make `inflight == 0` *more* true — a racing
   recycle-check can at worst defer a recycle by one more `ensure_up` call.
3. `escalation_last_completion`'s Mutex is independent of `slot.lock()`;
   `reap_idle_escalation` takes it, reads/clears, drops it, *then* takes
   `self.escalation.lock()` — never both at once. Keep that order
   (timestamp-check-then-kill) so a completion racing the reap is observed
   rather than killed.
4. Pipeline-level: `slm_slots` is always acquired before `escalation_slots`;
   nothing re-acquires `slm_slots` while holding `escalation_slots`. This
   one-directional order is what makes §7.3's deadlock analysis hold.

---

## 6. Spawn / teardown sequence

**Startup** (`start_pipeline`): build Sidecar → build SlmLane (no child
spawned; `ensure_up` stays lazy) → `spawn_idle_reaper` (no-op if
`idle_secs == 0`) → `Pipeline::new` (sizes `escalation_slots`) →
`watcher::spawn`.

**Teardown** (`stop_pipeline_inner` / process exit): pause → close
ingest_slots → `slm.begin_shutdown()` (kills children; reaper notices within
one 30 s tick — harmless if the process dies first) → sidecar shutdown →
release claims. `escalation_slots` is never `.close()`'d — consistent with
`slm_slots`; both rely on `shutting_down` failing `ensure_up` fast.

---

## 7. Failure modes

**7.1 Recycle mid-batch cold-start.** The file that triggers a recycle pays up
to HEALTH_TIMEOUT (60 s) before its naming call starts. `wall_clock_cap`
budgets 90×4 = 360 s for naming by default, so it fits; it is the same cost
the first file of any batch already pays. Accepted, bounded.

**7.2 Recycle starvation under sustained concurrency.** Recycle requires
`inflight == 0`; back-to-back load at `slm_parallel > 1` could defer it. In
practice permits are held across checker/ledger work, so gaps occur. Add an
observability line: warn if `served > 2 * recycle_after_requests` and still
not recycled.

**7.3 Escalation cap vs `slm_slots` — no deadlock.** Excess escalation
attempts queue on `escalation_slots.acquire()` (clock.parked-excluded, same
as the existing three semaphores — pipeline.rs:5643-5651 documents that P1
class). No circular wait: acquisition order is one-directional (§5.4). Effect
is FIFO queueing, not a hang — proven analytically and by test §8.4. Note the
real hazard the explicit semaphore avoids: capping only llama-server-side
`--parallel` would queue silently inside the server, invisible to
`clock.parked`, risking NAMING_HTTP_TIMEOUT (120 s) on requests that would
have succeeded.

**7.4 Collapse interaction.** When collapsed, `escalation_slots` is never
acquired and the reaper is a natural no-op (§1.8) — the only throttle is
`slm_slots` against the one physical server, which is correct.

**7.5 Shutdown races.** `ensure_up`'s three `shutting_down` checks fail
closed; the reap path re-checks the slot before clearing.

---

## 8. Test plan

### 8.1 Existing tests to update (compile-breaking only)
- slm.rs `shutdown_latch_prevents_a_server_respawn` (561), `lane()` (611) —
  append `SlmTuning::default()`.
- pipeline.rs `Harness::with` (3353) — `SlmTuning::default()` + the new
  `escalation_slots` literal; `restarted_with` (3387) clones it.
- pipeline.rs `e2e_what_the_model_proposes` (5202), `e2e_real_batch`
  (5389/5404-5419) — real cfg-derived tuning + sizing.
- pipeline.rs 5680/5757 literals — default tuning + `escalation_slots`.
- config.rs invalid-config table (~799): `slm_escalation_parallel = 0`/`5`,
  `slm_recycle_after_requests = 100_001`, `slm_escalation_idle_secs = 86_401`;
  RAM-tier + clamp-direction tests mirroring `slm_parallel`'s.

### 8.2 New fake-server infra (slm.rs test module — none exists today)
`#[cfg(unix)]` fake llama-server script whose `/health` returns
`{"status":"ok","pid": <getpid()>}`; a slow-POST variant (`time.sleep(N)`) for
in-flight-guard tests. Plus the `#[cfg(test)]` pid accessors (§1.8).

### 8.3 New unit tests (slm.rs)
- `primary_recycles_after_the_configured_request_count` — threshold 3, four
  sequential calls, pid differs before/after the 4th, all Ok.
- `primary_recycle_never_kills_an_in_flight_request` — threshold 1,
  parallel 2, slow server, two concurrent calls both Ok; respawn only on a
  3rd call.
- `escalation_server_is_reaped_after_idle_and_never_while_in_flight` —
  idle 1 s, poll 50 ms; pid Some → sleep 1.2 s → None; variant with a slow
  in-flight request spanning the window asserts pid unchanged.
- `escalation_idle_reap_is_a_no_op_when_collapsed` — regression for §7.4.
- `spawn_server_uses_the_resolved_tiers_own_parallel_and_ctx_size` — pure
  `server_args()` test, no process.

### 8.4 New pipeline-level tests (`#[cfg(unix)]` harness at 5656+)
- `escalation_slots_bounds_concurrency_without_deadlocking` — slm_parallel 2,
  escalation_parallel 1, both files forced to escalate; whole test under
  `tokio::time::timeout(30 s)`; both reach terminal states.
- `escalation_slots_not_acquired_when_collapsed` — permit count never drops
  while an escalation-tier attempt runs (needs a `#[cfg(test)]`
  `escalation_slots_available()` accessor).

### 8.5 `docs/SIZING.md`
Add a row for `slm_escalation_parallel: 2` (1.7B, calculated: ~2,262 + 448 ≈
2,710 MB, interpolated per the 448 MiB/slot formula), flagged as calculated
per the doc's own convention.
