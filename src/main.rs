mod cli;
mod client;
mod config;
mod duration;
mod logging;
mod rpc;
mod schedule;
mod supervisor;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use cli::{Cli, Command, RunArgs};
use client::Action;
use logging::LogWriter;
use supervisor::Manager;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => run(cli.run),
        Some(Command::Run(args)) => run(args),
        Some(Command::List { conn, json }) => client::list(&conn, json),
        Some(Command::Status { key, conn, json }) => client::status(&conn, &key, json),
        Some(Command::Start(target)) => client::control(Action::Start, &target),
        Some(Command::Stop(target)) => client::control(Action::Stop, &target),
        Some(Command::Check { config }) => client::check(&config),
    };
    result.unwrap_or_else(|e| {
        eprintln!("error: {e:#}");
        ExitCode::FAILURE
    })
}

fn run(args: RunArgs) -> Result<ExitCode> {
    let cfg = config::load(&args.config).context("config")?;
    tokio::runtime::Runtime::new()?.block_on(daemon(cfg, args))?;
    Ok(ExitCode::SUCCESS)
}

async fn daemon(cfg: config::Config, args: RunArgs) -> Result<()> {
    let console = Arc::new(LogWriter::stdout());
    console.line(
        "core",
        &format!("procd {} starting (config={}, readonly={})", env!("CARGO_PKG_VERSION"), args.config.display(), args.readonly),
    );
    if let Some(envfile) = &cfg.core.envfile {
        console.line("core", &format!("envfile loaded: {} ({} vars)", envfile.display(), cfg.core.env_vars.len()));
    }

    let manager = Manager::new(&cfg, Arc::clone(&console))?;
    // Bind before autostart: if another instance owns the socket, fail
    // without having spawned any children.
    let listener = rpc::bind(&cfg.core.sock, cfg.core.socket_perm).context("rpc server")?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    for err in manager.autostart() {
        console.line("core", &format!("autostart error: {err}"));
    }
    manager.start_schedulers();

    let shutdown = CancellationToken::new();
    let server = Arc::new(rpc::Server { manager: Arc::clone(&manager), readonly: args.readonly, console: Arc::clone(&console) });
    let serving = tokio::spawn(server.serve(listener, shutdown.clone()));
    console.line("core", &format!("listening on {} (readonly={})", cfg.core.sock.display(), args.readonly));

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }

    console.line("core", "shutdown signal received, stopping...");
    shutdown.cancel();
    let _ = serving.await;
    let _ = std::fs::remove_file(&cfg.core.sock);
    manager.stop_all().await;
    console.line("core", "shutdown complete");
    Ok(())
}
