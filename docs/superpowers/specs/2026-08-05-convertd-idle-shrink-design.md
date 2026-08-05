# Convertd idle pool shrink + RAM ceiling correction — design doc (2026-08-05)

Status: designed, not implemented. Grounded against `sidecar.rs` (full),
`config.rs` (full), `sidecar/BUILD.md`, `docs/SIZING.md`, `lib.rs:980-1309`,
and `convertd.py:95-140`.

## Corrections to common assumptions (read first)

1. **No tokio in `sidecar.rs`.** Only `std::sync::{Mutex, Condvar, Arc}` and
   `std::thread`. The reaper must be a plain OS thread.
2. **There is no graceful stdin-close retirement today.** `Proc`'s fields
   (`sidecar.rs:325-331`) drop in declaration order — `TrackedChild::drop`
   (`sidecar.rs:313-322`) runs *first* and hard-kills. The reaper gets the
   same hard kill as every existing retirement path — consistent, not a
   regression.
3. `Checkout::drop` is at `sidecar.rs:461-485`; `begin_shutdown`/drain at
   `487-516`; `spawn` at `535-630`.
4. **Stop is not wired in production.** `stop_pipeline` is deliberately
   unregistered (`lib.rs:1280-1293`); the pool only ever grows for the app's
   entire uptime — which is exactly why idle-shrink matters: after a batch,
   up to `convert_workers` processes sit fully loaded for days.

---

## Feature 1 — Idle Pool Shrink

### Where the reaper runs

**A dedicated background `std::thread`, spawned opt-in via a new
`Sidecar::spawn_idle_reaper(self: &Arc<Self>)`, called once by
`start_pipeline` after wrapping in `Arc`.** Not piggybacked on `checkout()` —
the point is to reap during a lull when nobody calls checkout for hours.

Not spawned in the constructor: six other call sites build short-lived
one-shot `Sidecar`s (`lib.rs:1005` diagnostics, `lib.rs:1118` review probe,
`preflight.rs:418`, tests). `spawn_idle_reaper` no-ops when
`idle_timeout.is_zero()` or `min_idle_workers >= max_workers` — every call
site except `start_pipeline` gets zero behavior change by construction.

**Ownership: `Weak<Sidecar>`.** A strong ref would create an Arc cycle with
`Drop for Sidecar` (`sidecar.rs:849-853`); `Weak` self-heals if anyone drops
a reaper-bearing `Sidecar` without calling shutdown (worst case: teardown
delayed by one poll tick, never leaked).

### Struct/field diffs

```rust
// sidecar.rs — PoolState.idle changes element type so the reaper can age entries.
struct IdleProc {
    proc: Proc,
    /// Set when pushed onto `idle` (Checkout::drop). Never read while checked out.
    since: std::time::Instant,
}

#[derive(Default)]
struct PoolState {
    idle: Vec<IdleProc>,   // was Vec<Proc>
    live: usize,
}

pub struct Sidecar {
    // ...existing fields unchanged...
    /// Reaping never drops `live` below this. Default 1 — always keep one
    /// warm so the next request after a lull skips the ~1s cold spawn
    /// (sidecar/BUILD.md).
    min_idle_workers: usize,
    /// `Duration::ZERO` disables reaping (the default — every call site
    /// except the pipeline's pool opts in explicitly).
    idle_timeout: Duration,
}
```

Call-site diffs inside `sidecar.rs`:
- `checkout()` (`sidecar.rs:393`): `state.idle.pop()` → destructure `IdleProc { proc, .. }`.
- `Checkout::drop` (`sidecar.rs:467`): push `IdleProc { proc, since: Instant::now() }`.
- `begin_shutdown` (`sidecar.rs:499-504`): `mem::take` — type changes, logic unaffected.
- `pool_tests` at `sidecar.rs:1181` (`idle.len()`) — unaffected.

New builder + constructor defaults:

```rust
impl Sidecar {
    pub fn with_idle_reap(mut self, min_idle: usize, idle_timeout: Duration) -> Self {
        self.min_idle_workers = min_idle.max(1);
        self.idle_timeout = idle_timeout;
        self
    }

    /// No-op unless `.with_idle_reap(...)` set a nonzero timeout with room
    /// under `max_workers`. Takes `Arc<Self>` because the thread must not
    /// hold a strong ref to itself — every long-lived caller already wraps
    /// in `Arc` before this is called.
    pub fn spawn_idle_reaper(self: &Arc<Self>) {
        if self.idle_timeout.is_zero() || self.min_idle_workers >= self.max_workers {
            return;
        }
        let weak = Arc::downgrade(self);
        if let Err(e) = std::thread::Builder::new()
            .name("convertd-reaper".into())
            .spawn(move || Sidecar::reap_idle_loop(weak))
        {
            log::warn!("could not start convertd idle reaper: {e}");
        }
    }
}
```

`Sidecar::with_timeout` gains default inits: `min_idle_workers: 1,
idle_timeout: Duration::ZERO` — byte-identical behavior for existing callers.

### Reaper pseudocode, exact lock scopes

```rust
fn reap_idle_loop(weak: std::sync::Weak<Sidecar>) {
    loop {
        let Some(sidecar) = weak.upgrade() else { return };   // Sidecar fully gone
        if sidecar.shutting_down.load(Ordering::Acquire) { return; }

        let reaped = sidecar.reap_idle_once();
        if reaped > 0 {
            log::info!("convertd idle reaper: retired {reaped} worker(s) idle past {:?}", sidecar.idle_timeout);
        }

        // Reuse the SAME condvar `checkout`/`Checkout::drop`/`begin_shutdown`
        // already signal on, so `begin_shutdown`'s notify_all wakes this
        // thread immediately instead of after a full poll tick.
        let guard = sidecar.lock_pool();
        let (_guard, _) = sidecar.available
            .wait_timeout(guard, reap_poll_interval(sidecar.idle_timeout))
            .unwrap_or_else(|e| e.into_inner());
        // `sidecar` (strong Arc) drops HERE, at loop-back — never held across
        // more than one lock+wait cycle.
    }
}

/// Poll finer than idle_timeout so real 300s timeouts land within ~10% slop,
/// floors at 50ms so a test-scale timeout doesn't busy-loop the mutex.
fn reap_poll_interval(idle_timeout: Duration) -> Duration {
    (idle_timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(30))
}

/// One pass. Lock scope: acquired once, released BEFORE dropping any expired
/// worker — identical discipline to `Checkout::drop` (sidecar.rs:480-482):
/// "Reaping takes the child-registry lock. Do it outside the pool lock so
/// shutdown and a simultaneous check-in cannot deadlock each other."
fn reap_idle_once(&self) -> usize {
    let mut state = self.lock_pool();                       // LOCK pool
    if self.shutting_down.load(Ordering::Acquire) { return 0; }

    let now = std::time::Instant::now();
    state.idle.sort_unstable_by_key(|w| w.since);            // oldest-idle-first
    let mut expired = Vec::new();
    while state.live > self.min_idle_workers {
        match state.idle.first() {
            Some(w) if now.duration_since(w.since) >= self.idle_timeout => {
                expired.push(state.idle.remove(0));
                state.live -= 1;
            }
            _ => break,
        }
    }
    drop(state);                                              // UNLOCK pool

    let count = expired.len();
    drop(expired);          // TrackedChild::drop: hard-kills, waits, unregisters
                            // (children-registry lock never nested inside pool lock)
    if count > 0 { self.available.notify_all(); }
    count
}
```

**Why no invariant break:** the free-list invariant (`sidecar.rs:383-386`,
"whichever worker comes back must satisfy the waiter") only matters when a
caller is blocked in checkout's wait — which only happens when
`idle.is_empty()`. The reaper only removes from a *non-empty* idle list, so a
blocked waiter and a reap-eligible list are mutually exclusive under the one
pool mutex.

**Race with `begin_shutdown`:** both take `lock_pool()`; `begin_shutdown`
sets `shutting_down` before locking, so whichever acquires second either
short-circuits (reaper) or finds idle drained. Each `IdleProc` is moved out
exactly once — no double-free.

### Config knobs

`config.rs`, next to `convert_workers`:

```rust
/// Idle convertd workers beyond this floor are eligible for reaping.
/// Default 1: always keep one warm.
pub convert_min_idle_workers: usize,
/// Seconds an idle convertd worker may sit before the reaper retires it.
/// 0 disables reaping (pre-feature behavior).
pub convert_idle_reap_secs: u64,
```

Defaults: `1` / `300`. Validation: `convert_min_idle_workers` 1..=8;
`convert_idle_reap_secs` ≤ 3600 (0 disables).

Wiring in `start_pipeline` (`lib.rs:1201-1212`): add
`.with_idle_reap(cfg.convert_min_idle_workers, Duration::from_secs(cfg.convert_idle_reap_secs))`
to the builder chain, then `sidecar.spawn_idle_reaper();` after the `Arc`.
Also surface both fields in `redacted_config` (`lib.rs:1027-1047`).

---

## Feature 2 — RAM Ceiling Correction

### The new constant

```rust
/// RSS per convertd worker once its heaviest lazily-loaded component
/// (RapidOCR) and lingua are both live. Measured 450-530 MB in production;
/// 550 sits above that band deliberately. Replaces the ~195 MB figure this
/// ceiling used before OCR+lingua were measured together — MarkItDown alone
/// is close to the old number, but `convertd.py`'s loaders (`_get`,
/// convertd.py:95-140) are memoized per-process with no unload path, and any
/// worker that ever services an `ocr` op or a `langid` op (run on
/// effectively every document) keeps that component loaded for the rest of
/// its life. A long-running pool converges toward the worse number.
const CONVERTD_WORKER_RSS_MB: u64 = 550;
```

### Updated curve

```rust
fn convert_workers_ram_ceiling(gib: Option<u64>) -> usize {
    match gib {
        // 8 GB class: ~1.2 GB left after OS/app/SLM@1. One worker (550 MB)
        // leaves real slack; two (1.1 GB) leaves under 150 MB — no margin.
        // This tier drops from 2 to 1.
        Some(g) if g <= 9 => 1,
        // 16 GB class: ~8.3 GB left after OS/app/SLM@2. Four workers
        // (2.2 GB) unchanged — wide margin at the corrected figure.
        Some(g) if g <= 17 => 4,
        // >16 GB: CPU-derived by_cpu (capped 6) binds first, not RAM.
        Some(_) => 6,
        // Match the smallest tier: don't gamble on the smaller machine's behalf.
        None => 1,
    }
}
```

### Before/after worker counts per RAM tier

| RAM | slm_parallel | SLM budget | Headroom after OS+app | Old ceiling (195 MB) | New ceiling (550 MB) |
|---|---|---|---|---|---|
| 8 GB | 1 | ~3.4 GB | ~1.2 GB | 2 | **1** |
| 12 GB | 2 | ~4.3 GB | ~4.3 GB | 4 | 4 |
| 16 GB | 2 | ~4.3 GB | ~8.3 GB | 4 | 4 |
| 32 GB | 4 | ~6.1 GB | ~20.5 GB | 6 | 6 |
| unknown | 2 | conservative | — | 2 | **1** |

**Headline: the 8 GB tier is the only one that changes, and it was unsafe** —
2 workers already consumed ~92% of that tier's margin before any document
work: exactly the thrash mode SIZING.md warns about. At the corrected 8 GB
ceiling of 1, `min_idle(1) >= max_workers(1)` → the reaper is naturally inert
there; it earns its keep on 12 GB+ machines.

### Stale-comment cleanup (same "~195 MB" figure)
`sidecar.rs:11`, `sidecar.rs:364-366`, `config.rs:60-61`, `config.rs:251-252`,
`config.rs:995-997`; footnote `docs/SIZING.md:61` (the 204 MB row is a
MarkItDown-only snapshot, not the OCR+lingua worst case). Also flag:
`src/main.ts` Settings UI if the two new knobs should be surfaced.

Tests to update: `convert_workers_are_capped_by_installed_ram`
(config.rs:999-1011) → `[(4,1),(8,1),(9,1),(12,4),(17,4),(32,6)]`, `None → 1`;
`an_eight_gib_machine_clamps_every_process_pool` (config.rs:1036-1045) →
expects 1.

---

## Test plan

Existing fakes: `pool_tests` (sidecar.rs:1129-1445) uses `sort`/`cat` as a
spawn-and-block fake via `stdin_reader()` (1140-1145); a unix-only module
(1507-1542) uses `fake-sidecar.sh` for real JSON echo.

New unit tests (pool_tests, same fake):
1. `idle_reaper_retires_workers_beyond_min_after_timeout` — pool(3),
   `with_idle_reap(1, 40ms)`, check out + drop 3, wait ~200 ms, assert
   `live == 1` and one tracked child.
2. `idle_reaper_never_drops_below_min_idle_workers` — min_idle 2, settles at 2.
3. `idle_reaper_leaves_checked_out_workers_alone` — 2 of 3 held; they survive.
4. `spawn_idle_reaper_is_a_no_op_by_default` — no opt-in, `live` unchanged
   past any window (protects the six other call sites by construction).
5. `idle_reaper_exits_promptly_on_shutdown` — long timeout, `begin_shutdown`,
   strong-count released within a generous bound.

Config tests: update the two above; boundary tests for the two new knobs per
the `rejects_zero_and_unbounded_resource_or_retry_values` pattern
(config.rs:794-843).

Not proposing a real-convertd E2E reaper test (300 s+ real wait, needs
BACKLOG_E2E env) — the fake-exe tests fully exercise the state machine.

---

## Risks

- Weak-upgrade race: ≤ one poll tick (≤30 s) of lingering thread after the
  last Arc drops — cosmetic.
- Thrash amplification if `idle_timeout` is set far below real
  inter-document gaps (repeated ~1 s respawns). Default 300 s; validated
  range bounds damage.
- 8 GB ceiling drop to 1 removes conversion parallelism on that tier —
  SIZING.md's own data says naming, not conversion, sets wall clock there;
  right tradeoff, but a real behavior change to call out.
- `clamp_resources_to_machine` is one-directional and already handles
  clamping a persisted `convert_workers: 2` down to 1 on 8 GB with a log.
- `IdleProc` wrapper blast radius: exactly two call sites + one read-only
  test `.len()` — private to the module.

---

## Optional stretch: OCR-pinned sub-pool — verdict: DEFER

Sketch: split `PoolState` into general/ocr partitions sharing one
mutex/condvar; `call()` classifies op → partition; `Checkout` carries its
partition. Rejected as a first move because: idle-reap already captures the
dominant (idle-time) cost; under sustained mixed load the free-list has no
capability memory, so workers converge to the loaded state anyway; it cannot
function on the 8 GB tier (1 worker); and it doubles pool bookkeeping while
adding an op-classifier that will drift against `convertd.py`'s OPS dict.
Revisit only on evidence of sustained active-batch memory pressure on
12-16 GB machines at `convert_workers >= 3-4`.
