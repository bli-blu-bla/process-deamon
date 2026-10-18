//! Client side of the socket API, used by the list/status/start/stop
//! subcommands to talk to a running daemon.

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cli::{ConnArgs, TargetArgs};
use crate::config;
use crate::rpc::{CODE_ALREADY_IN_TARGET_STATE, CODE_PROCESS_READONLY, Request, Response, RpcError};
use crate::supervisor::{ProcessInfo, State};

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: i64,
}

impl Client {
    pub fn connect(conn: &ConnArgs) -> Result<Self> {
        let path = socket_path(conn)?;
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("connect to {} (is procd running?)", path.display()))?;
        // A stop may legitimately take shutdown_timeout plus SIGKILL handling.
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream, next_id: 1 })
    }

    pub fn call(&mut self, method: &str, params: Option<Value>) -> Result<Result<Value, RpcError>> {
        let req = Request { id: Some(self.next_id), method: Some(method.into()), params };
        self.next_id += 1;
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        self.writer.write_all(&line).context("send request")?;

        let mut buf = String::new();
        if self.reader.read_line(&mut buf).context("read response")? == 0 {
            bail!("connection closed by procd");
        }
        let resp: Response = serde_json::from_str(&buf).context("invalid response")?;
        Ok(match resp.error {
            Some(err) => Err(err),
            None => Ok(resp.result.unwrap_or(Value::Null)),
        })
    }

    pub fn list(&mut self) -> Result<Vec<ProcessInfo>> {
        let result = self.call("process.list", None)??;
        Ok(serde_json::from_value(result["processes"].clone())?)
    }

    fn keyed(&mut self, method: &str, key: &str) -> Result<Result<ProcessInfo, RpcError>> {
        Ok(match self.call(method, Some(json!({ "key": key })))? {
            Ok(result) => Ok(serde_json::from_value(result["process"].clone())?),
            Err(e) => Err(e),
        })
    }
}

/// --socket wins; otherwise core.sock from the config file; otherwise the default.
fn socket_path(conn: &ConnArgs) -> Result<PathBuf> {
    if let Some(sock) = &conn.socket {
        return Ok(sock.clone());
    }
    Ok(config::socket_from_file(&conn.config)?.unwrap_or_else(|| PathBuf::from(config::DEFAULT_SOCK)))
}

pub fn list(conn: &ConnArgs, as_json: bool) -> Result<ExitCode> {
    let processes = Client::connect(conn)?.list()?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&processes)?);
    } else {
        print_table(&processes);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn status(conn: &ConnArgs, key: &str, as_json: bool) -> Result<ExitCode> {
    let info = Client::connect(conn)?.keyed("process.get", key)??;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&info)?);
        return Ok(ExitCode::SUCCESS);
    }

    let style = Style::detect();
    let mut rows: Vec<(&str, String)> = vec![
        ("key", info.key.clone()),
        ("state", style.state(info.state, info.state.as_str())),
        ("cmd", info.cmd.clone()),
    ];
    if info.pid != 0 {
        rows.push(("pid", info.pid.to_string()));
    }
    if let Some(started) = &info.started_at {
        rows.push(("started", started.clone()));
        rows.push(("uptime", human_duration(info.uptime_seconds)));
        rows.push(("memory", human_bytes(info.rss_bytes)));
        rows.push(("cpu", format!("{:.1}%", info.cpu_percent)));
    }
    if let Some(code) = info.exit_code {
        rows.push(("last exit", exit_text(code, &info.exit_signal)));
    }
    rows.push(("readonly", info.readonly.to_string()));
    rows.push(("autostart", info.autostart.to_string()));
    rows.push(("stdout", info.stdout.to_string()));
    if !info.logfile.is_empty() {
        rows.push(("logfile", info.logfile.clone()));
    }
    if !info.schedule.is_empty() {
        rows.push(("schedule", format!("{} ({})", info.schedule, info.schedule_action)));
        rows.push(("next run", info.next_run.clone().unwrap_or_else(|| "never".into())));
    }
    for (label, value) in rows {
        println!("{}  {value}", style.dim(&format!("{label:>10}")));
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Copy)]
pub enum Action {
    Start,
    Stop,
}

/// Starts/stops the given keys, or with --all every process that isn't
/// already in the target state (and, for stop, isn't readonly-protected).
pub fn control(action: Action, target: &TargetArgs) -> Result<ExitCode> {
    let mut client = Client::connect(&target.conn)?;
    let (method, verb) = match action {
        Action::Start => ("process.start", "started"),
        Action::Stop => ("process.stop", "stopped"),
    };

    let keys: Vec<String> = if target.all {
        client
            .list()?
            .into_iter()
            .filter(|p| match action {
                Action::Start => !p.state.is_active(),
                // Crashed processes may have an auto-restart pending, which
                // stop cancels.
                Action::Stop => matches!(p.state, State::Running | State::Starting | State::Crashed) && !p.readonly,
            })
            .map(|p| p.key)
            .collect()
    } else {
        target.keys.clone()
    };
    if keys.is_empty() && target.all {
        println!("nothing to do");
        return Ok(ExitCode::SUCCESS);
    }

    let style = Style::detect();
    let mut failed = false;
    for key in &keys {
        match client.keyed(method, key)? {
            Ok(info) => {
                let pid = if info.pid != 0 { format!(" (pid {})", info.pid) } else { String::new() };
                println!("{} {key}{pid}", style.ok(verb));
            }
            Err(e) if e.code == CODE_ALREADY_IN_TARGET_STATE => println!("{} {e}", style.dim("skip")),
            Err(e) => {
                failed = true;
                let hint = if e.code == CODE_PROCESS_READONLY { " (readonly = true in process.toml)" } else { "" };
                eprintln!("{} {key}: {e}{hint}", style.err("error"));
            }
        }
    }
    Ok(if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

pub fn check(path: &Path) -> Result<ExitCode> {
    let cfg = config::load(path)?;
    println!(
        "{}: ok ({} process{}, socket {})",
        path.display(),
        cfg.processes.len(),
        if cfg.processes.len() == 1 { "" } else { "es" },
        cfg.core.sock.display()
    );
    Ok(ExitCode::SUCCESS)
}

fn print_table(processes: &[ProcessInfo]) {
    let style = Style::detect();
    let header = ["KEY", "STATE", "PID", "UPTIME", "MEM", "CPU", "LAST EXIT"];
    let rows: Vec<[String; 7]> = processes
        .iter()
        .map(|p| {
            let running = p.started_at.is_some();
            let dash = || "-".to_owned();
            [
                if p.readonly { format!("{} (ro)", p.key) } else { p.key.clone() },
                p.state.to_string(),
                if p.pid != 0 { p.pid.to_string() } else { dash() },
                if running { human_duration(p.uptime_seconds) } else { dash() },
                if running { human_bytes(p.rss_bytes) } else { dash() },
                if running { format!("{:.1}%", p.cpu_percent) } else { dash() },
                p.exit_code.map(|c| exit_text(c, &p.exit_signal)).unwrap_or_else(dash),
            ]
        })
        .collect();

    let mut widths = header.map(|h| h.chars().count());
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(s.chars().count())));

    let line: Vec<String> = header.iter().zip(widths).map(|(h, w)| pad(h, w)).collect();
    println!("{}", style.dim(line.join("  ").trim_end()));
    for (row, p) in rows.iter().zip(processes) {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .enumerate()
            // Pad before coloring, so escape codes don't break alignment.
            .map(|(i, (cell, w))| if i == 1 { style.state(p.state, &pad(cell, w)) } else { pad(cell, w) })
            .collect();
        println!("{}", cells.join("  ").trim_end());
    }
}

fn exit_text(code: i32, signal: &str) -> String {
    if signal.is_empty() { format!("code {code}") } else { format!("signal {signal}") }
}

fn human_duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s}s"),
        (0, _, _) => format!("{h}h{m}m"),
        _ => format!("{d}d{h}h"),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// ANSI colors only on a terminal, and never with NO_COLOR set.
struct Style {
    color: bool,
}

impl Style {
    fn detect() -> Self {
        Self { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_owned() }
    }

    fn state(&self, state: State, s: &str) -> String {
        match state {
            State::Running => self.paint("32", s),
            State::Starting | State::Stopping => self.paint("33", s),
            State::Crashed | State::Failed => self.paint("31", s),
            State::Stopped | State::Exited => self.paint("2", s),
        }
    }

    fn ok(&self, s: &str) -> String {
        self.paint("32", s)
    }

    fn err(&self, s: &str) -> String {
        self.paint("31", s)
    }

    fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}
