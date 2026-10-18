use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Lightweight process supervisor for containers.
///
/// Without a subcommand, procd runs the supervisor daemon (same as
/// `procd run`). The other subcommands talk to a running daemon over
/// its Unix socket.
#[derive(Debug, Parser)]
#[command(name = "procd", version, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(flatten)]
    pub run: RunArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the supervisor daemon (default)
    Run(RunArgs),

    /// List all processes and their state
    #[command(visible_alias = "ls")]
    List {
        #[command(flatten)]
        conn: ConnArgs,
        /// Print raw JSON instead of a table
        #[arg(short, long)]
        json: bool,
    },

    /// Show details of one process
    Status {
        /// Process key from process.toml
        key: String,
        #[command(flatten)]
        conn: ConnArgs,
        /// Print raw JSON
        #[arg(short, long)]
        json: bool,
    },

    /// Start processes
    Start(TargetArgs),

    /// Stop processes (SIGTERM, SIGKILL after shutdown_timeout)
    Stop(TargetArgs),

    /// Validate a config file and exit
    Check {
        /// Path to process.toml
        #[arg(short, long, env = "PROCD_CONFIG", default_value = "process.toml")]
        config: PathBuf,
    },
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to process.toml
    #[arg(short, long, env = "PROCD_CONFIG", default_value = "process.toml")]
    pub config: PathBuf,

    /// Read-only API: reject process.start/process.stop over the socket
    #[arg(short, long)]
    pub readonly: bool,
}

/// How a client subcommand finds the daemon's socket.
#[derive(Debug, Args)]
pub struct ConnArgs {
    /// Socket path [default: core.sock from the config, else /run/procd.sock]
    #[arg(short, long, env = "PROCD_SOCKET")]
    pub socket: Option<PathBuf>,

    /// Config file to read core.sock from
    #[arg(short, long, env = "PROCD_CONFIG", default_value = "process.toml")]
    pub config: PathBuf,
}

#[derive(Debug, Args)]
pub struct TargetArgs {
    /// Process keys from process.toml
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    pub keys: Vec<String>,

    /// All processes (stop skips readonly-protected ones)
    #[arg(short, long)]
    pub all: bool,

    #[command(flatten)]
    pub conn: ConnArgs,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_common_forms() {
        let cli = Cli::try_parse_from(["procd", "-c", "x.toml", "-r"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.run.config, PathBuf::from("x.toml"));
        assert!(cli.run.readonly);

        let cli = Cli::try_parse_from(["procd", "stop", "--all", "-s", "/tmp/s.sock"]).unwrap();
        let Some(Command::Stop(t)) = cli.command else { panic!() };
        assert!(t.all && t.keys.is_empty());
        assert_eq!(t.conn.socket, Some(PathBuf::from("/tmp/s.sock")));

        assert!(Cli::try_parse_from(["procd", "start"]).is_err(), "needs keys or --all");
        assert!(Cli::try_parse_from(["procd", "start", "a", "-a"]).is_err(), "keys conflict with --all");
    }
}
