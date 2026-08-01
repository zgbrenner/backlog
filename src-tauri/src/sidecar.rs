//! Client for the `convertd` Python sidecar: a pool of warm processes, newline-
//! delimited JSON over stdin/stdout, no terminal window. Ops:
//!   pdf_probe | convert | ocr | langid | classify | salience |
//!   rank_paragraphs | extract_entities | ettin_spans | ping
//! Respawn-on-death is handled here; the pipeline replays the job from the
//! ledger on RUNTIME_FAIL.
//!
//! A pool rather than one process because convertd's main loop is
//! `while True: readline()` — strictly one request at a time — so a single
//! warm process made every conversion in the app queue behind every other one
//! however large `Config::convert_workers` was. Each worker is its own ~195 MB
//! Python process, which is why that setting is capped against installed RAM.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStderr, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Longest single stdout line convertd is allowed to send. The protocol is
/// one JSON object per line and the largest legitimate one is a converted
/// document's markdown, which `convert` already truncates to head/tail pages;
/// anything beyond this is a runaway, and `BufRead::read_line` would grow the
/// buffer to hold it until this process died.
const MAX_STDOUT_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Stderr is prose, not payload — a traceback line far past this is noise.
const MAX_STDERR_LINE_BYTES: usize = 16 * 1024;

/// Stderr lines logged per spawned child before the rest are suppressed.
///
/// llama-server narrates every request, and a several-thousand-file backfill
/// would push the startup diagnostics — the whole reason stderr is captured —
/// out of the rotating log within minutes. Startup output arrives first, so a
/// per-process cap keeps exactly the part that explains a failure.
const MAX_LOGGED_STDERR_LINES: usize = 2000;

#[cfg(not(windows))]
fn terminate_pid(pid: u32) -> bool {
    const SIGKILL: i32 = 9;
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    // SAFETY: the PID remains registered and therefore unreaped while the
    // registry lock is held by the caller; it cannot have been recycled.
    unsafe { kill(pid as i32, SIGKILL) == 0 }
}

#[cfg(windows)]
fn terminate_pid(pid: u32) -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn TerminateProcess(process: isize, exit_code: u32) -> i32;
        fn WaitForSingleObject(object: isize, milliseconds: u32) -> u32;
        fn CloseHandle(object: isize) -> i32;
    }
    const PROCESS_TERMINATE: u32 = 0x0001;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 258;
    // SAFETY: `pid` remains registered and unreaped while the caller holds the
    // registry lock. The returned handle is checked before use and closed.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        return false;
    }
    let initial = unsafe { WaitForSingleObject(handle, 0) };
    let stopped = if initial == WAIT_OBJECT_0 {
        // It exited before shutdown acquired the handle but has not yet been
        // reaped by its checkout. That is already a successful outcome.
        true
    } else if initial == WAIT_TIMEOUT {
        (unsafe { TerminateProcess(handle, 1) }) != 0
            && unsafe { WaitForSingleObject(handle, 5_000) } == WAIT_OBJECT_0
    } else {
        false
    };
    let _ = unsafe { CloseHandle(handle) };
    stopped
}

/// Outcome of one [`read_line_bounded`] call.
pub enum LineRead {
    Line(String),
    /// A line that ran past the ceiling and was discarded, with the number of
    /// bytes dropped.
    Overlong(u64),
    Eof,
    Err(std::io::Error),
}

/// `BufRead::read_line` with a hard ceiling.
///
/// A child that writes megabytes without ever emitting a newline (a hung
/// generator, a binary blob on the wrong stream) would otherwise be able to
/// grow this process's memory without limit. Over-long lines are drained and
/// dropped rather than buffered, so the reader stays in sync with the stream
/// and the next well-formed line is still delivered.
pub fn read_line_bounded(reader: &mut impl BufRead, max_bytes: usize) -> LineRead {
    let mut buf: Vec<u8> = Vec::new();
    let mut discarded: u64 = 0;
    loop {
        let (complete, consume) = {
            let available = match reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return LineRead::Err(e),
            };
            if available.is_empty() {
                // EOF. A trailing fragment with no newline is still worth
                // returning; a fragment we already gave up on is not.
                return match (discarded, buf.is_empty()) {
                    (0, true) => LineRead::Eof,
                    (0, false) => LineRead::Line(String::from_utf8_lossy(&buf).into_owned()),
                    _ => LineRead::Overlong(discarded),
                };
            }
            match available.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    if discarded > 0 {
                        discarded += index as u64;
                    } else {
                        buf.extend_from_slice(&available[..index]);
                    }
                    (true, index + 1)
                }
                None => {
                    let len = available.len();
                    if discarded > 0 {
                        discarded += len as u64;
                    } else if buf.len() + len > max_bytes {
                        discarded = (buf.len() + len) as u64;
                        buf = Vec::new();
                    } else {
                        buf.extend_from_slice(available);
                    }
                    (false, len)
                }
            }
        };
        reader.consume(consume);
        if complete {
            return if discarded > 0 {
                LineRead::Overlong(discarded)
            } else {
                LineRead::Line(String::from_utf8_lossy(&buf).into_owned())
            };
        }
    }
}

/// Drain a child process's stderr into the app log on a dedicated thread.
///
/// Both of BackLog's children used to get `Stdio::null()`, which threw away
/// the one signal that explains a wedged run: convertd's tracebacks, and
/// llama-server's startup diagnostics (a port conflict otherwise surfaces
/// only as a 60-second timeout followed by SLM_FAIL flags with no cause
/// recorded anywhere). The shipped build is windowed and has no console, so
/// `logging.rs`'s rotating file is where this has to land.
pub fn log_child_stderr(
    label: &'static str,
    stderr: ChildStderr,
    max_lines: usize,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("{label}-stderr"))
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut logged = 0usize;
            loop {
                match read_line_bounded(&mut reader, MAX_STDERR_LINE_BYTES) {
                    LineRead::Line(line) => {
                        let line = line.trim_end();
                        if line.is_empty() {
                            continue;
                        }
                        logged += 1;
                        match logged.cmp(&max_lines) {
                            std::cmp::Ordering::Less => log::info!("{label}: {line}"),
                            std::cmp::Ordering::Equal => log::info!(
                                "{label}: {line} (further output from this process suppressed)"
                            ),
                            std::cmp::Ordering::Greater => {}
                        }
                    }
                    LineRead::Overlong(bytes) => {
                        log::warn!("{label}: discarded a {bytes}-byte stderr line with no newline")
                    }
                    LineRead::Eof => break,
                    LineRead::Err(e) => {
                        log::warn!("{label}: stderr could not be read: {e}");
                        break;
                    }
                }
            }
        })
}

#[derive(Debug, Serialize)]
struct Request<'a> {
    id: u64,
    op: &'a str,
    #[serde(flatten)]
    args: Value,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Value,
}

pub struct Sidecar {
    exe: std::path::PathBuf,
    /// Injected into the spawned process's `BACKLOG_MODELS_DIR` (see
    /// `with_models_dir`). `None` leaves convertd.py's own default in effect
    /// (dev layout: `../models` beside the sidecar executable) — every
    /// production call site sets this via `model_download::resolve_models_dir`
    /// so an installed app's `models.lock.json` and any optional local model
    /// snapshots (the slim sidecar profile ships none by default -- see
    /// `sidecar/convertd.py`'s `_gliclass`/`_granite` loaders), which live
    /// under app-data rather than beside the exe, are actually found.
    models_dir: Option<std::path::PathBuf>,
    /// Idle worker processes, plus how many exist in total.
    ///
    /// This used to be a single `Mutex<Option<Proc>>`, which made every sidecar
    /// op in the app — probe, convert, OCR, langid — queue behind every other
    /// one, because `call` holds the lock across the blocking `recv_timeout`.
    /// `convert_workers` sized a semaphore in `pipeline.rs` and therefore bought
    /// queue depth and no parallelism whatsoever: measured at ~34 s/file with
    /// fifteen cores idle. convertd's main loop is `while True: readline()`,
    /// strictly one request at a time per process, so the fix has to be more
    /// processes rather than more requests down one pipe.
    ///
    /// A checked-out worker is *absent* from `idle` and owned by the caller for
    /// the round trip, which is what makes a failed worker easy to retire: the
    /// caller simply retires it, the tracked child is killed, and `live` drops
    /// so the next checkout spawns a replacement.
    pool: Mutex<PoolState>,
    /// Signalled whenever a worker is returned or retired. A free-list with a
    /// condvar rather than one mutex per slot: with per-slot mutexes a caller
    /// that found every slot busy had to pick one and block on it, and could
    /// then sit behind a long OCR while a different worker went idle beside it.
    available: Condvar,
    max_workers: usize,
    counter: std::sync::atomic::AtomicU64,
    /// Permanent latch set before app exit starts tearing down process-owned
    /// state. Tauri exits with `process::exit`, so destructors are not a
    /// lifecycle guarantee.
    shutting_down: AtomicBool,
    /// Every spawned child remains registered until it has been reaped. The
    /// registry closes both spawn-vs-shutdown and PID-reuse races.
    children: Arc<Mutex<HashSet<u32>>>,
    pub timeout: Duration,
}

#[derive(Default)]
struct PoolState {
    /// Spawned and ready. Popped on checkout, pushed back on success.
    idle: Vec<Proc>,
    /// Spawned and not yet retired, whether idle or checked out. Bounded by
    /// `max_workers`; counted separately from `idle.len()` because a checked-out
    /// worker is in neither collection.
    live: usize,
}

struct TrackedChild {
    child: Child,
    pid: u32,
    children: Arc<Mutex<HashSet<u32>>>,
}

impl TrackedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        // Reaping and unregistering are one critical section. Otherwise
        // shutdown could snapshot this PID after `try_wait` reaped it and
        // accidentally signal a new process that reused the number.
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let status = self.child.try_wait()?;
        if status.is_some() {
            children.remove(&self.pid);
        }
        Ok(status)
    }
}

impl Drop for TrackedChild {
    fn drop(&mut self) {
        // Keep the PID registered until the child is reaped. `begin_shutdown`
        // holds the same lock while signalling its snapshot, so the OS cannot
        // recycle a tracked PID between the snapshot and termination.
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self.child.kill();
        let _ = self.child.wait();
        children.remove(&self.pid);
    }
}

struct Proc {
    child: TrackedChild,
    stdin: ChildStdin,
    rx: Receiver<std::io::Result<String>>,
    _reader: JoinHandle<()>,
    _stderr: JoinHandle<()>,
}

impl Sidecar {
    /// Default-timeout constructor. Not currently called internally (the app
    /// always threads `Config::sidecar_timeout_secs` through `with_timeout`
    /// below) but kept as stable, obvious-default API surface for tests and
    /// other callers.
    #[allow(dead_code)]
    pub fn new(exe: std::path::PathBuf) -> Self {
        Self::with_timeout(exe, Duration::from_secs(120))
    }

    pub fn with_timeout(exe: std::path::PathBuf, timeout: Duration) -> Self {
        Self {
            exe,
            models_dir: None,
            pool: Mutex::new(PoolState::default()),
            available: Condvar::new(),
            // One worker unless a caller opts in. The three short-lived
            // diagnostic probes (`get_diagnostics`, `preflight`, the review-only
            // pipeline) want exactly one, and so does every test; only the
            // long-lived pipeline sidecar pools.
            max_workers: 1,
            counter: std::sync::atomic::AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
            children: Arc::new(Mutex::new(HashSet::new())),
            timeout,
        }
    }

    /// Run up to `workers` convertd processes, converting that many documents at
    /// once.
    ///
    /// Each worker is a separate Python process at roughly 195 MB resident once
    /// MarkItDown and RapidOCR have loaded, so this is a memory decision as much
    /// as a throughput one — `Config::convert_workers` is capped against
    /// installed RAM for that reason. Builder-style, and clamped to at least one
    /// so a nonsense value cannot produce a `Sidecar` that can never answer.
    pub fn with_workers(mut self, workers: usize) -> Self {
        self.max_workers = workers.max(1);
        self
    }

    fn lock_pool(&self) -> std::sync::MutexGuard<'_, PoolState> {
        // Poison is recovered rather than propagated: one panicking caller must
        // not take the whole pool down with it. `Checkout::drop` runs during that
        // caller's unwind and leaves the pool structurally sound either way.
        self.pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a worker out of the pool, spawning one if there is room.
    ///
    /// Waits on the condvar when `max_workers` are already out, so whichever
    /// worker is returned next satisfies whichever caller is waiting — rather
    /// than a caller pre-committing to one worker and then sitting behind a long
    /// OCR while a different one goes idle beside it.
    fn checkout(&self) -> anyhow::Result<Checkout<'_>> {
        let mut state = self.lock_pool();
        loop {
            if self.shutting_down.load(Ordering::Acquire) {
                anyhow::bail!("document processing is shutting down");
            }
            if let Some(proc) = state.idle.pop() {
                return Ok(Checkout {
                    sidecar: self,
                    proc: Some(proc),
                });
            }
            if state.live < self.max_workers {
                // Reserve the slot before releasing the lock so two callers
                // cannot both decide there is room for the last worker.
                state.live += 1;
                drop(state);
                return match self.spawn() {
                    Ok(proc) if !self.shutting_down.load(Ordering::Acquire) => Ok(Checkout {
                        sidecar: self,
                        proc: Some(proc),
                    }),
                    Ok(proc) => {
                        drop(proc);
                        self.lock_pool().live -= 1;
                        self.available.notify_all();
                        anyhow::bail!("document processing is shutting down");
                    }
                    Err(e) => {
                        // Give the reservation back, or the pool shrinks by one
                        // every time a spawn fails and eventually serves nobody.
                        self.lock_pool().live -= 1;
                        self.available.notify_one();
                        Err(e)
                    }
                };
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// A worker on loan from the pool, returned on drop.
///
/// RAII rather than an explicit check-in because `call` can leave by several
/// paths — success, four kinds of failure, and a panic in `serde` or a caller's
/// own code. Without this, any path that missed the check-in would silently cost
/// the pool one worker for the life of the process, and a long backfill would
/// grind down to nothing with no error to explain it.
struct Checkout<'a> {
    sidecar: &'a Sidecar,
    /// `None` once `retire` has been called: the worker is not coming back and
    /// its slot should be freed for a replacement.
    proc: Option<Proc>,
}

impl Checkout<'_> {
    fn proc(&mut self) -> &mut Proc {
        // Only `retire` clears this, and it consumes the borrow, so a retired
        // checkout is never used again.
        self.proc.as_mut().expect("checked-out worker")
    }

    /// Discard this worker instead of returning it: wedged, dead, or out of sync
    /// with its own response stream. Dropping its tracked child kills it; the
    /// next checkout spawns a clean replacement.
    fn retire(&mut self) {
        self.proc = None;
    }
}

impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        let retire = {
            let mut state = self.sidecar.lock_pool();
            match self.proc.take() {
                Some(proc) if !self.sidecar.shutting_down.load(Ordering::Acquire) => {
                    state.idle.push(proc);
                    None
                }
                Some(proc) => {
                    state.live -= 1;
                    Some(proc)
                }
                None => {
                    state.live -= 1;
                    None
                }
            }
        };
        // Reaping takes the child-registry lock. Do it outside the pool lock so
        // shutdown and a simultaneous check-in cannot deadlock each other.
        drop(retire);
        self.sidecar.available.notify_all();
    }
}

impl Sidecar {
    /// Permanently stop admitting work and terminate every spawned converter.
    ///
    /// This is explicit because Tauri's exit path calls `process::exit`, which
    /// skips Rust destructors. It is safe to call more than once.
    pub fn begin_shutdown(&self) -> usize {
        self.shutting_down.store(true, Ordering::Release);
        self.available.notify_all();

        // Idle workers can be reaped synchronously because nobody owns them.
        // Checked-out workers retain their slot until their Checkout drops, but
        // are signalled below so blocked pipe reads wake promptly.
        let idle = {
            let mut state = self.lock_pool();
            let idle = std::mem::take(&mut state.idle);
            state.live = state.live.saturating_sub(idle.len());
            idle
        };
        drop(idle);

        let children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        let mut failures = 0usize;
        for &pid in children.iter() {
            if !terminate_pid(pid) {
                failures += 1;
                log::error!("could not terminate convertd process {pid} during shutdown");
            }
        }
        failures
    }

    #[cfg(test)]
    fn tracked_children(&self) -> HashSet<u32> {
        self.children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Sets the models directory injected into every spawned process as
    /// `BACKLOG_MODELS_DIR`. Builder-style so existing call sites (and the
    /// unix-only tests below, which never load a real model) are unaffected
    /// unless they opt in.
    pub fn with_models_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.models_dir = Some(dir);
        self
    }

    fn spawn(&self) -> anyhow::Result<Proc> {
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("document processing is shutting down");
        }
        let mut cmd = Command::new(&self.exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Piped, not null: see `log_child_stderr`.
            .stderr(Stdio::piped());
        if let Some(dir) = &self.models_dir {
            cmd.env("BACKLOG_MODELS_DIR", dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut raw_child = cmd.spawn()?;
        let pid = raw_child.id();
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        if self.shutting_down.load(Ordering::Acquire) {
            drop(children);
            let _ = raw_child.kill();
            let _ = raw_child.wait();
            anyhow::bail!("document processing is shutting down");
        }
        children.insert(pid);
        drop(children);
        let mut child = TrackedChild {
            child: raw_child,
            pid,
            children: self.children.clone(),
        };
        let stdin = child
            .child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stdin"))?;
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stdout"))?;
        let stderr = child
            .child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("no sidecar stderr"))?;
        let stderr_thread = log_child_stderr("convertd", stderr, MAX_LOGGED_STDERR_LINES)?;

        // Drain stdout on a dedicated thread so `call` can enforce a per-request
        // deadline with recv_timeout. A blocking read on the pipe is otherwise
        // uncancellable, letting one pathological document wedge the sidecar
        // (and every job queued behind it) indefinitely.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("convertd-reader".into())
            .spawn(move || {
                let mut r = BufReader::new(stdout);
                loop {
                    match read_line_bounded(&mut r, MAX_STDOUT_LINE_BYTES) {
                        LineRead::Line(line) => {
                            if tx.send(Ok(line)).is_err() {
                                break; // receiver gone; nothing to do
                            }
                        }
                        // Dropped rather than forwarded: `call` would fail to
                        // parse it anyway, and the point is that it never got
                        // buffered in the first place.
                        LineRead::Overlong(bytes) => {
                            log::warn!(
                                "convertd: discarded a {bytes}-byte response with no newline"
                            )
                        }
                        LineRead::Eof => break, // the child closed stdout
                        LineRead::Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    }
                }
            })?;
        let proc = Proc {
            child,
            stdin,
            rx,
            _reader: reader,
            _stderr: stderr_thread,
        };
        if self.shutting_down.load(Ordering::Acquire) {
            drop(proc);
            anyhow::bail!("document processing is shutting down");
        }
        Ok(proc)
    }

    pub fn call(&self, op: &str, args: Value) -> anyhow::Result<Value> {
        let id = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Ids stay globally unique across the pool. Nothing requires that — a
        // worker only ever sees its own requests, on its own private channel —
        // but it keeps a stray line traceable to one call rather than to one
        // call per worker.
        let mut checkout = self.checkout()?;
        if self.shutting_down.load(Ordering::Acquire) {
            anyhow::bail!("document processing is shutting down");
        }

        // A worker can have died on its own between checkouts (crash, killed by
        // the OS under memory pressure). Retire it and take a fresh one rather
        // than writing a request into a closed pipe.
        if matches!(checkout.proc().child.try_wait(), Ok(Some(_)) | Err(_)) {
            checkout.retire();
            drop(checkout);
            checkout = self.checkout()?;
        }
        let proc = checkout.proc();

        let req = Request { id, op, args };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        if let Err(error) = proc.stdin.write_all(line.as_bytes()) {
            if self.shutting_down.load(Ordering::Acquire) {
                anyhow::bail!("document processing is shutting down");
            }
            return Err(error.into());
        }
        if let Err(error) = proc.stdin.flush() {
            if self.shutting_down.load(Ordering::Acquire) {
                anyhow::bail!("document processing is shutting down");
            }
            return Err(error.into());
        }

        enum Wake {
            Resp(Response),
            Timeout,
            ReadErr(String),
            Closed,
        }

        let deadline = std::time::Instant::now() + self.timeout;
        let wake = loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break Wake::Timeout;
            }
            match proc.rx.recv_timeout(remaining) {
                Ok(Ok(buf)) => match serde_json::from_str::<Response>(buf.trim()) {
                    Ok(resp) if resp.id == id => break Wake::Resp(resp),
                    // Stray stderr-ish noise on stdout, or a stale response from
                    // a replayed job — keep waiting for our id.
                    _ => continue,
                },
                Ok(Err(e)) => break Wake::ReadErr(e.to_string()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break Wake::Closed,
            }
        };

        match wake {
            Wake::Resp(resp) => {
                if !resp.ok {
                    anyhow::bail!("sidecar '{op}' failed: {}", resp.error.unwrap_or_default());
                }
                Ok(resp.data)
            }
            // Retire the wedged/broken worker instead of returning it to the
            // pool; `Checkout::drop` frees its slot and drops the tracked
            // child, so the next checkout spawns a clean replacement. A wedged
            // worker must never go back into circulation: its next response
            // would arrive against a stale id and be discarded, leaving it one
            // reply behind forever.
            Wake::Timeout => {
                checkout.retire();
                if self.shutting_down.load(Ordering::Acquire) {
                    anyhow::bail!("document processing is shutting down");
                }
                anyhow::bail!("sidecar '{op}' timed out after {:?}", self.timeout);
            }
            Wake::ReadErr(e) => {
                checkout.retire();
                if self.shutting_down.load(Ordering::Acquire) {
                    anyhow::bail!("document processing is shutting down");
                }
                anyhow::bail!("sidecar read error during '{op}': {e}");
            }
            Wake::Closed => {
                checkout.retire();
                if self.shutting_down.load(Ordering::Acquire) {
                    anyhow::bail!("document processing is shutting down");
                }
                anyhow::bail!("sidecar closed stream during '{op}'");
            }
        }
    }

    // ---- typed helpers -----------------------------------------------------

    /// Returns (median_chars_per_page, page_count).
    pub fn pdf_probe(&self, path: &str) -> anyhow::Result<(u64, u64)> {
        let v = self.call("pdf_probe", serde_json::json!({ "path": path }))?;
        Ok((
            v["median_chars_per_page"].as_u64().unwrap_or(0),
            v["pages"].as_u64().unwrap_or(0),
        ))
    }

    pub fn convert(
        &self,
        path: &str,
        head_pages: usize,
        tail_pages: usize,
    ) -> anyhow::Result<ConvertResult> {
        let v = self.call(
            "convert",
            serde_json::json!({ "path": path, "head_pages": head_pages, "tail_pages": tail_pages }),
        )?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn ocr(
        &self,
        path: &str,
        dpi: u32,
        head_pages: usize,
        tail_pages: usize,
    ) -> anyhow::Result<ConvertResult> {
        let v = self.call(
            "ocr",
            serde_json::json!({ "path": path, "dpi": dpi, "head_pages": head_pages, "tail_pages": tail_pages }),
        )?;
        Ok(serde_json::from_value(v)?)
    }

    pub fn langid(&self, text: &str) -> anyhow::Result<String> {
        let v = self.call("langid", serde_json::json!({ "text": text }))?;
        Ok(v["lang"].as_str().unwrap_or("en").to_string())
    }

    pub fn classify(&self, text: &str, labels: &[String]) -> anyhow::Result<(String, f64)> {
        let v = self.call(
            "classify",
            serde_json::json!({ "text": text, "labels": labels }),
        )?;
        Ok((
            v["label"].as_str().unwrap_or("correspondence").to_string(),
            v["score"].as_f64().unwrap_or(0.0),
        ))
    }

    pub fn rank_paragraphs(
        &self,
        paragraphs: &[SourceParagraph],
        probes: &[String],
        top_k: usize,
        min_score: f64,
        diversity: f64,
    ) -> anyhow::Result<SemanticRankResult> {
        let value = self.call(
            "rank_paragraphs",
            serde_json::json!({
                "paragraphs": paragraphs,
                "probes": probes,
                "top_k": top_k,
                "min_score": min_score,
                "diversity": diversity,
            }),
        )?;
        let result: SemanticRankResult = serde_json::from_value(value)?;
        validate_rank_result(&result, paragraphs)?;
        Ok(result)
    }

    pub fn extract_entities(
        &self,
        paragraphs: &[SourceParagraph],
        labels: &[EntityLabel],
        threshold: f64,
        max_per_label: usize,
    ) -> anyhow::Result<EntityExtractionResult> {
        let value = self.call(
            "extract_entities",
            serde_json::json!({
                "paragraphs": paragraphs,
                "labels": labels,
                "threshold": threshold,
                "max_per_label": max_per_label,
            }),
        )?;
        let result: EntityExtractionResult = serde_json::from_value(value)?;
        validate_entity_result(&result, paragraphs)?;
        Ok(result)
    }

    pub fn ettin_spans(&self, text: &str) -> anyhow::Result<Vec<EttinSpan>> {
        let v = self.call("ettin_spans", serde_json::json!({ "text": text }))?;
        Ok(serde_json::from_value(v["spans"].clone()).unwrap_or_default())
    }

    pub fn versions(&self) -> anyhow::Result<Value> {
        self.call("versions", serde_json::json!({}))
    }

    /// Cheap liveness probe used by `preflight::run` (via `spawn_blocking`)
    /// to verify the sidecar executable launches and answers before a batch
    /// starts.
    pub fn ping(&self) -> anyhow::Result<()> {
        self.call("ping", serde_json::json!({})).map(|_| ())
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.begin_shutdown();
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConvertResult {
    pub markdown: String,
    #[serde(default)]
    pub doc_meta_dates: Vec<String>, // ISO dates from doc properties
    #[serde(default)]
    pub ocr_used: bool,
    #[serde(default)]
    pub ocr_mean_conf: f64,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub letterhead_resets: u32, // multi-doc packet heuristic count
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EttinSpan {
    pub label: String, // DATE | PARTY | SUBJECT
    pub text: String,
    pub score: f64,
    #[serde(default)]
    pub iso: Option<String>, // normalized, DATE spans only
}

/// One unchanged source paragraph. All offsets in this semantic protocol are
/// Unicode scalar-value indices, matching Python's `str` indexing, rather
/// than UTF-8 byte offsets.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SourceParagraph {
    pub index: usize,
    pub text: String,
    pub start_char: usize,
    pub end_char: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct RankedParagraph {
    pub index: usize,
    pub text: String,
    pub start_char: usize,
    pub end_char: usize,
    pub score: f64,
    pub probe: String,
    pub rank: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct SemanticRankResult {
    pub available: bool,
    pub model: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub results: Vec<RankedParagraph>,
    #[serde(default)]
    pub source_chars: usize,
    #[serde(default)]
    pub selected_chars: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EntityLabel {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EntitySpan {
    pub label: String,
    pub text: String,
    pub score: f64,
    pub paragraph_index: usize,
    pub start_char: usize,
    pub end_char: usize,
    #[serde(default)]
    pub iso: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct EntityExtractionResult {
    pub available: bool,
    pub model: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub spans: Vec<EntitySpan>,
    #[serde(default)]
    pub label_cache_key: String,
    #[serde(default)]
    pub label_embeddings_reused: bool,
    #[serde(default)]
    pub candidates_considered: usize,
}

fn char_slice(text: &str, start: usize, end: usize) -> Option<String> {
    if end < start {
        return None;
    }
    let total = text.chars().count();
    if end > total {
        return None;
    }
    Some(text.chars().skip(start).take(end - start).collect())
}

fn validate_rank_result(
    result: &SemanticRankResult,
    paragraphs: &[SourceParagraph],
) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for ranked in &result.results {
        anyhow::ensure!(
            ranked.score.is_finite() && (0.0..=1.0).contains(&ranked.score),
            "semantic rank score is outside [0, 1]"
        );
        anyhow::ensure!(ranked.rank > 0, "semantic rank must be one-based");
        anyhow::ensure!(
            seen.insert(ranked.index),
            "semantic rank payload repeats paragraph {}",
            ranked.index
        );
        let source = paragraphs
            .iter()
            .find(|paragraph| paragraph.index == ranked.index)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "semantic rank references unknown paragraph {}",
                    ranked.index
                )
            })?;
        anyhow::ensure!(
            source.text == ranked.text
                && source.start_char == ranked.start_char
                && source.end_char == ranked.end_char,
            "semantic rank changed paragraph {} or its provenance",
            ranked.index
        );
    }
    let measured_selected: usize = result
        .results
        .iter()
        .map(|item| item.text.chars().count())
        .sum();
    anyhow::ensure!(
        result.selected_chars == measured_selected,
        "semantic rank selected_chars does not match the returned text"
    );
    Ok(())
}

fn validate_entity_result(
    result: &EntityExtractionResult,
    paragraphs: &[SourceParagraph],
) -> anyhow::Result<()> {
    for span in &result.spans {
        anyhow::ensure!(
            !span.label.trim().is_empty(),
            "semantic entity label is empty"
        );
        anyhow::ensure!(
            span.score.is_finite() && (0.0..=1.0).contains(&span.score),
            "semantic entity score is outside [0, 1]"
        );
        let source = paragraphs
            .iter()
            .find(|paragraph| paragraph.index == span.paragraph_index)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "semantic entity references unknown paragraph {}",
                    span.paragraph_index
                )
            })?;
        let exact = char_slice(&source.text, span.start_char, span.end_char).ok_or_else(|| {
            anyhow::anyhow!(
                "semantic entity offsets are outside paragraph {}",
                span.paragraph_index
            )
        })?;
        anyhow::ensure!(
            exact == span.text,
            "semantic entity changed source text in paragraph {}",
            span.paragraph_index
        );
    }
    Ok(())
}

#[cfg(test)]
mod semantic_payload_tests {
    use super::*;

    fn paragraphs() -> Vec<SourceParagraph> {
        vec![SourceParagraph {
            index: 7,
            text: "Acme LLC hired José Doe.".to_string(),
            start_char: 10,
            end_char: 35,
        }]
    }

    #[test]
    fn valid_rank_payload_preserves_exact_source() {
        let source = paragraphs();
        let result = SemanticRankResult {
            available: true,
            model: "fixture".into(),
            reason: None,
            results: vec![RankedParagraph {
                index: 7,
                text: source[0].text.clone(),
                start_char: 10,
                end_char: 35,
                score: 0.75,
                probe: "parties".into(),
                rank: 1,
            }],
            source_chars: source[0].text.chars().count(),
            selected_chars: source[0].text.chars().count(),
        };
        validate_rank_result(&result, &source).expect("valid payload");
    }

    #[test]
    fn rank_payload_rejects_unknown_or_non_finite_results() {
        let source = paragraphs();
        let mut result = SemanticRankResult {
            available: true,
            model: "fixture".into(),
            reason: None,
            results: vec![RankedParagraph {
                index: 99,
                text: "invented".into(),
                start_char: 0,
                end_char: 8,
                score: f64::NAN,
                probe: "date".into(),
                rank: 1,
            }],
            source_chars: 8,
            selected_chars: 8,
        };
        assert!(validate_rank_result(&result, &source).is_err());
        result.results[0].score = 0.5;
        assert!(validate_rank_result(&result, &source).is_err());
    }

    #[test]
    fn entity_offsets_are_unicode_character_offsets_and_must_slice_exactly() {
        let source = paragraphs();
        let good = EntityExtractionResult {
            available: true,
            model: "fixture".into(),
            reason: None,
            spans: vec![EntitySpan {
                label: "PERSON".into(),
                text: "José Doe".into(),
                score: 0.8,
                paragraph_index: 7,
                start_char: 15,
                end_char: 23,
                iso: None,
            }],
            label_cache_key: "abc".into(),
            label_embeddings_reused: false,
            candidates_considered: 1,
        };
        validate_entity_result(&good, &source).expect("unicode character offsets remain exact");
        let mut bad = good.clone();
        bad.spans[0].end_char = 200;
        assert!(validate_entity_result(&bad, &source).is_err());
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::Arc;

    /// A stand-in worker that spawns and then blocks reading stdin, which is all
    /// the pool's bookkeeping cares about. `checkout` spawns as part of
    /// reserving, so these tests need a real executable — but not a real
    /// convertd: nothing here sends a request or waits for a reply.
    ///
    /// `sort` on Windows and `cat` on unix both consume stdin until EOF and stay
    /// alive until killed, which is exactly the lifecycle a convertd worker has.
    fn stdin_reader() -> std::path::PathBuf {
        #[cfg(windows)]
        return std::path::PathBuf::from("sort");
        #[cfg(not(windows))]
        return std::path::PathBuf::from("cat");
    }

    fn pool(workers: usize) -> Arc<Sidecar> {
        Arc::new(
            Sidecar::with_timeout(stdin_reader(), Duration::from_millis(50)).with_workers(workers),
        )
    }

    #[test]
    fn shutdown_latch_rejects_new_checkout_without_spawning() {
        let sidecar = pool(1);

        assert_eq!(sidecar.begin_shutdown(), 0);

        let error = sidecar
            .ping()
            .expect_err("shutdown must reject new converter work");
        assert_eq!(error.to_string(), "document processing is shutting down");
        let spawn_error = sidecar
            .spawn()
            .err()
            .expect("shutdown must reject a direct spawn race");
        assert_eq!(
            spawn_error.to_string(),
            "document processing is shutting down"
        );
        assert_eq!(sidecar.lock_pool().live, 0);
        assert!(sidecar.tracked_children().is_empty());
    }

    #[test]
    fn begin_shutdown_drains_every_idle_worker() {
        let sidecar = pool(2);
        let a = sidecar.checkout().expect("spawned worker one");
        let b = sidecar.checkout().expect("spawned worker two");
        drop((a, b));
        assert_eq!(sidecar.lock_pool().idle.len(), 2);
        assert_eq!(sidecar.tracked_children().len(), 2);

        assert_eq!(sidecar.begin_shutdown(), 0);

        let state = sidecar.lock_pool();
        assert_eq!(state.live, 0);
        assert!(state.idle.is_empty());
        drop(state);
        assert!(sidecar.tracked_children().is_empty());
    }

    #[test]
    fn checked_out_worker_is_terminated_and_retires_after_shutdown() {
        let sidecar = pool(1);
        let mut held = sidecar.checkout().expect("spawned checked-out worker");

        assert_eq!(sidecar.begin_shutdown(), 0);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if held
                .proc()
                .child
                .try_wait()
                .expect("worker status remains observable")
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shutdown must terminate a checked-out worker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            sidecar.lock_pool().live,
            1,
            "the checkout still owns its pool slot until check-in"
        );

        drop(held);

        let state = sidecar.lock_pool();
        assert_eq!(state.live, 0);
        assert!(
            state.idle.is_empty(),
            "check-in after shutdown must retire instead of pooling"
        );
        drop(state);
        assert!(sidecar.tracked_children().is_empty());
    }

    /// The whole point of the change: N workers means N documents in flight.
    /// Before it, `call` held one mutex across the entire round trip, so a
    /// second caller waited for the first no matter how large `convert_workers`
    /// was.
    #[test]
    fn a_pool_admits_one_caller_per_worker_and_makes_the_rest_wait() {
        let sidecar = pool(3);
        let a = sidecar.checkout().expect("reserved slot 1");
        let b = sidecar.checkout().expect("reserved slot 2");
        let c = sidecar.checkout().expect("reserved slot 3");
        assert_eq!(sidecar.lock_pool().live, 3);

        // A fourth caller must wait rather than share a worker, because convertd
        // handles exactly one request at a time per process.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let sidecar = sidecar.clone();
            std::thread::spawn(move || {
                let _held = sidecar.checkout();
                let _ = tx.send(());
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "a fourth caller must not be admitted while three workers are out"
        );

        // Whichever worker comes back must satisfy the waiter. This is the
        // property a per-slot mutex could not provide: there, a waiter picked
        // one slot up front and could sit behind it while another went idle.
        drop(b);
        assert!(
            rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "returning any worker must release the waiting caller"
        );
        waiter.join().expect("waiter must not panic");
        drop((a, c));
    }

    /// A single worker is still strictly serialized, which is what every
    /// short-lived diagnostic probe and every other test relies on.
    #[test]
    fn the_default_pool_is_one_worker() {
        let sidecar = pool(1);
        let held = sidecar.checkout().expect("reserved the only slot");
        let (tx, rx) = std::sync::mpsc::channel();
        let sidecar2 = sidecar.clone();
        let waiter = std::thread::spawn(move || {
            let _held = sidecar2.checkout();
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "one worker must admit one caller at a time"
        );
        drop(held);
        assert!(rx.recv_timeout(Duration::from_secs(10)).is_ok());
        waiter.join().unwrap();

        // `with_workers(0)` must not produce a pool that can never answer.
        let clamped =
            Sidecar::with_timeout(stdin_reader(), Duration::from_millis(10)).with_workers(0);
        assert_eq!(clamped.max_workers, 1);
    }

    /// Retiring a worker must free its slot. If it did not, every timeout would
    /// permanently shrink the pool and a long backfill would grind to a halt
    /// with nothing in the log to explain it.
    #[test]
    fn retiring_a_worker_frees_its_slot_for_a_replacement() {
        let sidecar = pool(1);
        let mut held = sidecar.checkout().expect("reserved the only slot");
        assert_eq!(sidecar.lock_pool().live, 1);
        held.retire();
        drop(held);
        let state = sidecar.lock_pool();
        assert_eq!(state.live, 0, "a retired worker must not hold its slot");
        assert!(
            state.idle.is_empty(),
            "a retired worker must not go back into circulation"
        );
    }

    /// Times the same conversions through one worker and through several, using
    /// the real `convertd`.
    ///
    /// `#[ignore]` because it needs the built sidecar and real documents. It
    /// exists because the end-to-end batch cannot show this: with `slm_parallel`
    /// low, naming is the binding constraint, so pooling conversion leaves the
    /// wall clock unchanged and the win is invisible exactly where you would
    /// look for it. This measures the conversion stage on its own.
    ///
    /// ```powershell
    /// $env:BACKLOG_E2E_CONVERTD = "$env:LOCALAPPDATA\BackLog\convertd.exe"
    /// $env:BACKLOG_E2E_DOCS     = "C:\path\to\a\folder\of\documents"
    /// cargo test -p backlog --lib convert_throughput -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs the built convertd and a folder of real documents"]
    fn convert_throughput_scales_with_workers() {
        let exe = std::path::PathBuf::from(
            std::env::var("BACKLOG_E2E_CONVERTD").expect("BACKLOG_E2E_CONVERTD must be set"),
        );
        let docs = std::path::PathBuf::from(
            std::env::var("BACKLOG_E2E_DOCS").expect("BACKLOG_E2E_DOCS must be set"),
        );
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&docs)
            .expect("document folder must exist")
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        assert!(files.len() >= 4, "need at least four documents to compare");

        let run = |workers: usize| -> Duration {
            let sidecar = Arc::new(
                Sidecar::with_timeout(exe.clone(), Duration::from_secs(120)).with_workers(workers),
            );

            // Warm every worker before timing anything. `convertd` is a
            // PyInstaller one-file build: the first request to a fresh worker
            // pays for unpacking to %TEMP% and starting a Python interpreter,
            // seconds of it. Timing that would measure N cold starts against
            // one and conclude, wrongly, that pooling is slower — which is
            // exactly what the first version of this test reported. The app
            // holds one pool across a whole backfill, so startup is paid once
            // and amortised to nothing; steady state is the honest comparison.
            let warm: Vec<_> = (0..workers)
                .map(|_| {
                    let sidecar = sidecar.clone();
                    std::thread::spawn(move || {
                        let _ = sidecar.call("ping", serde_json::json!({}));
                    })
                })
                .collect();
            for h in warm {
                h.join().expect("warmup must not panic");
            }
            assert_eq!(
                sidecar.lock_pool().live,
                workers,
                "every worker should be spawned and idle before timing"
            );

            let started = std::time::Instant::now();
            let handles: Vec<_> = files
                .iter()
                .map(|path| {
                    let path = path.clone();
                    let sidecar = sidecar.clone();
                    std::thread::spawn(move || {
                        // Any real op that does actual work; `convert` is the one
                        // the pipeline spends its time in.
                        let _ = sidecar.call(
                            "convert",
                            serde_json::json!({
                                "path": path.to_string_lossy(),
                                "head_pages": 10,
                                "tail_pages": 3,
                            }),
                        );
                    })
                })
                .collect();
            for h in handles {
                h.join().expect("no conversion thread may panic");
            }
            started.elapsed()
        };

        let one = run(1);
        let many = run(4);
        let speedup = one.as_secs_f64() / many.as_secs_f64();
        eprintln!(
            "\n=== {} documents | 1 worker: {:.1}s | 4 workers: {:.1}s | speedup {:.2}x ===\n",
            files.len(),
            one.as_secs_f64(),
            many.as_secs_f64(),
            speedup
        );
        assert!(
            many < one,
            "four workers must beat one: {:.1}s vs {:.1}s",
            many.as_secs_f64(),
            one.as_secs_f64()
        );
    }

    /// A panic while holding a worker must not cost the pool a slot. `call` can
    /// leave by several paths and RAII is what makes all of them safe.
    #[test]
    fn a_panicking_caller_returns_its_worker() {
        let sidecar = pool(2);
        let sidecar2 = sidecar.clone();
        let panicked = std::thread::spawn(move || {
            let _held = sidecar2.checkout().expect("reserved a slot");
            panic!("caller explodes mid-round-trip");
        })
        .join();
        assert!(panicked.is_err(), "the thread was supposed to panic");

        // Poison recovered, slot returned, pool still fully usable.
        let a = sidecar
            .checkout()
            .expect("pool survives a panicking caller");
        let b = sidecar.checkout().expect("both slots still available");
        assert_eq!(sidecar.lock_pool().live, 2);
        drop((a, b));
    }
}

#[cfg(test)]
mod line_tests {
    use super::*;

    fn read_all(input: &[u8], max_bytes: usize) -> Vec<String> {
        let mut reader = BufReader::with_capacity(8, input);
        let mut out = Vec::new();
        loop {
            match read_line_bounded(&mut reader, max_bytes) {
                LineRead::Line(line) => out.push(line),
                LineRead::Overlong(bytes) => out.push(format!("<dropped {bytes}>")),
                LineRead::Eof => return out,
                LineRead::Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn splits_on_newlines_across_buffer_refills() {
        // An 8-byte BufReader capacity forces the multi-`fill_buf` path that
        // a real pipe hits on any response worth reading.
        assert_eq!(
            read_all(b"{\"id\":1}\n{\"id\":2,\"ok\":true}\n", 1024),
            vec![
                "{\"id\":1}".to_string(),
                "{\"id\":2,\"ok\":true}".to_string()
            ]
        );
    }

    #[test]
    fn returns_a_trailing_fragment_that_never_got_its_newline() {
        assert_eq!(
            read_all(b"a\nb", 1024),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// The point of the ceiling: a child that never emits a newline must not
    /// be able to grow this process's memory, and the reader must stay in
    /// sync so the *next* line still arrives.
    #[test]
    fn discards_an_overlong_line_and_keeps_reading() {
        let mut input = vec![b'x'; 4096];
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":9}\n");

        let lines = read_all(&input, 64);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("<dropped "), "{}", lines[0]);
        assert_eq!(lines[1], "{\"id\":9}");
    }

    #[test]
    fn an_unterminated_overlong_line_ends_the_stream_without_buffering_it() {
        let input = vec![b'x'; 100_000];
        assert_eq!(read_all(&input, 128), vec!["<dropped 100000>".to_string()]);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn write_executable(path: &std::path::Path, source: &str) {
        std::fs::write(path, source).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn timeout_kills_process_and_next_request_respawns() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-sidecar.sh");
        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do sleep 5; done\n",
        );

        let sidecar = Sidecar::with_timeout(executable.clone(), Duration::from_millis(75));
        let error = sidecar
            .call("ping", serde_json::json!({}))
            .expect_err("silent sidecar must time out");
        assert!(error.to_string().contains("timed out"));

        write_executable(
            &executable,
            "#!/bin/sh\nwhile IFS= read -r line; do printf '%s\\n' '{\"id\":2,\"ok\":true}'; done\n",
        );
        sidecar
            .ping()
            .expect("next request should spawn a clean process");
    }
}
