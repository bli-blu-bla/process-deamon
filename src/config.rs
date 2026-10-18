//! process.toml loading. The file format is the same as the Go version's:
//! unknown fields are a hard error (typo protection), durations are Go-style
//! strings, and relative paths resolve against the config file's directory.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, de::Error as _};

use crate::duration;
use crate::schedule::{Action as ScheduleAction, Schedule};

pub const DEFAULT_SOCK: &str = "/run/procd.sock";
pub const DEFAULT_SOCKET_PERM: u32 = 0o660;
pub const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_METRICS_INTERVAL: Duration = Duration::from_secs(2);
pub const DEFAULT_RESTART_DELAY: Duration = Duration::from_secs(1);
pub const DEFAULT_RESTART_WINDOW: Duration = Duration::from_secs(60);
pub const DEFAULT_RESTART_MAX_RETRIES: u32 = 5;
pub const DEFAULT_FAILED_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct Config {
    pub core: CoreConfig,
    pub processes: Vec<ProcessConfig>,
}

#[derive(Debug, Clone)]
pub struct CoreConfig {
    pub sock: PathBuf,
    pub socket_perm: u32,
    pub shutdown_timeout: Duration,
    pub metrics_interval: Duration,
    pub envfile: Option<PathBuf>,
    /// Parsed KEY=VALUE pairs from `envfile`, applied to every process.
    pub env_vars: Vec<(String, String)>,
    /// Shared crash log across all processes (one line per crash).
    pub logfile: Option<PathBuf>,
    /// Run on every crash unless a process overrides it; empty = disabled.
    pub failed_hook: String,
    pub failed_hook_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ProcessConfig {
    pub key: String,
    pub cmd: String,
    pub workdir: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Protects only against process.stop over the API.
    pub readonly: bool,
    pub autostart: bool,
    pub stdout: bool,
    pub logfile: Option<PathBuf>,

    /// Restart policy, modeled after systemd's Restart=on-failure: only
    /// applies to crashes (non-zero exit / killed by signal), never to a
    /// clean exit or an explicit process.stop.
    pub restart: bool,
    pub restart_delay: Duration,
    pub restart_window: Duration,
    /// 0 = unlimited.
    pub restart_max_retries: u32,

    /// None inherits core.failed_hook; Some("") disables the hook for this
    /// process only.
    pub failed_hook: Option<String>,

    /// Cron-style trigger; independent of autostart and the restart policy.
    pub schedule: Option<Schedule>,
    pub schedule_action: ScheduleAction,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    core: RawCore,
    #[serde(default, rename = "process")]
    processes: Vec<RawProcess>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCore {
    sock: Option<PathBuf>,
    socket_perm: Option<u32>,
    #[serde(default, deserialize_with = "de_duration")]
    shutdown_timeout: Option<Duration>,
    #[serde(default, deserialize_with = "de_duration")]
    metrics_interval: Option<Duration>,
    envfile: Option<PathBuf>,
    logfile: Option<PathBuf>,
    failed_hook: Option<String>,
    #[serde(default, deserialize_with = "de_duration")]
    failed_hook_timeout: Option<Duration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProcess {
    #[serde(default)]
    key: String,
    #[serde(default)]
    cmd: String,
    workdir: Option<PathBuf>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    readonly: bool,
    autostart: Option<bool>,
    stdout: Option<bool>,
    logfile: Option<PathBuf>,
    #[serde(default)]
    restart: bool,
    #[serde(default, deserialize_with = "de_duration")]
    restart_delay: Option<Duration>,
    #[serde(default, deserialize_with = "de_duration")]
    restart_window: Option<Duration>,
    restart_max_retries: Option<u32>,
    failed_hook: Option<String>,
    schedule: Option<String>,
    schedule_action: Option<String>,
}

fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    let s = String::deserialize(d)?;
    duration::parse(&s).map(Some).map_err(D::Error::custom)
}

/// Zero (or unset) means "use the default", as in the Go version.
fn or_default(d: Option<Duration>, default: Duration) -> Duration {
    d.filter(|d| !d.is_zero()).unwrap_or(default)
}

fn non_empty(p: Option<PathBuf>) -> Option<PathBuf> {
    p.filter(|p| !p.as_os_str().is_empty())
}

pub fn load(path: &Path) -> Result<Config> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let raw: RawConfig =
        toml::from_str(&text).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;

    let abs = std::path::absolute(path).with_context(|| format!("resolve {}", path.display()))?;
    let config_dir = abs.parent().unwrap_or(Path::new("/"));
    let resolve = |p: PathBuf| if p.is_absolute() { p } else { config_dir.join(p) };

    let envfile = non_empty(raw.core.envfile).map(resolve);
    let env_vars = match &envfile {
        Some(path) => parse_env_file(path).context("core.envfile")?,
        None => Vec::new(),
    };

    let core = CoreConfig {
        sock: non_empty(raw.core.sock).unwrap_or_else(|| DEFAULT_SOCK.into()),
        socket_perm: raw.core.socket_perm.filter(|&p| p != 0).unwrap_or(DEFAULT_SOCKET_PERM),
        shutdown_timeout: or_default(raw.core.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT),
        metrics_interval: or_default(raw.core.metrics_interval, DEFAULT_METRICS_INTERVAL),
        envfile,
        env_vars,
        logfile: non_empty(raw.core.logfile).map(resolve),
        failed_hook: raw.core.failed_hook.unwrap_or_default(),
        failed_hook_timeout: or_default(raw.core.failed_hook_timeout, DEFAULT_FAILED_HOOK_TIMEOUT),
    };

    let mut seen = HashSet::new();
    let mut processes = Vec::with_capacity(raw.processes.len());
    for (i, p) in raw.processes.into_iter().enumerate() {
        if p.key.is_empty() {
            bail!("{}: process #{} has an empty key", path.display(), i + 1);
        }
        if !seen.insert(p.key.clone()) {
            bail!("{}: duplicate process key {:?}", path.display(), p.key);
        }
        if p.cmd.is_empty() {
            bail!("{}: process {:?}: cmd is required", path.display(), p.key);
        }
        let env = p
            .env
            .iter()
            .map(|entry| {
                entry
                    .split_once('=')
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .ok_or_else(|| anyhow!("{}: process {:?}: env entry {entry:?} is not KEY=VALUE", path.display(), p.key))
            })
            .collect::<Result<_>>()?;

        let schedule = p
            .schedule
            .filter(|s| !s.trim().is_empty())
            .map(|s| Schedule::parse(&s))
            .transpose()
            .map_err(|e| anyhow!("{}: process {:?}: {e}", path.display(), p.key))?;
        let schedule_action = match p.schedule_action.as_deref() {
            None => ScheduleAction::default(),
            Some(_) if schedule.is_none() => {
                bail!("{}: process {:?}: schedule_action requires schedule", path.display(), p.key)
            }
            Some(a) => ScheduleAction::parse(a).map_err(|e| anyhow!("{}: process {:?}: {e}", path.display(), p.key))?,
        };

        processes.push(ProcessConfig {
            key: p.key,
            cmd: p.cmd,
            workdir: non_empty(p.workdir).map(resolve),
            env,
            readonly: p.readonly,
            autostart: p.autostart.unwrap_or(true),
            stdout: p.stdout.unwrap_or(true),
            logfile: non_empty(p.logfile).map(resolve),
            restart: p.restart,
            restart_delay: or_default(p.restart_delay, DEFAULT_RESTART_DELAY),
            restart_window: or_default(p.restart_window, DEFAULT_RESTART_WINDOW),
            restart_max_retries: p.restart_max_retries.unwrap_or(DEFAULT_RESTART_MAX_RETRIES),
            failed_hook: p.failed_hook,
            schedule,
            schedule_action,
        });
    }

    Ok(Config { core, processes })
}

/// Reads only `core.sock` from a config file, for the client subcommands. A
/// missing file is not an error (the default socket is used then), and no
/// other validation happens, so the client works even with a config the
/// daemon would reject for unrelated reasons (e.g. a missing envfile).
pub fn socket_from_file(path: &Path) -> Result<Option<PathBuf>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let table: toml::Table =
        toml::from_str(&text).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
    Ok(table
        .get("core")
        .and_then(|core| core.get("sock"))
        .and_then(|sock| sock.as_str())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

/// Minimal dotenv: one KEY=VALUE per line, blank lines and `#` comments are
/// ignored, values may be wrapped in matching single or double quotes. No
/// interpolation, no multi-line values - deliberately simple.
fn parse_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    let data = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut vars = Vec::new();
    for (i, raw_line) in data.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("{}:{}: missing '=' in {raw_line:?}", path.display(), i + 1);
        };
        let key = key.trim();
        if key.is_empty() {
            bail!("{}:{}: empty key", path.display(), i + 1);
        }
        vars.push((key.to_owned(), unquote(value.trim()).to_owned()));
    }
    Ok(vars)
}

fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[0] == b[b.len() - 1] {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn applies_defaults_and_resolves_paths() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".env", "# comment\nTOKEN=\"abc\"\nPLAIN = x=y\n");
        let cfg = load(&write(
            dir.path(),
            "process.toml",
            r#"
            [core]
            socket_perm = 0o600
            envfile = ".env"
            logfile = "crashes.log"

            [[process]]
            key = "a"
            cmd = "true"
            workdir = "sub"
            env = ["X=1"]
            restart_delay = "2s"
            restart_max_retries = 0
            failed_hook = ""
            "#,
        ))
        .unwrap();

        assert_eq!(cfg.core.sock, PathBuf::from(DEFAULT_SOCK));
        assert_eq!(cfg.core.socket_perm, 0o600);
        assert_eq!(cfg.core.shutdown_timeout, DEFAULT_SHUTDOWN_TIMEOUT);
        assert_eq!(cfg.core.env_vars, vec![("TOKEN".into(), "abc".into()), ("PLAIN".into(), "x=y".into())]);
        assert_eq!(cfg.core.logfile, Some(dir.path().join("crashes.log")));

        let p = &cfg.processes[0];
        assert_eq!(p.workdir, Some(dir.path().join("sub")));
        assert!(p.autostart && p.stdout && !p.readonly);
        assert_eq!(p.restart_delay, Duration::from_secs(2));
        assert_eq!(p.restart_window, DEFAULT_RESTART_WINDOW);
        assert_eq!(p.restart_max_retries, 0);
        assert_eq!(p.failed_hook.as_deref(), Some(""));
        assert_eq!(p.env, vec![("X".into(), "1".into())]);
    }

    #[test]
    fn rejects_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(&write(dir.path(), "p.toml", "[[process]]\nkey = \"a\"\ncmd = \"x\"\nrestrat = true\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("restrat"), "{err}");
    }

    #[test]
    fn rejects_duplicate_keys_and_bad_durations() {
        let dir = tempfile::tempdir().unwrap();
        let dup = "[[process]]\nkey=\"a\"\ncmd=\"x\"\n[[process]]\nkey=\"a\"\ncmd=\"y\"\n";
        assert!(load(&write(dir.path(), "d.toml", dup)).is_err());
        let bad = "[core]\nshutdown_timeout = \"10\"\n";
        assert!(load(&write(dir.path(), "b.toml", bad)).is_err());
    }

    #[test]
    fn parses_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let ok = "[[process]]\nkey=\"a\"\ncmd=\"x\"\nschedule=\"0 3 * * *\"\nschedule_action=\"restart\"\n";
        let p = &load(&write(dir.path(), "ok.toml", ok)).unwrap().processes[0];
        assert_eq!(p.schedule.as_ref().unwrap().to_string(), "0 3 * * *");
        assert_eq!(p.schedule_action, ScheduleAction::Restart);

        let bad_expr = "[[process]]\nkey=\"a\"\ncmd=\"x\"\nschedule=\"0 25 * * *\"\n";
        assert!(load(&write(dir.path(), "e.toml", bad_expr)).is_err());
        let action_only = "[[process]]\nkey=\"a\"\ncmd=\"x\"\nschedule_action=\"start\"\n";
        assert!(load(&write(dir.path(), "a.toml", action_only)).is_err());
    }

    #[test]
    fn loads_shipped_example() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("process.toml");
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.processes.len(), 3);
        assert!(cfg.processes[0].readonly);
        assert!(cfg.processes[1].restart);
    }
}
