use anyhow::Result;

#[cfg(windows)]
pub mod pageant;
#[cfg(windows)]
pub use pageant::PageantBackend;

use crate::model::BackendKind;

pub trait AgentBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    fn transact(&self, request_frame: &[u8]) -> Result<Vec<u8>>;
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::AgentBackend;
    use crate::model::BackendKind;

    struct EchoBackend;

    impl AgentBackend for EchoBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Pageant
        }

        fn transact(&self, request_frame: &[u8]) -> Result<Vec<u8>> {
            Ok(request_frame.to_vec())
        }
    }

    #[test]
    fn backend_receives_and_returns_full_wire_frames() {
        let backend = EchoBackend;
        let frame = [0, 0, 0, 2, 10, 20];

        assert_eq!(backend.name(), "pageant");
        assert_eq!(backend.transact(&frame).unwrap(), frame);
    }
}
