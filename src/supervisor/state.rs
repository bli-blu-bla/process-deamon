use std::fmt;

use serde::{Deserialize, Serialize};

/// Lifecycle state of a managed process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    /// Exited on its own with code 0, no stop was requested.
    Exited,
    /// Exited on its own with a non-zero code or via signal, no stop was requested.
    Crashed,
    /// Crashed restart_max_retries times within restart_window; auto-restart gave up.
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Stopped => "stopped",
            State::Starting => "starting",
            State::Running => "running",
            State::Stopping => "stopping",
            State::Exited => "exited",
            State::Crashed => "crashed",
            State::Failed => "failed",
        }
    }

    /// A process in one of these states has a live child.
    pub fn is_active(self) -> bool {
        matches!(self, State::Starting | State::Running | State::Stopping)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// Wire representation of a process's status, returned by process.list and
/// process.get. Field names and omit-when-empty rules match the Go version,
/// so existing API clients keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub key: String,
    pub cmd: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uptime_seconds: u64,
    pub readonly: bool,
    pub autostart: bool,
    pub stdout: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub logfile: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rss_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub cpu_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exit_signal: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run: Option<String>,
}

fn is_zero<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}
