use std::io::{self, Read, Write};

use thiserror::Error;

pub const FRAME_HEADER_LEN: usize = 4;
pub const MAX_PAYLOAD_LEN: usize = 256 * 1024;
pub const MAX_FRAME_LEN: usize = FRAME_HEADER_LEN + MAX_PAYLOAD_LEN;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame I/O failed")]
    Io(#[source] io::Error),
    #[error("truncated frame header: received {bytes_read} of 4 bytes")]
    TruncatedHeader { bytes_read: usize },
    #[error("frame payload length {length} exceeds the {max} byte limit")]
    PayloadTooLarge { length: usize, max: usize },
    #[error("truncated frame payload: received {bytes_read} of {expected} bytes")]
    TruncatedPayload { expected: usize, bytes_read: usize },
    #[error("frame has {actual} payload bytes but declares {declared}")]
    LengthMismatch { declared: usize, actual: usize },
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0_u8; FRAME_HEADER_LEN];
    let header_bytes = read_up_to(reader, &mut header)?;
    if header_bytes == 0 {
        return Ok(None);
    }
    if header_bytes != FRAME_HEADER_LEN {
        return Err(FrameError::TruncatedHeader {
            bytes_read: header_bytes,
        });
    }

    let payload_len = u32::from_be_bytes(header) as usize;
    ensure_payload_limit(payload_len)?;

    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload_len);
    frame.extend_from_slice(&header);
    frame.resize(FRAME_HEADER_LEN + payload_len, 0);

    let payload_bytes = read_up_to(reader, &mut frame[FRAME_HEADER_LEN..])?;
    if payload_bytes != payload_len {
        return Err(FrameError::TruncatedPayload {
            expected: payload_len,
            bytes_read: payload_bytes,
        });
    }

    Ok(Some(frame))
}

pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    ensure_payload_limit(payload.len())?;

    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn validate_frame(frame: &[u8]) -> Result<(), FrameError> {
    if frame.len() < FRAME_HEADER_LEN {
        return Err(FrameError::TruncatedHeader {
            bytes_read: frame.len(),
        });
    }

    let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    ensure_payload_limit(declared)?;

    let actual = frame.len() - FRAME_HEADER_LEN;
    if actual < declared {
        return Err(FrameError::TruncatedPayload {
            expected: declared,
            bytes_read: actual,
        });
    }
    if actual > declared {
        return Err(FrameError::LengthMismatch { declared, actual });
    }

    Ok(())
}

pub fn write_frame<W: Write>(writer: &mut W, frame: &[u8]) -> Result<(), FrameError> {
    validate_frame(frame)?;
    writer.write_all(frame).map_err(FrameError::Io)
}

fn ensure_payload_limit(length: usize) -> Result<(), FrameError> {
    if length > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge {
            length,
            max: MAX_PAYLOAD_LEN,
        });
    }
    Ok(())
}

fn read_up_to<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<usize, FrameError> {
    let mut offset = 0;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..]) {
            Ok(0) => break,
            Ok(read) => offset += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(offset)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{
        FrameError, MAX_PAYLOAD_LEN, encode_frame, read_frame, validate_frame, write_frame,
    };

    #[test]
    fn frame_round_trips() {
        let frame = encode_frame(b"request").unwrap();
        let mut input = Cursor::new(frame.clone());

        assert_eq!(read_frame(&mut input).unwrap(), Some(frame));
        assert!(read_frame(&mut input).unwrap().is_none());
    }

    #[test]
    fn clean_eof_is_the_only_none_result() {
        assert!(
            read_frame(&mut Cursor::new(Vec::<u8>::new()))
                .unwrap()
                .is_none()
        );

        let error = read_frame(&mut Cursor::new(vec![0, 0, 0])).unwrap_err();
        assert!(matches!(
            error,
            FrameError::TruncatedHeader { bytes_read: 3 }
        ));
    }

    #[test]
    fn truncated_payload_is_reported() {
        let error = read_frame(&mut Cursor::new(vec![0, 0, 0, 3, 1, 2])).unwrap_err();

        assert!(matches!(
            error,
            FrameError::TruncatedPayload {
                expected: 3,
                bytes_read: 2
            }
        ));
    }

    #[test]
    fn oversized_payload_is_rejected_before_allocation() {
        let header = ((MAX_PAYLOAD_LEN + 1) as u32).to_be_bytes();
        let error = read_frame(&mut Cursor::new(header)).unwrap_err();

        assert!(matches!(error, FrameError::PayloadTooLarge { .. }));
        assert!(matches!(
            encode_frame(&vec![0; MAX_PAYLOAD_LEN + 1]),
            Err(FrameError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn validation_rejects_trailing_bytes() {
        let error = validate_frame(&[0, 0, 0, 1, 1, 2]).unwrap_err();

        assert!(matches!(
            error,
            FrameError::LengthMismatch {
                declared: 1,
                actual: 2
            }
        ));
    }

    #[test]
    fn write_frame_validates_and_writes_full_wire_frame() {
        let frame = encode_frame(b"response").unwrap();
        let mut output = Vec::new();

        write_frame(&mut output, &frame).unwrap();

        assert_eq!(output, frame);
    }
}
