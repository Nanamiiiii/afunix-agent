use std::collections::HashMap;
use std::io;
use std::net::Shutdown;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use socket2::Socket;

use crate::backend::{AgentBackend, PageantBackend};
use crate::control::{spawn_control_server, wake_control_server};
use crate::lifecycle::Lifecycle;
use crate::model::{BackendKind, DaemonConfig};
use crate::platform::SingleInstanceGuard;
use crate::proxy::proxy_connection;
use crate::unix_socket::UnixListener;

const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

pub fn run(config: DaemonConfig) -> Result<()> {
    let _instance = acquire_instance()?;
    let listener = UnixListener::bind(&config.socket_path).with_context(|| {
        format!(
            "failed to create AF_UNIX listener at {}",
            config.socket_path.display()
        )
    })?;
    let lifecycle = Lifecycle::new(config.clone());
    install_ctrl_c_handler(lifecycle.clone())?;
    let control = spawn_control_server(lifecycle.clone())?;
    let backend = create_backend(config.backend);
    let active_sockets = Arc::new(ActiveSockets::default());
    let mut workers = Vec::new();
    let mut failure = None;

    tracing::info!(
        backend = %config.backend,
        socket = %config.socket_path.display(),
        "daemon started"
    );

    while !lifecycle.is_shutdown_requested() {
        reap_workers(&mut workers);

        if control.is_finished() {
            failure = Some(anyhow!("control server stopped unexpectedly"));
            lifecycle.request_shutdown();
            break;
        }

        match listener.accept() {
            Ok(stream) => match spawn_worker(
                stream,
                Arc::clone(&backend),
                lifecycle.clone(),
                Arc::clone(&active_sockets),
            ) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    failure = Some(
                        anyhow::Error::new(error).context("failed to spawn connection worker"),
                    );
                    lifecycle.request_shutdown();
                }
            },
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL_INTERVAL);
            }
            Err(error) => {
                failure = Some(anyhow::Error::new(error).context("AF_UNIX accept failed"));
                lifecycle.request_shutdown();
            }
        }
    }

    lifecycle.request_shutdown();
    let _ = wake_control_server();
    active_sockets.shutdown_all();
    join_workers(workers, &mut failure);

    match control.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => record_failure(&mut failure, anyhow::Error::new(error)),
        Err(_) => record_failure(&mut failure, anyhow!("control server thread panicked")),
    }

    if let Err(error) = listener.close() {
        record_failure(
            &mut failure,
            anyhow::Error::new(error).context("AF_UNIX endpoint cleanup failed"),
        );
    }
    tracing::info!("daemon stopped");

    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn acquire_instance() -> Result<SingleInstanceGuard> {
    SingleInstanceGuard::acquire().map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            anyhow!("daemon is already running")
        } else {
            anyhow::Error::new(error).context("failed to acquire the singleton mutex")
        }
    })
}

fn install_ctrl_c_handler(lifecycle: Lifecycle) -> Result<()> {
    ctrlc::set_handler(move || {
        lifecycle.request_shutdown();
    })
    .context("failed to install the Ctrl-C handler")
}

fn create_backend(kind: BackendKind) -> Arc<dyn AgentBackend> {
    match kind {
        BackendKind::Pageant => Arc::new(PageantBackend),
    }
}

fn spawn_worker(
    mut stream: Socket,
    backend: Arc<dyn AgentBackend>,
    lifecycle: Lifecycle,
    active_sockets: Arc<ActiveSockets>,
) -> io::Result<JoinHandle<()>> {
    let registration = active_sockets.register(&stream)?;
    let connection = lifecycle.track_connection();

    thread::Builder::new()
        .name("afunix-agent-connection".to_owned())
        .spawn(move || {
            let _registration = registration;
            let _connection = connection;
            tracing::debug!("connection opened");
            if let Err(error) = proxy_connection(&mut stream, backend.as_ref()) {
                tracing::warn!(error = %error, "connection failed");
            }
            let _ = stream.shutdown(Shutdown::Both);
            tracing::debug!("connection closed");
        })
}

fn reap_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < workers.len() {
        if workers[index].is_finished() {
            let worker = workers.swap_remove(index);
            if worker.join().is_err() {
                tracing::error!("connection worker panicked");
            }
        } else {
            index += 1;
        }
    }
}

fn join_workers(workers: Vec<JoinHandle<()>>, failure: &mut Option<anyhow::Error>) {
    for worker in workers {
        if worker.join().is_err() {
            record_failure(failure, anyhow!("connection worker panicked"));
        }
    }
}

fn record_failure(target: &mut Option<anyhow::Error>, error: anyhow::Error) {
    if target.is_none() {
        *target = Some(error);
    } else {
        tracing::error!(error = %error, "additional shutdown failure");
    }
}

#[derive(Default)]
struct ActiveSockets {
    next_id: AtomicU64,
    sockets: Mutex<HashMap<u64, Socket>>,
}

impl ActiveSockets {
    fn register(self: &Arc<Self>, stream: &Socket) -> io::Result<SocketRegistration> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let shutdown_socket = stream.try_clone()?;
        self.sockets.lock().unwrap().insert(id, shutdown_socket);
        Ok(SocketRegistration {
            id,
            sockets: Arc::clone(self),
        })
    }

    fn shutdown_all(&self) {
        for socket in self.sockets.lock().unwrap().values() {
            if let Err(error) = socket.shutdown(Shutdown::Both)
                && error.kind() != io::ErrorKind::NotConnected
            {
                tracing::debug!(error = %error, "connection shutdown failed");
            }
        }
    }
}

struct SocketRegistration {
    id: u64,
    sockets: Arc<ActiveSockets>,
}

impl Drop for SocketRegistration {
    fn drop(&mut self) {
        self.sockets.sockets.lock().unwrap().remove(&self.id);
    }
}
