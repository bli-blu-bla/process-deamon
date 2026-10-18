//! Process supervision: starting/stopping children, tracking their state,
//! auto-restart on crash, crash log, failed hook and scheduled starts/restarts.

mod hook;
mod metrics;
mod state;

use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;
use chrono::{DateTime, Local, SecondsFormat};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{Config, ProcessConfig};
use crate::duration;
use crate::logging::LogWriter;
use crate::schedule::{Action as ScheduleAction, Schedule};
use hook::{CrashEvent, RestartOutcome};
use metrics::MetricsSampler;
pub use state::{ProcessInfo, State};

/// How long a run's output may keep draining after its main process exited.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Schedulers re-check the wall clock at least this often, so a clock step
/// (NTP, suspended host) doesn't push a fire time away.
const SCHEDULE_MAX_SLEEP: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("process not found")]
    NotFound,
    #[error("process already {}", .0.state)]
    AlreadyRunning(Box<ProcessInfo>),
    #[error("process already {}", .0.state)]
    AlreadyStopped(Box<ProcessInfo>),
    #[error("start: {0}")]
    Spawn(io::Error),
    /// A scheduled auto-restart lost a race against a manual start/stop or
    /// shutdown. Internal only, never surfaced to API clients.
    #[error("auto-restart superseded")]
    Superseded,
}

/// Owns all configured processes; the only thing that starts/stops them.
pub struct Manager {
    processes: Vec<Arc<Process>>, // process.toml order, for stable listings
    console: Arc<LogWriter>,
    crash_log: Option<LogWriter>,
    shutdown_timeout: Duration,
    metrics_interval: Duration,
    core_env: Vec<(String, String)>,
    failed_hook: String,
    failed_hook_timeout: Duration,
    hooks: TaskTracker,
    /// Set by stop_all; blocks new auto-restarts and hooks.
    shutting_down: AtomicBool,
    schedulers: TaskTracker,
    /// Cancelled by stop_all; ends all scheduler tasks.
    schedulers_cancel: CancellationToken,
}

struct Process {
    cfg: ProcessConfig,
    log_writer: Option<Arc<LogWriter>>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    state: State,
    pid: u32,
    started_at: Option<DateTime<Local>>,
    exit: Option<Exit>,
    stop_requested: bool,
    /// Flips to true once the current run has been reaped.
    done: Option<watch::Receiver<bool>>,
    metrics: Option<MetricsSampler>,

    // Details of the most recent run, kept after pid is cleared on exit -
    // the failed hook reports on a process that is already gone.
    last_pid: u32,
    exited_at: Option<DateTime<Local>>,

    /// Crash timestamps within the current restart_window, oldest first.
    restart_history: Vec<Instant>,
    pending_restart: Option<JoinHandle<()>>,
    /// Bumped whenever a pending restart is scheduled or cancelled, so a
    /// timer that already fired can tell it has been superseded.
    restart_gen: u64,

    /// When the schedule fires next; None without a schedule.
    next_run: Option<DateTime<Local>>,
}

#[derive(Debug, Clone, Default)]
struct Exit {
    code: i32,
    signal: String,
}

impl Process {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn info(&self) -> ProcessInfo {
        self.snapshot(&self.lock())
    }

    fn snapshot(&self, g: &Inner) -> ProcessInfo {
        let mut info = ProcessInfo {
            key: self.cfg.key.clone(),
            cmd: self.cfg.cmd.clone(),
            state: g.state,
            pid: 0,
            started_at: None,
            uptime_seconds: 0,
            readonly: self.cfg.readonly,
            autostart: self.cfg.autostart,
            stdout: self.cfg.stdout,
            logfile: self.cfg.logfile.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            rss_bytes: 0,
            cpu_percent: 0.0,
            exit_code: None,
            exit_signal: String::new(),
            schedule: String::new(),
            schedule_action: String::new(),
            next_run: g.next_run.map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
        };
        if let Some(schedule) = &self.cfg.schedule {
            info.schedule = schedule.to_string();
            info.schedule_action = self.cfg.schedule_action.as_str().to_owned();
        }
        if g.state.is_active() {
            info.pid = g.pid;
            if let Some(started) = g.started_at {
                info.started_at = Some(started.to_rfc3339_opts(SecondsFormat::Secs, true));
                info.uptime_seconds = (Local::now() - started).num_seconds().max(0) as u64;
            }
            if let Some(m) = &g.metrics {
                let m = m.snapshot();
                info.rss_bytes = m.rss_bytes;
                info.cpu_percent = m.cpu_percent;
            }
        }
        if let Some(exit) = &g.exit {
            info.exit_code = Some(exit.code);
            info.exit_signal = exit.signal.clone();
        }
        info
    }

    /// Cancels a scheduled auto-restart; returns whether one was pending.
    fn cancel_pending_restart(g: &mut Inner) -> bool {
        g.restart_gen += 1;
        match g.pending_restart.take() {
            Some(handle) => {
                handle.abort();
                true
            }
            None => false,
        }
    }
}

impl Manager {
    pub fn new(cfg: &Config, console: Arc<LogWriter>) -> anyhow::Result<Arc<Self>> {
        let crash_log = match &cfg.core.logfile {
            Some(path) => Some(LogWriter::append(path).with_context(|| format!("core.logfile {}", path.display()))?),
            None => None,
        };

        let mut processes = Vec::with_capacity(cfg.processes.len());
        for pc in &cfg.processes {
            let log_writer = match &pc.logfile {
                Some(path) => Some(Arc::new(
                    LogWriter::append(path).with_context(|| format!("process {:?}: logfile {}", pc.key, path.display()))?,
                )),
                None => None,
            };
            processes.push(Arc::new(Process { cfg: pc.clone(), log_writer, inner: Mutex::default() }));
        }

        Ok(Arc::new(Self {
            processes,
            console,
            crash_log,
            shutdown_timeout: cfg.core.shutdown_timeout,
            metrics_interval: cfg.core.metrics_interval,
            core_env: cfg.core.env_vars.clone(),
            failed_hook: cfg.core.failed_hook.clone(),
            failed_hook_timeout: cfg.core.failed_hook_timeout,
            hooks: TaskTracker::new(),
            shutting_down: AtomicBool::new(false),
            schedulers: TaskTracker::new(),
            schedulers_cancel: CancellationToken::new(),
        }))
    }

    fn find(&self, key: &str) -> Option<&Arc<Process>> {
        self.processes.iter().find(|p| p.cfg.key == key)
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Starts every process with autostart = true. Keeps going on errors so
    /// one bad entry doesn't block the rest; returns one message per failure.
    pub fn autostart(self: &Arc<Self>) -> Vec<String> {
        self.processes
            .iter()
            .filter(|p| p.cfg.autostart)
            .filter_map(|p| self.start_process(p, None).err().map(|e| format!("{}: {e}", p.cfg.key)))
            .collect()
    }

    /// Spawns one scheduler task per process with a schedule. Call once,
    /// after autostart.
    pub fn start_schedulers(self: &Arc<Self>) {
        for p in &self.processes {
            let Some(schedule) = p.cfg.schedule.clone() else { continue };
            let next = schedule.next_after(Local::now());
            p.lock().next_run = next;
            let when = match next {
                Some(t) => format!("next run {}", t.format("%Y-%m-%d %H:%M:%S")),
                None => "never fires".to_owned(),
            };
            self.console.line(
                "core",
                &format!("{}: schedule {:?} ({}), {when}", p.cfg.key, schedule.to_string(), p.cfg.schedule_action.as_str()),
            );
            if next.is_some() {
                self.schedulers.spawn(Arc::clone(self).run_schedule(Arc::clone(p), schedule));
            }
        }
    }

    async fn run_schedule(self: Arc<Self>, p: Arc<Process>, schedule: Schedule) {
        let mut next = p.lock().next_run;
        while let Some(at) = next {
            // Sleep against the wall clock in bounded steps (see SCHEDULE_MAX_SLEEP).
            loop {
                let remaining = (at - Local::now()).to_std().unwrap_or_default();
                if remaining.is_zero() {
                    break;
                }
                tokio::select! {
                    _ = self.schedulers_cancel.cancelled() => return,
                    _ = tokio::time::sleep(remaining.min(SCHEDULE_MAX_SLEEP)) => {}
                }
            }

            self.fire_schedule(&p).await;
            // Ticks missed while the action ran (e.g. a slow stop) or across
            // a clock jump are skipped, not fired in a burst.
            next = schedule.next_after(at.max(Local::now()));
            p.lock().next_run = next;
        }
    }

    /// Scheduled actions bypass `readonly` like shutdown does: they come
    /// from process.toml, not from an API client.
    async fn fire_schedule(self: &Arc<Self>, p: &Arc<Process>) {
        let key = &p.cfg.key;
        if p.cfg.schedule_action == ScheduleAction::Restart {
            match self.stop(key).await {
                Ok(_) | Err(Error::AlreadyStopped(_)) => {}
                Err(e) => self.console.line("core", &format!("scheduled restart {key}: stop: {e}")),
            }
        }
        if self.schedulers_cancel.is_cancelled() {
            return;
        }
        let verb = p.cfg.schedule_action.as_str();
        match self.start_process(p, None) {
            Ok(info) => self.console.line("core", &format!("scheduled {verb}: {key} started (pid {})", info.pid)),
            Err(Error::AlreadyRunning(_)) => {
                self.console.line("core", &format!("scheduled {verb}: {key} is still running, skipped"));
            }
            Err(e) => self.console.line("core", &format!("scheduled {verb} {key}: {e}")),
        }
    }

    pub fn list(&self) -> Vec<ProcessInfo> {
        self.processes.iter().map(|p| p.info()).collect()
    }

    pub fn get(&self, key: &str) -> Option<ProcessInfo> {
        self.find(key).map(|p| p.info())
    }

    /// Manual (re)start: a fresh slate, so the crash-burst history is
    /// cleared and a process in `failed` gets a clean chance again.
    pub fn start(self: &Arc<Self>, key: &str) -> Result<ProcessInfo, Error> {
        let p = self.find(key).ok_or(Error::NotFound)?;
        self.start_process(p, None)
    }

    /// `auto_restart` carries the restart generation when called from a
    /// restart timer: that path must keep the restart history (otherwise the
    /// burst limit is never reached) and must back off if the restart was
    /// cancelled after the timer fired.
    fn start_process(self: &Arc<Self>, p: &Arc<Process>, auto_restart: Option<u64>) -> Result<ProcessInfo, Error> {
        let mut g = p.lock();
        if let Some(generation) = auto_restart {
            if generation != g.restart_gen || self.is_shutting_down() {
                return Err(Error::Superseded);
            }
            g.pending_restart = None;
        }
        if g.state.is_active() {
            return Err(Error::AlreadyRunning(Box::new(p.snapshot(&g))));
        }
        if auto_restart.is_none() {
            Process::cancel_pending_restart(&mut g);
            g.restart_history.clear();
        }

        // The lock is held across spawn (which is synchronous), so no other
        // caller can observe or race a half-started process.
        if let Err(e) = self.spawn_locked(p, &mut g) {
            g.state = State::Stopped;
            return Err(Error::Spawn(e));
        }
        Ok(p.snapshot(&g))
    }

    fn spawn_locked(self: &Arc<Self>, p: &Arc<Process>, g: &mut Inner) -> io::Result<()> {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&p.cfg.cmd)
            .envs(self.core_env.iter().map(|(k, v)| (k, v)))
            .envs(p.cfg.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so stop can signal children of the shell too.
            .process_group(0);
        if let Some(dir) = &p.cfg.workdir {
            cmd.current_dir(dir);
        }

        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or_default();
        let (done_tx, done_rx) = watch::channel(false);

        g.state = State::Running;
        g.pid = pid;
        g.started_at = Some(Local::now());
        g.stop_requested = false;
        g.exit = None;
        g.done = Some(done_rx);
        g.metrics = Some(MetricsSampler::spawn(pid, self.metrics_interval));

        let key = &p.cfg.key;
        let pumps = [
            child.stdout.take().map(|out| tokio::spawn(pump_output(out, key.clone(), "", Arc::clone(&self.console), p.log_writer.clone(), p.cfg.stdout))),
            child.stderr.take().map(|err| tokio::spawn(pump_output(err, key.clone(), "", Arc::clone(&self.console), p.log_writer.clone(), p.cfg.stdout))),
        ];

        let mgr = Arc::clone(self);
        let process = Arc::clone(p);
        tokio::spawn(async move {
            let status = child.wait().await;
            // Let the output drain so the last lines are logged before the
            // state flips - but bounded: an orphaned grandchild (e.g. when
            // the shell itself was killed) can keep the pipes open forever,
            // and must not keep the exit from being noticed. Pumps that are
            // still busy keep forwarding in the background.
            let drain = async {
                for pump in pumps.into_iter().flatten() {
                    let _ = pump.await;
                }
            };
            let _ = tokio::time::timeout(OUTPUT_DRAIN_GRACE, drain).await;
            mgr.on_exit(&process, status, done_tx);
        });
        Ok(())
    }

    /// The single writer of terminal states (stopped/exited/crashed).
    fn on_exit(self: &Arc<Self>, p: &Arc<Process>, status: io::Result<ExitStatus>, done: watch::Sender<bool>) {
        let exit = match status {
            Ok(s) => match (s.code(), s.signal()) {
                (Some(code), _) => Exit { code, signal: String::new() },
                (None, Some(sig)) => Exit { code: -1, signal: signal_name(sig) },
                (None, None) => Exit { code: -1, signal: String::new() },
            },
            Err(_) => Exit { code: -1, signal: String::new() },
        };

        let crash = {
            let mut g = p.lock();
            g.metrics = None;
            g.state = if g.stop_requested {
                State::Stopped
            } else if exit.code == 0 && exit.signal.is_empty() {
                State::Exited
            } else {
                State::Crashed
            };
            g.exit = Some(exit.clone());
            g.exited_at = Some(Local::now());
            g.last_pid = g.pid;
            g.pid = 0;
            (g.state == State::Crashed).then(|| CrashEvent::snapshot(p, &g))
        };
        done.send_replace(true);

        if let Some(mut ev) = crash {
            self.record_crash(p, &exit);
            // The hook runs after the restart decision, so it can report
            // whether the process is coming back - one hook call per crash.
            ev.apply(self.maybe_restart(p));
            self.run_failed_hook(p, ev);
        }
    }

    /// One line in the shared crash log (core.logfile) per crash, regardless
    /// of the restart policy.
    fn record_crash(&self, p: &Process, exit: &Exit) {
        if let Some(log) = &self.crash_log {
            log.line(
                &p.cfg.key,
                &format!("crashed: exit_code={} signal={:?} cmd={:?}", exit.code, exit.signal, p.cfg.cmd),
            );
        }
    }

    /// systemd-style Restart=on-failure: wait restart_delay and try again,
    /// unless the process already crashed restart_max_retries times within
    /// restart_window - then give up and move to `failed`, so a crash loop
    /// can't run forever unnoticed.
    fn maybe_restart(self: &Arc<Self>, p: &Arc<Process>) -> RestartOutcome {
        let cfg = &p.cfg;
        let max = cfg.restart_max_retries;
        if !cfg.restart || self.is_shutting_down() {
            return RestartOutcome::default();
        }

        let mut g = p.lock();
        if g.state != State::Crashed {
            // Superseded by a manual start/stop in the meantime.
            return RestartOutcome::default();
        }

        let now = Instant::now();
        g.restart_history.retain(|t| now.duration_since(*t) < cfg.restart_window);
        let window = duration::format(cfg.restart_window);

        if max > 0 && g.restart_history.len() >= max as usize {
            g.state = State::Failed;
            drop(g);
            self.console.line("core", &format!("{} crashed {max} times within {window}, giving up automatic restarts", cfg.key));
            return RestartOutcome { gave_up: true, attempt: max as usize, ..Default::default() };
        }

        g.restart_history.push(now);
        let attempt = g.restart_history.len();
        g.restart_gen += 1;
        let generation = g.restart_gen;
        let delay = cfg.restart_delay;
        let mgr = Arc::clone(self);
        let process = Arc::clone(p);
        g.pending_restart = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            match mgr.start_process(&process, Some(generation)) {
                Ok(_) | Err(Error::Superseded) => {}
                Err(e) => mgr.console.line("core", &format!("auto-restart {}: {e}", process.cfg.key)),
            }
        }));
        drop(g);

        let attempts = if max > 0 { format!("{attempt}/{max}") } else { attempt.to_string() };
        self.console.line(
            "core",
            &format!("{} crashed, restarting in {} (attempt {attempts} within {window})", cfg.key, duration::format(delay)),
        );
        RestartOutcome { scheduled: true, gave_up: false, delay, attempt }
    }

    /// SIGTERM to the process group, SIGKILL after shutdown_timeout. Also
    /// cancels a pending auto-restart ("stop" means "don't come back on its
    /// own"), so stopping a crash-looping process between restarts succeeds.
    pub async fn stop(self: &Arc<Self>, key: &str) -> Result<ProcessInfo, Error> {
        let p = self.find(key).ok_or(Error::NotFound)?;

        let (pid, mut done) = {
            let mut g = p.lock();
            let cancelled_restart = Process::cancel_pending_restart(&mut g);
            if !matches!(g.state, State::Running | State::Starting) {
                let info = p.snapshot(&g);
                return if cancelled_restart { Ok(info) } else { Err(Error::AlreadyStopped(Box::new(info))) };
            }
            g.stop_requested = true;
            g.state = State::Stopping;
            (g.pid, g.done.clone().expect("running process has a done channel"))
        };

        if let Err(e) = kill_group(pid, Signal::SIGTERM) {
            self.console.line("core", &format!("sigterm {key} (pid {pid}): {e}"));
        }
        if tokio::time::timeout(self.shutdown_timeout, done.wait_for(|d| *d)).await.is_err() {
            self.console.line(
                "core",
                &format!("{key} did not stop within {}, sending SIGKILL", duration::format(self.shutdown_timeout)),
            );
            if let Err(e) = kill_group(pid, Signal::SIGKILL) {
                self.console.line("core", &format!("sigkill {key} (pid {pid}): {e}"));
            }
            let _ = done.wait_for(|d| *d).await;
        }
        Ok(p.info())
    }

    /// Stops everything for procd shutdown, bypassing readonly (that is
    /// enforced only at the API layer). Ends the schedulers, cancels all
    /// pending restarts and blocks new ones, then waits for in-flight hooks.
    pub async fn stop_all(self: &Arc<Self>) {
        self.shutting_down.store(true, Ordering::SeqCst);

        // Schedulers first: one in the middle of a scheduled restart must not
        // start the process again after it has been stopped below.
        self.schedulers_cancel.cancel();
        self.schedulers.close();
        self.schedulers.wait().await;

        let mut stops = JoinSet::new();
        for p in &self.processes {
            let running = {
                let mut g = p.lock();
                Process::cancel_pending_restart(&mut g);
                matches!(g.state, State::Running | State::Starting)
            };
            if running {
                let mgr = Arc::clone(self);
                let key = p.cfg.key.clone();
                stops.spawn(async move {
                    if let Err(e) = mgr.stop(&key).await {
                        mgr.console.line("core", &format!("stop {key}: {e}"));
                    }
                });
            }
        }
        stops.join_all().await;

        // Running hooks finish (bounded by failed_hook_timeout) instead of
        // being cut off mid-notification.
        self.hooks.close();
        self.hooks.wait().await;
    }
}

/// Always drains the pipe (otherwise the child blocks once the pipe buffer is
/// full) and forwards lines to whichever outputs are enabled. The console
/// gets a timestamp+key prefix; the per-process logfile gets the raw line.
async fn pump_output<R: AsyncRead + Unpin>(
    pipe: R,
    key: String,
    prefix: &'static str,
    console: Arc<LogWriter>,
    file: Option<Arc<LogWriter>>,
    console_enabled: bool,
) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let line = String::from_utf8_lossy(line);
                if console_enabled {
                    console.line(&key, &format!("{prefix}{line}"));
                }
                if let Some(file) = &file {
                    file.raw(&line);
                }
            }
            Err(e) => {
                if console_enabled {
                    console.line(&key, &format!("[procd] output read error: {e}"));
                }
                break;
            }
        }
    }
}

fn kill_group(pid: u32, signal: Signal) -> nix::Result<()> {
    killpg(Pid::from_raw(pid as i32), signal)
}

fn describe_status(s: ExitStatus) -> String {
    match (s.code(), s.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(sig)) => format!("signal: {}", signal_name(sig)),
        _ => s.to_string(),
    }
}

/// Signal names as the Go version reported them (PROCD_EXIT_SIGNAL,
/// exit_signal in the API), so hook scripts and clients keep matching.
fn signal_name(sig: i32) -> String {
    let name = match Signal::try_from(sig) {
        Ok(Signal::SIGHUP) => "hangup",
        Ok(Signal::SIGINT) => "interrupt",
        Ok(Signal::SIGQUIT) => "quit",
        Ok(Signal::SIGILL) => "illegal instruction",
        Ok(Signal::SIGTRAP) => "trace/breakpoint trap",
        Ok(Signal::SIGABRT) => "aborted",
        Ok(Signal::SIGBUS) => "bus error",
        Ok(Signal::SIGFPE) => "floating point exception",
        Ok(Signal::SIGKILL) => "killed",
        Ok(Signal::SIGUSR1) => "user defined signal 1",
        Ok(Signal::SIGSEGV) => "segmentation fault",
        Ok(Signal::SIGUSR2) => "user defined signal 2",
        Ok(Signal::SIGPIPE) => "broken pipe",
        Ok(Signal::SIGALRM) => "alarm clock",
        Ok(Signal::SIGTERM) => "terminated",
        _ => return format!("signal {sig}"),
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn manager(toml: &str) -> (Arc<Manager>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("process.toml");
        std::fs::write(&path, toml).unwrap();
        let cfg = config::load(&path).unwrap();
        (Manager::new(&cfg, Arc::new(LogWriter::stdout())).unwrap(), dir)
    }

    async fn wait_for_state(m: &Manager, key: &str, want: State) -> ProcessInfo {
        for _ in 0..100 {
            let info = m.get(key).unwrap();
            if info.state == want {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{key} never reached {want}, is {}", m.get(key).unwrap().state);
    }

    #[tokio::test]
    async fn start_stop_and_already_states() {
        let (m, _dir) = manager("[[process]]\nkey = \"s\"\ncmd = \"sleep 30\"\nstdout = false\n");
        let info = m.start("s").unwrap();
        assert_eq!(info.state, State::Running);
        assert!(info.pid > 0);
        assert!(matches!(m.start("s"), Err(Error::AlreadyRunning(_))));

        let info = m.stop("s").await.unwrap();
        assert_eq!(info.state, State::Stopped);
        assert_eq!(info.exit_signal, "terminated");
        assert!(matches!(m.stop("s").await, Err(Error::AlreadyStopped(_))));
        assert!(matches!(m.start("nope"), Err(Error::NotFound)));
    }

    #[tokio::test]
    async fn clean_exit_is_not_a_crash() {
        let (m, _dir) = manager("[[process]]\nkey = \"t\"\ncmd = \"true\"\nrestart = true\n");
        m.start("t").unwrap();
        let info = wait_for_state(&m, "t", State::Exited).await;
        assert_eq!(info.exit_code, Some(0));
    }

    #[tokio::test]
    async fn crash_loop_gives_up_and_runs_hook() {
        let (m, dir) = manager(
            r#"
            [core]
            logfile = "crashes.log"
            failed_hook = "echo $PROCD_EVENT $PROCD_RESTART_COUNT $PROCD_EXIT_CODE >> hook.out"

            [[process]]
            key = "c"
            cmd = "exit 3"
            workdir = "."
            restart = true
            restart_delay = "10ms"
            restart_max_retries = 2
            "#,
        );

        m.start("c").unwrap();
        let info = wait_for_state(&m, "c", State::Failed).await;
        assert_eq!(info.exit_code, Some(3));
        m.stop_all().await; // waits for hooks

        let hooks = std::fs::read_to_string(dir.path().join("hook.out")).unwrap();
        assert_eq!(hooks.lines().collect::<Vec<_>>(), ["crashed 1 3", "crashed 2 3", "failed 2 3"]);
        let crashes = std::fs::read_to_string(dir.path().join("crashes.log")).unwrap();
        assert_eq!(crashes.lines().count(), 3);
    }

    #[tokio::test]
    async fn stop_cancels_pending_restart() {
        let (m, _dir) = manager("[[process]]\nkey = \"c\"\ncmd = \"exit 1\"\nrestart = true\nrestart_delay = \"5s\"\n");
        m.start("c").unwrap();
        wait_for_state(&m, "c", State::Crashed).await;
        assert_eq!(m.stop("c").await.unwrap().state, State::Crashed);
        // Nothing pending any more, so a second stop reports the state.
        assert!(matches!(m.stop("c").await, Err(Error::AlreadyStopped(_))));
    }

    #[tokio::test]
    async fn schedule_starts_finished_job_again() {
        let (m, dir) = manager(
            "[[process]]\nkey = \"j\"\ncmd = \"echo run >> runs\"\nworkdir = \".\"\nautostart = false\nschedule = \"@every 1s\"\n",
        );
        m.start_schedulers();
        assert!(m.get("j").unwrap().next_run.is_some());
        tokio::time::sleep(Duration::from_millis(2500)).await;
        m.stop_all().await;
        let runs = std::fs::read_to_string(dir.path().join("runs")).unwrap();
        assert_eq!(runs.lines().count(), 2);
    }

    #[tokio::test]
    async fn schedule_restart_replaces_running_process() {
        let (m, _dir) = manager(
            "[[process]]\nkey = \"r\"\ncmd = \"exec sleep 30\"\nstdout = false\nreadonly = true\nschedule = \"@every 1s\"\nschedule_action = \"restart\"\n",
        );
        let first = m.start("r").unwrap().pid;
        m.start_schedulers();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let info = m.get("r").unwrap();
        assert_eq!(info.state, State::Running);
        assert_ne!(info.pid, first);
        assert_eq!(info.schedule_action, "restart");
        m.stop_all().await;
        assert_eq!(m.get("r").unwrap().state, State::Stopped);
    }

    #[tokio::test]
    async fn exit_is_noticed_while_orphan_holds_output_pipe() {
        // The shell forks `sleep` (no exec, because of the trailing echo);
        // killing only the shell leaves sleep holding stdout open.
        let (m, _dir) = manager("[[process]]\nkey = \"o\"\ncmd = \"sleep 30; echo done\"\nstdout = false\n");
        let pid = m.start("o").unwrap().pid;
        nix::sys::signal::kill(Pid::from_raw(pid as i32), Signal::SIGKILL).unwrap();
        let info = wait_for_state(&m, "o", State::Crashed).await;
        assert_eq!(info.exit_signal, "killed");
        kill_group(pid, Signal::SIGKILL).ok(); // the orphaned sleep
    }
}
