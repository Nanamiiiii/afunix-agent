use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use tracing_subscriber::EnvFilter;

use crate::control::{ControlError, send_request, wait_for_ready};
use crate::control_protocol::{ControlRequest, ControlResponse};
use crate::daemon;
use crate::model::{BackendKind, DaemonConfig, DaemonStatus};
use crate::platform::{SingleInstanceGuard, spawn_detached_current_exe};

const START_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(35);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Parser)]
#[command(name = "afunix-agent", version, about)]
pub struct Cli {
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start(DaemonOptions),
    Serve(DaemonOptions),
    Stop,
    Restart,
    Status {
        #[arg(long)]
        json: bool,
    },
    Socket,
}

#[derive(Debug, Clone, Args)]
struct DaemonOptions {
    #[arg(long, default_value_t = BackendKind::Pageant)]
    backend: BackendKind,

    #[arg(long)]
    socket: Option<PathBuf>,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose)?;

    match cli.command {
        Command::Start(options) => start(config_from_options(options)?, cli.verbose),
        Command::Serve(options) => daemon::run(config_from_options(options)?),
        Command::Stop => stop(),
        Command::Restart => restart(cli.verbose),
        Command::Status { json } => status(json),
        Command::Socket => socket(),
    }
}

fn init_logging(verbose: u8) -> Result<()> {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|error| anyhow!("failed to initialize logging: {error}"))
}

fn config_from_options(options: DaemonOptions) -> Result<DaemonConfig> {
    let socket_path = resolve_socket_path(options.socket)?;
    Ok(DaemonConfig::new(options.backend, socket_path))
}

fn resolve_socket_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) if starts_with_home_component(&path) => expand_home(&path, &home_directory()?),
        Some(path) => path,
        None => home_directory()?.join(".ssh").join("ssh-agent.sock"),
    };
    std::path::absolute(&path)
        .with_context(|| format!("failed to resolve socket path {}", path.display()))
}

fn home_directory() -> Result<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("USERPROFILE is not set"))
}

fn starts_with_home_component(path: &Path) -> bool {
    matches!(path.components().next(), Some(Component::Normal(component)) if component == "~")
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    let mut components = path.components();
    if starts_with_home_component(path) {
        components.next();
        let mut expanded = home.to_path_buf();
        expanded.extend(components);
        expanded
    } else {
        path.to_path_buf()
    }
}

fn start(config: DaemonConfig, verbose: u8) -> Result<()> {
    if daemon_is_running()? || daemon_instance_exists()? {
        return Err(anyhow!("daemon is already running"));
    }

    let mut args = Vec::<OsString>::new();
    for _ in 0..verbose {
        args.push(OsString::from("-v"));
    }
    args.push(OsString::from("serve"));
    args.push(OsString::from("--backend"));
    args.push(OsString::from(config.backend.as_str()));
    args.push(OsString::from("--socket"));
    args.push(config.socket_path.as_os_str().to_owned());

    let mut child =
        spawn_detached_current_exe(args).context("failed to start the daemon process")?;
    let child_pid = child.id();
    let status = match wait_for_ready(START_TIMEOUT, Some(&mut child)) {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            if daemon_instance_exists()? {
                return Err(anyhow!("daemon is already running"));
            }
            return Err(anyhow::Error::new(error).context("daemon startup failed"));
        }
    };

    if status.pid != child_pid {
        let _ = child.kill();
        let _ = child.wait();
        return Err(anyhow!("daemon is already running"));
    }

    Ok(())
}

fn stop() -> Result<()> {
    request_shutdown()?;
    wait_for_stop(STOP_TIMEOUT)
}

fn restart(verbose: u8) -> Result<()> {
    let current = request_status()?;
    let config = DaemonConfig::new(current.backend, current.socket_path);
    request_shutdown()?;
    wait_for_stop(STOP_TIMEOUT)?;
    start(config, verbose)
}

fn status(json: bool) -> Result<()> {
    let status = request_status()?;
    if json {
        let output = JsonStatus::from(&status);
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("running: true");
        println!("pid: {}", status.pid);
        println!("backend: {}", status.backend);
        println!("socket: {}", status.socket_path.display());
        println!("uptime: {}", format_uptime(status.uptime_seconds));
        println!("connections: {}", status.active_connections);
    }
    Ok(())
}

fn socket() -> Result<()> {
    match request_daemon(&ControlRequest::GetSocket)? {
        ControlResponse::Socket(path) => {
            println!("{}", path.display());
            Ok(())
        }
        ControlResponse::Error(message) => Err(anyhow!(message)),
        response => Err(anyhow!("unexpected control response: {response:?}")),
    }
}

fn request_status() -> Result<DaemonStatus> {
    match request_daemon(&ControlRequest::Status)? {
        ControlResponse::Status(status) => Ok(status),
        ControlResponse::Error(message) => Err(anyhow!(message)),
        response => Err(anyhow!("unexpected control response: {response:?}")),
    }
}

fn request_shutdown() -> Result<()> {
    match request_daemon(&ControlRequest::Shutdown)? {
        ControlResponse::Ok => Ok(()),
        ControlResponse::Error(message) => Err(anyhow!(message)),
        response => Err(anyhow!("unexpected control response: {response:?}")),
    }
}

fn request_daemon(request: &ControlRequest) -> Result<ControlResponse> {
    send_request(request).map_err(|error| match error {
        ControlError::OpenPipe(source) if source.kind() == io::ErrorKind::NotFound => {
            anyhow!("daemon is not running")
        }
        other => anyhow::Error::new(other),
    })
}

fn daemon_is_running() -> Result<bool> {
    match send_request(&ControlRequest::Ping) {
        Ok(_) => Ok(true),
        Err(ControlError::OpenPipe(error)) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(ControlError::OpenPipe(error)) => {
            if daemon_instance_exists()? {
                Ok(true)
            } else {
                Err(ControlError::OpenPipe(error).into())
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn daemon_instance_exists() -> Result<bool> {
    match SingleInstanceGuard::acquire() {
        Ok(guard) => {
            drop(guard);
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(true),
        Err(error) => {
            Err(anyhow::Error::new(error).context("failed to inspect the singleton mutex"))
        }
    }
}

fn wait_for_stop(timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        match SingleInstanceGuard::acquire() {
            Ok(guard) => {
                drop(guard);
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context("failed to verify that the daemon stopped")
                );
            }
        }

        if started.elapsed() >= timeout {
            return Err(anyhow!("daemon did not stop within {timeout:?}"));
        }
        thread::sleep(STOP_POLL_INTERVAL);
    }
}

fn format_uptime(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = seconds % 3600 / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

#[derive(Serialize)]
struct JsonStatus<'a> {
    running: bool,
    pid: u32,
    backend: BackendKind,
    socket: &'a Path,
    uptime_seconds: u64,
    active_connections: usize,
}

impl<'a> From<&'a DaemonStatus> for JsonStatus<'a> {
    fn from(status: &'a DaemonStatus) -> Self {
        Self {
            running: true,
            pid: status.pid,
            backend: status.backend,
            socket: &status.socket_path,
            uptime_seconds: status.uptime_seconds,
            active_connections: status.active_connections,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{expand_home, format_uptime};

    #[test]
    fn expands_only_a_leading_home_component() {
        let home = Path::new(r"C:\Users\test");
        assert_eq!(
            expand_home(Path::new(r"~\.ssh\agent.sock"), home),
            PathBuf::from(r"C:\Users\test\.ssh\agent.sock")
        );
        assert_eq!(expand_home(Path::new("~"), home), home);
        assert_eq!(
            expand_home(Path::new(r"~other\agent.sock"), home),
            PathBuf::from(r"~other\agent.sock")
        );
    }

    #[test]
    fn formats_uptime_without_wrapping_hours() {
        assert_eq!(format_uptime(0), "00:00:00");
        assert_eq!(format_uptime(3_661), "01:01:01");
        assert_eq!(format_uptime(90_061), "25:01:01");
    }
}
