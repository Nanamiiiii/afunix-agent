use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::DaemonStatus;

pub const MAX_CONTROL_LINE_LEN: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    Ping,
    Status,
    GetSocket,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlResponse {
    Pong,
    Status(DaemonStatus),
    Socket(PathBuf),
    Ok,
    Error(String),
}

#[derive(Debug, Error)]
pub enum ControlCodecError {
    #[error("control protocol I/O failed")]
    Io(#[source] io::Error),
    #[error("control message exceeds the {max} byte limit")]
    LineTooLong { max: usize },
    #[error("control message ended before a newline delimiter")]
    MissingNewline,
    #[error("invalid control message JSON")]
    Json(#[source] serde_json::Error),
}

pub fn read_request<R: BufRead>(
    reader: &mut R,
) -> Result<Option<ControlRequest>, ControlCodecError> {
    read_message(reader)
}

pub fn write_request<W: Write>(
    writer: &mut W,
    request: &ControlRequest,
) -> Result<(), ControlCodecError> {
    write_message(writer, request)
}

pub fn read_response<R: BufRead>(
    reader: &mut R,
) -> Result<Option<ControlResponse>, ControlCodecError> {
    read_message(reader)
}

pub fn write_response<W: Write>(
    writer: &mut W,
    response: &ControlResponse,
) -> Result<(), ControlCodecError> {
    write_message(writer, response)
}

fn read_message<R, T>(reader: &mut R) -> Result<Option<T>, ControlCodecError>
where
    R: BufRead,
    T: DeserializeOwned,
{
    let mut line = Vec::new();

    loop {
        let available = reader.fill_buf().map_err(ControlCodecError::Io)?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(ControlCodecError::MissingNewline)
            };
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            append_bounded(&mut line, &available[..newline])?;
            reader.consume(newline + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(ControlCodecError::Json);
        }

        let consumed = available.len();
        append_bounded(&mut line, available)?;
        reader.consume(consumed);
    }
}

fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), ControlCodecError>
where
    W: Write,
    T: Serialize,
{
    let encoded = serde_json::to_vec(message).map_err(ControlCodecError::Json)?;
    if encoded.len() > MAX_CONTROL_LINE_LEN {
        return Err(ControlCodecError::LineTooLong {
            max: MAX_CONTROL_LINE_LEN,
        });
    }

    writer.write_all(&encoded).map_err(ControlCodecError::Io)?;
    writer.write_all(b"\n").map_err(ControlCodecError::Io)
}

fn append_bounded(line: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ControlCodecError> {
    if line.len().saturating_add(bytes.len()) > MAX_CONTROL_LINE_LEN {
        return Err(ControlCodecError::LineTooLong {
            max: MAX_CONTROL_LINE_LEN,
        });
    }
    line.extend_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::path::PathBuf;

    use super::{
        ControlCodecError, ControlRequest, ControlResponse, MAX_CONTROL_LINE_LEN, read_request,
        read_response, write_request, write_response,
    };
    use crate::model::{BackendKind, DaemonStatus};

    #[test]
    fn requests_round_trip_as_newline_delimited_json() {
        for request in [
            ControlRequest::Ping,
            ControlRequest::Status,
            ControlRequest::GetSocket,
            ControlRequest::Shutdown,
        ] {
            let mut encoded = Vec::new();
            write_request(&mut encoded, &request).unwrap();
            assert_eq!(encoded.last(), Some(&b'\n'));

            let mut reader = BufReader::new(Cursor::new(encoded));
            assert_eq!(read_request(&mut reader).unwrap(), Some(request));
            assert_eq!(read_request(&mut reader).unwrap(), None);
        }
    }

    #[test]
    fn responses_round_trip_as_newline_delimited_json() {
        let status = DaemonStatus {
            pid: 42,
            backend: BackendKind::Pageant,
            socket_path: PathBuf::from("agent.sock"),
            uptime_seconds: 7,
            active_connections: 2,
        };
        let responses = [
            ControlResponse::Pong,
            ControlResponse::Status(status),
            ControlResponse::Socket(PathBuf::from("agent.sock")),
            ControlResponse::Ok,
            ControlResponse::Error("failed".to_owned()),
        ];

        for response in responses {
            let mut encoded = Vec::new();
            write_response(&mut encoded, &response).unwrap();
            let mut reader = BufReader::new(Cursor::new(encoded));

            assert_eq!(read_response(&mut reader).unwrap(), Some(response));
        }
    }

    #[test]
    fn codec_accepts_crlf() {
        let mut reader = BufReader::new(Cursor::new(b"\"ping\"\r\n"));

        assert_eq!(
            read_request(&mut reader).unwrap(),
            Some(ControlRequest::Ping)
        );
    }

    #[test]
    fn missing_newline_is_reported() {
        let mut reader = BufReader::new(Cursor::new(b"\"ping\""));

        assert!(matches!(
            read_request(&mut reader),
            Err(ControlCodecError::MissingNewline)
        ));
    }

    #[test]
    fn oversized_input_is_rejected() {
        let mut input = vec![b' '; MAX_CONTROL_LINE_LEN + 1];
        input.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(input));

        assert!(matches!(
            read_request(&mut reader),
            Err(ControlCodecError::LineTooLong { .. })
        ));
    }

    #[test]
    fn oversized_output_is_rejected() {
        let response = ControlResponse::Error("x".repeat(MAX_CONTROL_LINE_LEN));
        let mut output = Vec::new();

        assert!(matches!(
            write_response(&mut output, &response),
            Err(ControlCodecError::LineTooLong { .. })
        ));
        assert!(output.is_empty());
    }
}
