use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    #[default]
    Pageant,
}

impl BackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pageant => "pageant",
        }
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for BackendKind {
    type Err = ParseBackendKindError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pageant" => Ok(Self::Pageant),
            _ => Err(ParseBackendKindError(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unsupported backend: {0}")]
pub struct ParseBackendKindError(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub backend: BackendKind,
    pub socket_path: PathBuf,
}

impl DaemonConfig {
    pub fn new(backend: BackendKind, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            backend,
            socket_path: socket_path.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub pid: u32,
    pub backend: BackendKind,
    pub socket_path: PathBuf,
    pub uptime_seconds: u64,
    pub active_connections: usize,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{BackendKind, DaemonConfig, DaemonStatus};

    #[test]
    fn backend_kind_has_stable_text_forms() {
        assert_eq!(BackendKind::Pageant.as_str(), "pageant");
        assert_eq!(BackendKind::Pageant.to_string(), "pageant");
        assert_eq!("pageant".parse(), Ok(BackendKind::Pageant));
        assert!("unknown".parse::<BackendKind>().is_err());
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = DaemonConfig::new(BackendKind::Pageant, PathBuf::from("agent.sock"));
        let json = serde_json::to_string(&config).unwrap();
        let decoded: DaemonConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, config);
        assert!(json.contains("\"pageant\""));
    }

    #[test]
    fn status_round_trips_through_json() {
        let status = DaemonStatus {
            pid: 42,
            backend: BackendKind::Pageant,
            socket_path: PathBuf::from("agent.sock"),
            uptime_seconds: 17,
            active_connections: 3,
        };
        let json = serde_json::to_string(&status).unwrap();
        let decoded: DaemonStatus = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, status);
    }
}
