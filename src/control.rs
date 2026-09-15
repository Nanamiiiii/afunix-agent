use std::io::{self, BufReader, Read, Write};
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::control_protocol::{
    ControlCodecError, ControlRequest, ControlResponse, read_request, read_response, write_request,
    write_response,
};
use crate::lifecycle::Lifecycle;
use crate::model::DaemonStatus;
use crate::platform::{NamedPipeServer, control_pipe_name, open_named_pipe_client};

const READY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("failed to resolve the control pipe name")]
    PipeName(#[source] io::Error),
    #[error("failed to open the control pipe")]
    OpenPipe(#[source] io::Error),
    #[error("failed to create the control pipe server")]
    CreateServer(#[source] io::Error),
    #[error("failed to accept a control pipe connection")]
    Accept(#[source] io::Error),
    #[error("control protocol failed")]
    Protocol(#[from] ControlCodecError),
    #[error("failed to flush the control pipe")]
    Flush(#[source] io::Error),
    #[error("the control server closed the connection without a response")]
    EmptyResponse,
    #[error("unexpected control response: {0:?}")]
    UnexpectedResponse(ControlResponse),
    #[error("the daemon did not become ready within {0:?}")]
    ReadyTimeout(Duration),
    #[error("the daemon process exited before becoming ready: {0}")]
    ChildExited(ExitStatus),
    #[error("failed to query the daemon process state")]
    ChildStatus(#[source] io::Error),
    #[error("failed to spawn a control thread")]
    SpawnThread(#[source] io::Error),
    #[error("control request timed out after {0:?}")]
    RequestTimeout(Duration),
    #[error("control client thread stopped without a result")]
    ClientThreadStopped,
}

pub type ControlServerHandle = JoinHandle<Result<(), ControlError>>;

pub fn send_request(request: &ControlRequest) -> Result<ControlResponse, ControlError> {
    let request = request.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("afunix-agent-control-client".to_owned())
        .spawn(move || {
            let _ = sender.send(send_request_blocking(&request));
        })
        .map_err(ControlError::SpawnThread)?;

    match receiver.recv_timeout(REQUEST_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(ControlError::RequestTimeout(REQUEST_TIMEOUT)),
        Err(RecvTimeoutError::Disconnected) => Err(ControlError::ClientThreadStopped),
    }
}

fn send_request_blocking(request: &ControlRequest) -> Result<ControlResponse, ControlError> {
    let pipe_name = control_pipe_name().map_err(ControlError::PipeName)?;
    let mut pipe = open_named_pipe_client(&pipe_name).map_err(ControlError::OpenPipe)?;
    write_request(&mut pipe, request)?;
    pipe.flush().map_err(ControlError::Flush)?;

    let mut reader = BufReader::new(pipe);
    read_response(&mut reader)?.ok_or(ControlError::EmptyResponse)
}

pub fn run_control_server(lifecycle: Lifecycle) -> Result<(), ControlError> {
    let server = NamedPipeServer::for_current_user().map_err(ControlError::CreateServer)?;
    run_server_loop(server, lifecycle)
}

pub fn spawn_control_server(lifecycle: Lifecycle) -> Result<ControlServerHandle, ControlError> {
    let server = NamedPipeServer::for_current_user().map_err(ControlError::CreateServer)?;
    thread::Builder::new()
        .name("afunix-agent-control".to_owned())
        .spawn(move || run_server_loop(server, lifecycle))
        .map_err(ControlError::SpawnThread)
}

pub fn wake_control_server() -> Result<(), ControlError> {
    match send_request(&ControlRequest::Ping)? {
        ControlResponse::Pong => Ok(()),
        response => Err(ControlError::UnexpectedResponse(response)),
    }
}

pub fn wait_for_ready(
    timeout: Duration,
    mut child: Option<&mut Child>,
) -> Result<DaemonStatus, ControlError> {
    let started = Instant::now();

    loop {
        if let Some(child) = child.as_deref_mut()
            && let Some(status) = child.try_wait().map_err(ControlError::ChildStatus)?
        {
            return Err(ControlError::ChildExited(status));
        }

        if let Ok(response) = send_request(&ControlRequest::Status) {
            return match response {
                ControlResponse::Status(status) => Ok(status),
                response => Err(ControlError::UnexpectedResponse(response)),
            };
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(ControlError::ReadyTimeout(timeout));
        }
        thread::sleep(READY_POLL_INTERVAL.min(timeout - elapsed));
    }
}

fn run_server_loop(mut server: NamedPipeServer, lifecycle: Lifecycle) -> Result<(), ControlError> {
    while !lifecycle.is_shutdown_requested() {
        let mut pipe = server.accept().map_err(ControlError::Accept)?;
        let connection_lifecycle = lifecycle.clone();
        thread::Builder::new()
            .name("afunix-agent-control-connection".to_owned())
            .spawn(move || {
                if let Err(error) = handle_connection(&mut pipe, &connection_lifecycle) {
                    tracing::warn!(error = %error, "control connection failed");
                }
            })
            .map_err(ControlError::SpawnThread)?;
    }

    Ok(())
}

fn handle_connection<S>(stream: &mut S, lifecycle: &Lifecycle) -> Result<(), ControlError>
where
    S: Read + Write,
{
    let request = {
        let mut reader = BufReader::new(&mut *stream);
        read_request(&mut reader)
    };

    let request = match request {
        Ok(Some(request)) => request,
        Ok(None) => {
            write_error(stream, "empty control request")?;
            return Ok(());
        }
        Err(error) => {
            write_error(stream, &error.to_string())?;
            return Ok(());
        }
    };

    let response = match request {
        ControlRequest::Ping => ControlResponse::Pong,
        ControlRequest::Status => ControlResponse::Status(lifecycle.status()),
        ControlRequest::GetSocket => ControlResponse::Socket(lifecycle.status().socket_path),
        ControlRequest::Shutdown => {
            write_and_flush(stream, &ControlResponse::Ok)?;
            lifecycle.request_shutdown();
            return Ok(());
        }
    };

    write_and_flush(stream, &response)
}

fn write_error<S>(stream: &mut S, message: &str) -> Result<(), ControlError>
where
    S: Write,
{
    write_and_flush(stream, &ControlResponse::Error(message.to_owned()))
}

fn write_and_flush<S>(stream: &mut S, response: &ControlResponse) -> Result<(), ControlError>
where
    S: Write,
{
    write_response(stream, response)?;
    stream.flush().map_err(ControlError::Flush)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read, Write};
    use std::path::PathBuf;

    use super::handle_connection;
    use crate::control_protocol::{ControlRequest, ControlResponse, read_response, write_request};
    use crate::lifecycle::Lifecycle;
    use crate::model::{BackendKind, DaemonConfig};

    struct MemoryPipe {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
        lifecycle: Lifecycle,
        shutdown_at_flush: Option<bool>,
    }

    impl MemoryPipe {
        fn new(input: Vec<u8>, lifecycle: Lifecycle) -> Self {
            Self {
                input: Cursor::new(input),
                output: Vec::new(),
                lifecycle,
                shutdown_at_flush: None,
            }
        }

        fn response(&self) -> ControlResponse {
            let mut reader = io::BufReader::new(Cursor::new(&self.output));
            read_response(&mut reader).unwrap().unwrap()
        }
    }

    impl Read for MemoryPipe {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.input.read(buffer)
        }
    }

    impl Write for MemoryPipe {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.shutdown_at_flush = Some(self.lifecycle.is_shutdown_requested());
            Ok(())
        }
    }

    fn lifecycle() -> Lifecycle {
        Lifecycle::with_pid(
            DaemonConfig::new(BackendKind::Pageant, PathBuf::from("agent.sock")),
            42,
        )
    }

    fn encoded(request: ControlRequest) -> Vec<u8> {
        let mut input = Vec::new();
        write_request(&mut input, &request).unwrap();
        input
    }

    #[test]
    fn serves_each_request_type() {
        let lifecycle = lifecycle();

        for (request, expected) in [
            (ControlRequest::Ping, ControlResponse::Pong),
            (
                ControlRequest::GetSocket,
                ControlResponse::Socket(PathBuf::from("agent.sock")),
            ),
        ] {
            let mut pipe = MemoryPipe::new(encoded(request), lifecycle.clone());
            handle_connection(&mut pipe, &lifecycle).unwrap();
            assert_eq!(pipe.response(), expected);
        }

        let mut pipe = MemoryPipe::new(encoded(ControlRequest::Status), lifecycle.clone());
        handle_connection(&mut pipe, &lifecycle).unwrap();
        let ControlResponse::Status(status) = pipe.response() else {
            panic!("expected status response");
        };
        assert_eq!(status.pid, 42);
    }

    #[test]
    fn shutdown_is_acknowledged_and_flushed_before_it_is_requested() {
        let lifecycle = lifecycle();
        let mut pipe = MemoryPipe::new(encoded(ControlRequest::Shutdown), lifecycle.clone());

        handle_connection(&mut pipe, &lifecycle).unwrap();

        assert_eq!(pipe.response(), ControlResponse::Ok);
        assert_eq!(pipe.shutdown_at_flush, Some(false));
        assert!(lifecycle.is_shutdown_requested());
    }

    #[test]
    fn malformed_and_empty_requests_receive_errors() {
        let lifecycle = lifecycle();

        for input in [b"not-json\n".to_vec(), Vec::new()] {
            let mut pipe = MemoryPipe::new(input, lifecycle.clone());
            handle_connection(&mut pipe, &lifecycle).unwrap();
            assert!(matches!(pipe.response(), ControlResponse::Error(_)));
        }
    }
}
