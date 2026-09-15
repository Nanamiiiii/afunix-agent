use std::io::{Read, Write};

use thiserror::Error;

use crate::backend::AgentBackend;
use crate::framing::{FrameError, read_frame, write_frame};

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("failed to read an SSH agent frame")]
    ReadFrame(#[source] FrameError),
    #[error("agent backend transaction failed")]
    Backend(#[source] anyhow::Error),
    #[error("failed to write an SSH agent frame")]
    WriteFrame(#[source] FrameError),
}

pub fn proxy_connection<S, B>(stream: &mut S, backend: &B) -> Result<(), ProxyError>
where
    S: Read + Write,
    B: AgentBackend + ?Sized,
{
    while let Some(request_frame) = read_frame(stream).map_err(ProxyError::ReadFrame)? {
        let response_frame = backend
            .transact(&request_frame)
            .map_err(ProxyError::Backend)?;
        write_frame(stream, &response_frame).map_err(ProxyError::WriteFrame)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read, Write};
    use std::sync::Mutex;

    use anyhow::{Result, anyhow};

    use super::{ProxyError, proxy_connection};
    use crate::backend::AgentBackend;
    use crate::framing::{FrameError, encode_frame};
    use crate::model::BackendKind;

    struct MemoryStream {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl MemoryStream {
        fn new(input: Vec<u8>) -> Self {
            Self {
                input: Cursor::new(input),
                output: Vec::new(),
            }
        }
    }

    impl Read for MemoryStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for MemoryStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct RecordingBackend {
        requests: Mutex<Vec<Vec<u8>>>,
    }

    impl AgentBackend for RecordingBackend {
        fn kind(&self) -> BackendKind {
            BackendKind::Pageant
        }

        fn transact(&self, request_frame: &[u8]) -> Result<Vec<u8>> {
            self.requests.lock().unwrap().push(request_frame.to_vec());
            encode_frame(b"response").map_err(Into::into)
        }
    }

    #[test]
    fn proxies_multiple_full_wire_frames_until_clean_eof() {
        let first = encode_frame(b"first").unwrap();
        let second = encode_frame(b"second").unwrap();
        let mut input = first.clone();
        input.extend_from_slice(&second);
        let mut stream = MemoryStream::new(input);
        let backend = RecordingBackend {
            requests: Mutex::new(Vec::new()),
        };

        proxy_connection(&mut stream, &backend).unwrap();

        assert_eq!(*backend.requests.lock().unwrap(), vec![first, second]);
        let expected = encode_frame(b"response").unwrap().repeat(2);
        assert_eq!(stream.output, expected);
    }

    #[test]
    fn malformed_backend_response_is_rejected() {
        struct MalformedBackend;

        impl AgentBackend for MalformedBackend {
            fn kind(&self) -> BackendKind {
                BackendKind::Pageant
            }

            fn transact(&self, _request_frame: &[u8]) -> Result<Vec<u8>> {
                Ok(vec![0, 0, 0, 2, 1])
            }
        }

        let mut stream = MemoryStream::new(encode_frame(b"request").unwrap());
        let error = proxy_connection(&mut stream, &MalformedBackend).unwrap_err();

        assert!(matches!(
            error,
            ProxyError::WriteFrame(FrameError::TruncatedPayload { .. })
        ));
        assert!(stream.output.is_empty());
    }

    #[test]
    fn backend_errors_stop_the_connection() {
        struct FailingBackend;

        impl AgentBackend for FailingBackend {
            fn kind(&self) -> BackendKind {
                BackendKind::Pageant
            }

            fn transact(&self, _request_frame: &[u8]) -> Result<Vec<u8>> {
                Err(anyhow!("unavailable"))
            }
        }

        let mut stream = MemoryStream::new(encode_frame(b"request").unwrap());

        assert!(matches!(
            proxy_connection(&mut stream, &FailingBackend),
            Err(ProxyError::Backend(_))
        ));
    }

    #[test]
    fn truncated_input_is_not_treated_as_clean_eof() {
        let mut stream = MemoryStream::new(vec![0, 0]);
        let backend = RecordingBackend {
            requests: Mutex::new(Vec::new()),
        };

        assert!(matches!(
            proxy_connection(&mut stream, &backend),
            Err(ProxyError::ReadFrame(FrameError::TruncatedHeader { .. }))
        ));
    }
}
