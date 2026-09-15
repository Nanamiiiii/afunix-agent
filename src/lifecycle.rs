use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use crate::model::{DaemonConfig, DaemonStatus};

#[derive(Clone)]
pub struct Lifecycle {
    inner: Arc<LifecycleInner>,
}

struct LifecycleInner {
    config: DaemonConfig,
    pid: u32,
    started_at: Instant,
    shutdown_requested: AtomicBool,
    active_connections: AtomicUsize,
}

impl Lifecycle {
    pub fn new(config: DaemonConfig) -> Self {
        Self::with_pid(config, std::process::id())
    }

    pub fn with_pid(config: DaemonConfig, pid: u32) -> Self {
        Self {
            inner: Arc::new(LifecycleInner {
                config,
                pid,
                started_at: Instant::now(),
                shutdown_requested: AtomicBool::new(false),
                active_connections: AtomicUsize::new(0),
            }),
        }
    }

    pub fn request_shutdown(&self) -> bool {
        !self.inner.shutdown_requested.swap(true, Ordering::AcqRel)
    }

    pub fn is_shutdown_requested(&self) -> bool {
        self.inner.shutdown_requested.load(Ordering::Acquire)
    }

    pub fn track_connection(&self) -> ActiveConnectionGuard {
        self.inner.active_connections.fetch_add(1, Ordering::AcqRel);
        ActiveConnectionGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    pub fn active_connections(&self) -> usize {
        self.inner.active_connections.load(Ordering::Acquire)
    }

    pub fn status(&self) -> DaemonStatus {
        DaemonStatus {
            pid: self.inner.pid,
            backend: self.inner.config.backend,
            socket_path: self.inner.config.socket_path.clone(),
            uptime_seconds: self.inner.started_at.elapsed().as_secs(),
            active_connections: self.active_connections(),
        }
    }
}

#[must_use = "the connection guard must be held while the connection is active"]
pub struct ActiveConnectionGuard {
    inner: Arc<LifecycleInner>,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        let previous = self.inner.active_connections.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "active connection count underflow");
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::thread;

    use super::Lifecycle;
    use crate::model::{BackendKind, DaemonConfig};

    fn lifecycle() -> Lifecycle {
        Lifecycle::with_pid(
            DaemonConfig::new(BackendKind::Pageant, PathBuf::from("agent.sock")),
            123,
        )
    }

    #[test]
    fn shutdown_transition_is_atomic_and_idempotent() {
        let lifecycle = lifecycle();
        assert!(!lifecycle.is_shutdown_requested());

        assert!(lifecycle.request_shutdown());
        assert!(lifecycle.is_shutdown_requested());
        assert!(!lifecycle.request_shutdown());
    }

    #[test]
    fn connection_guard_tracks_raii_lifetime() {
        let lifecycle = lifecycle();
        let first = lifecycle.track_connection();
        let second = lifecycle.track_connection();
        assert_eq!(lifecycle.active_connections(), 2);
        assert_eq!(lifecycle.status().active_connections, 2);

        drop(first);
        assert_eq!(lifecycle.active_connections(), 1);
        drop(second);
        assert_eq!(lifecycle.active_connections(), 0);
    }

    #[test]
    fn guards_keep_shared_state_alive() {
        let guard = lifecycle().track_connection();
        drop(guard);
    }

    #[test]
    fn concurrent_shutdown_has_one_winner() {
        let lifecycle = Arc::new(lifecycle());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let lifecycle = Arc::clone(&lifecycle);
                thread::spawn(move || lifecycle.request_shutdown())
            })
            .collect();

        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn status_reports_configuration_pid_and_uptime() {
        let lifecycle = lifecycle();
        let status = lifecycle.status();

        assert_eq!(status.pid, 123);
        assert_eq!(status.backend, BackendKind::Pageant);
        assert_eq!(status.socket_path, PathBuf::from("agent.sock"));
        assert_eq!(status.uptime_seconds, 0);
        assert_eq!(status.active_connections, 0);
    }
}
