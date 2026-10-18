//! failed_hook: a shell command run once per crash, with the crash details
//! passed as PROCD_* environment variables.

use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat};
use nix::sys::signal::Signal;
use tokio::process::Command;

use super::{Inner, Manager, Process, State, describe_status, kill_group, pump_output};
use crate::logging::LogWriter;

/// Snapshot of one crash. Taken while the process lock is held, because pid,
/// start time and exit details are overwritten as soon as a scheduled
/// restart fires.
pub(super) struct CrashEvent {
    event: &'static str, // "crashed", or "failed" once auto-restart gave up
    key: String,
    cmd: String,
    workdir: String,
    logfile: String,
    state: State,
    pid: u32,
    exit_code: i32,
    exit_signal: String,
    started_at: Option<DateTime<Local>>,
    exited_at: Option<DateTime<Local>>,
    restart_enabled: bool,
    restart_scheduled: bool,
    restart_delay: Duration,
    restart_count: usize,
    restart_max: u32,
}

/// What maybe_restart decided, so the hook can report whether the process is
/// coming back.
#[derive(Default)]
pub(super) struct RestartOutcome {
    pub scheduled: bool,
    pub gave_up: bool,
    pub delay: Duration,
    /// Crashes counted within the current restart_window.
    pub attempt: usize,
}

impl CrashEvent {
    pub(super) fn snapshot(p: &Process, g: &Inner) -> Self {
        let display = |path: &Option<std::path::PathBuf>| {
            path.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
        };
        let exit = g.exit.clone().unwrap_or_default();
        Self {
            event: "crashed",
            key: p.cfg.key.clone(),
            cmd: p.cfg.cmd.clone(),
            workdir: display(&p.cfg.workdir),
            logfile: display(&p.cfg.logfile),
            state: g.state,
            pid: g.last_pid,
            exit_code: exit.code,
            exit_signal: exit.signal,
            started_at: g.started_at,
            exited_at: g.exited_at,
            restart_enabled: p.cfg.restart,
            restart_scheduled: false,
            restart_delay: Duration::ZERO,
            restart_count: 0,
            restart_max: p.cfg.restart_max_retries,
        }
    }

    /// Giving up on automatic restarts turns a "crashed" event into "failed".
    pub(super) fn apply(&mut self, o: RestartOutcome) {
        if o.gave_up {
            self.event = "failed";
            self.state = State::Failed;
        }
        self.restart_scheduled = o.scheduled;
        self.restart_delay = o.delay;
        self.restart_count = o.attempt;
    }

    /// Every variable is always present (empty rather than absent), so
    /// scripts can read them under `set -u` without checks.
    fn env(&self) -> Vec<(String, String)> {
        let rfc3339 = |t: Option<DateTime<Local>>| {
            t.map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)).unwrap_or_default()
        };
        let uptime = match (self.started_at, self.exited_at) {
            (Some(s), Some(e)) => (e - s).num_seconds().max(0),
            _ => 0,
        };
        [
            ("EVENT", self.event.to_owned()),
            ("KEY", self.key.clone()),
            ("CMD", self.cmd.clone()),
            ("WORKDIR", self.workdir.clone()),
            ("LOGFILE", self.logfile.clone()),
            ("STATE", self.state.to_string()),
            ("PID", self.pid.to_string()),
            ("EXIT_CODE", self.exit_code.to_string()),
            ("EXIT_SIGNAL", self.exit_signal.clone()),
            ("STARTED_AT", rfc3339(self.started_at)),
            ("EXITED_AT", rfc3339(self.exited_at)),
            ("UPTIME_SECONDS", uptime.to_string()),
            ("RESTART_ENABLED", self.restart_enabled.to_string()),
            ("RESTART_SCHEDULED", self.restart_scheduled.to_string()),
            ("RESTART_DELAY_SECONDS", self.restart_delay.as_secs().to_string()),
            ("RESTART_COUNT", self.restart_count.to_string()),
            ("RESTART_MAX_RETRIES", self.restart_max.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (format!("PROCD_{k}"), v))
        .collect()
    }
}

impl Manager {
    /// Launches the hook for a crash in the background, so a slow hook never
    /// stalls the exit/restart path. No new hooks start once shutdown began;
    /// stop_all waits for the ones already running.
    pub(super) fn run_failed_hook(&self, p: &Process, ev: CrashEvent) {
        let hook = p.cfg.failed_hook.as_deref().unwrap_or(&self.failed_hook);
        if hook.is_empty() || self.is_shutting_down() {
            return;
        }

        // Same environment as the crashed process itself, plus the event.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(hook)
            .envs(self.core_env.iter().map(|(k, v)| (k, v)))
            .envs(p.cfg.env.iter().map(|(k, v)| (k, v)))
            .envs(ev.env())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(dir) = &p.cfg.workdir {
            cmd.current_dir(dir);
        }

        let console = Arc::clone(&self.console);
        let timeout = self.failed_hook_timeout;
        self.hooks.spawn(exec_hook(cmd, console, timeout, ev.key, ev.event));
    }
}

/// Runs the hook in its own process group (so children can be killed as a
/// whole on timeout) and echoes its output under the crashed process's key.
async fn exec_hook(mut cmd: Command, console: Arc<LogWriter>, timeout: Duration, key: String, event: &str) {
    console.line(&key, &format!("[hook] running failed_hook (event={event})"));
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            console.line(&key, &format!("[hook] failed_hook error: {e}"));
            return;
        }
    };

    let pumps = [
        child.stdout.take().map(|out| tokio::spawn(pump_output(out, key.clone(), "[hook] ", Arc::clone(&console), None, true))),
        child.stderr.take().map(|err| tokio::spawn(pump_output(err, key.clone(), "[hook] ", Arc::clone(&console), None, true))),
    ];

    let status: Option<std::io::Result<ExitStatus>> = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => Some(status),
        Err(_) => {
            if let Some(pid) = child.id() {
                let _ = kill_group(pid, Signal::SIGKILL);
            }
            let _ = child.wait().await;
            None
        }
    };

    // A backgrounded grandchild could keep the pipes open forever; don't let
    // it block shutdown.
    for pump in pumps.into_iter().flatten() {
        let abort = pump.abort_handle();
        if tokio::time::timeout(Duration::from_secs(1), pump).await.is_err() {
            abort.abort();
        }
    }

    match status {
        None => console.line(&key, &format!("[hook] failed_hook timed out after {}, killed", crate::duration::format(timeout))),
        Some(Ok(s)) if s.success() => console.line(&key, "[hook] ok"),
        Some(Ok(s)) => console.line(&key, &format!("[hook] failed_hook error: {}", describe_status(s))),
        Some(Err(e)) => console.line(&key, &format!("[hook] failed_hook error: {e}")),
    }
}
