use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};

use crate::platform::{self, AfUnixEndpointIdentity};

const LISTEN_BACKLOG: i32 = 16;
const MARKER_SUFFIX: &str = ".afunix-agent-owned";
const MARKER_PREFIX: &[u8] = b"afunix-agent socket endpoint v1\0";
const MARKER_ID_LEN: usize = 24;
const STALE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

pub type UnixStream = Socket;

#[derive(Debug)]
pub struct UnixListener {
    socket: Option<Socket>,
    path: PathBuf,
    marker_path: PathBuf,
    identity: AfUnixEndpointIdentity,
    cleaned: bool,
}

impl UnixListener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        create_parent_directory(&path)?;
        let marker_path = marker_path(&path);
        prepare_paths(&path, &marker_path)?;
        let mut marker = OwnershipMarker::reserve(marker_path.clone())?;

        let address = SockAddr::unix(&path).map_err(|error| {
            contextual_error(error, format!("invalid AF_UNIX path {}", path.display()))
        })?;
        let socket = Socket::new(Domain::UNIX, Type::STREAM, None).map_err(|error| {
            contextual_error(error, "failed to create the AF_UNIX socket".to_owned())
        })?;
        socket.bind(&address).map_err(|error| {
            contextual_error(
                error,
                format!("failed to bind AF_UNIX endpoint {}", path.display()),
            )
        })?;
        marker.preserve();
        let identity = platform::af_unix_endpoint_identity(&path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "bound endpoint is not a Windows AF_UNIX reparse point",
            )
        })?;
        if let Err(error) = platform::restrict_path_to_current_user(&path) {
            marker.remove_on_drop();
            return Err(cleanup_failed_bind(
                socket,
                &path,
                identity,
                contextual_error(error, "failed to secure the AF_UNIX endpoint".to_owned()),
            ));
        }
        if let Err(error) = socket.listen(LISTEN_BACKLOG) {
            marker.remove_on_drop();
            return Err(cleanup_failed_bind(
                socket,
                &path,
                identity,
                contextual_error(
                    error,
                    format!("failed to listen on AF_UNIX endpoint {}", path.display()),
                ),
            ));
        }
        if let Err(error) = socket.set_nonblocking(true) {
            marker.remove_on_drop();
            return Err(cleanup_failed_bind(
                socket,
                &path,
                identity,
                contextual_error(
                    error,
                    format!(
                        "failed to make AF_UNIX listener {} nonblocking",
                        path.display()
                    ),
                ),
            ));
        }

        if let Err(error) = marker.finalize(identity) {
            marker.remove_on_drop();
            return Err(cleanup_failed_bind(socket, &path, identity, error));
        }
        marker.preserve();
        Ok(Self {
            socket: Some(socket),
            path,
            marker_path,
            identity,
            cleaned: false,
        })
    }

    pub fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.socket().accept()?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn close(mut self) -> io::Result<()> {
        self.cleanup()
    }

    fn socket(&self) -> &Socket {
        self.socket
            .as_ref()
            .expect("listener socket is present until cleanup")
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.cleaned {
            return Ok(());
        }
        self.cleaned = true;
        drop(self.socket.take());

        match platform::remove_af_unix_endpoint(&self.path, self.identity) {
            Ok(()) => remove_owned_marker(&self.marker_path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                remove_owned_marker(&self.marker_path)
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                remove_owned_marker(&self.marker_path)?;
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for UnixListener {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            tracing::error!(error = %error, path = %self.path.display(), "AF_UNIX endpoint cleanup failed");
        }
    }
}

fn create_parent_directory(path: &Path) -> io::Result<()> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|error| {
        contextual_error(
            error,
            format!(
                "failed to create AF_UNIX endpoint directory {}",
                parent.display()
            ),
        )
    })
}

fn prepare_paths(path: &Path, marker_path: &Path) -> io::Result<()> {
    let endpoint_exists = path.try_exists().map_err(|error| {
        contextual_error(
            error,
            format!("failed to inspect AF_UNIX endpoint path {}", path.display()),
        )
    })?;
    let marker_exists = marker_path.try_exists().map_err(|error| {
        contextual_error(
            error,
            format!(
                "failed to inspect ownership marker {}",
                marker_path.display()
            ),
        )
    })?;

    if !endpoint_exists {
        if marker_exists {
            verify_marker(marker_path)?;
            remove_owned_marker(marker_path)?;
        }
        return Ok(());
    }

    let identity = platform::af_unix_endpoint_identity(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-AF_UNIX path {}", path.display()),
        )
    })?;
    if !marker_exists {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "refusing to remove AF_UNIX endpoint without ownership marker {}",
                path.display()
            ),
        ));
    }
    let marker_identity = verify_marker(marker_path)?;
    if marker_identity != identity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "AF_UNIX ownership marker does not match endpoint {}",
                path.display()
            ),
        ));
    }
    if endpoint_is_live(path)? {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("AF_UNIX endpoint is already active at {}", path.display()),
        ));
    }

    platform::remove_af_unix_endpoint(path, identity).map_err(|error| {
        contextual_error(
            error,
            format!("failed to remove stale AF_UNIX endpoint {}", path.display()),
        )
    })?;
    remove_owned_marker(marker_path)
}

fn endpoint_is_live(path: &Path) -> io::Result<bool> {
    let address = SockAddr::unix(path)?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    match socket.connect_timeout(&address, STALE_CONNECT_TIMEOUT) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(false),
        Err(error) => Err(contextual_error(
            error,
            format!(
                "cannot prove that AF_UNIX endpoint {} is stale",
                path.display()
            ),
        )),
    }
}

fn marker_path(path: &Path) -> PathBuf {
    let mut marker: OsString = path.as_os_str().to_owned();
    marker.push(MARKER_SUFFIX);
    PathBuf::from(marker)
}

fn verify_marker(path: &Path) -> io::Result<AfUnixEndpointIdentity> {
    let expected_len = MARKER_PREFIX.len() + MARKER_ID_LEN;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != expected_len as u64 {
        return Err(invalid_marker(path));
    }
    let mut content = Vec::with_capacity(expected_len);
    OpenOptions::new()
        .read(true)
        .open(path)?
        .take(expected_len as u64 + 1)
        .read_to_end(&mut content)?;
    if !content.starts_with(MARKER_PREFIX) || content.len() != expected_len {
        return Err(invalid_marker(path));
    }

    let identity = &content[MARKER_PREFIX.len()..];
    let volume_serial_number = u64::from_le_bytes(identity[..8].try_into().unwrap());
    let file_id = identity[8..].try_into().unwrap();
    Ok(AfUnixEndpointIdentity {
        volume_serial_number,
        file_id,
    })
}

fn marker_content(identity: AfUnixEndpointIdentity) -> Vec<u8> {
    let mut content = Vec::with_capacity(MARKER_PREFIX.len() + MARKER_ID_LEN);
    content.extend_from_slice(MARKER_PREFIX);
    content.extend_from_slice(&identity.volume_serial_number.to_le_bytes());
    content.extend_from_slice(&identity.file_id);
    content
}

fn remove_owned_marker(path: &Path) -> io::Result<()> {
    match verify_marker(path) {
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn invalid_marker(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid AF_UNIX ownership marker {}", path.display()),
    )
}

struct OwnershipMarker {
    path: PathBuf,
    file: Option<fs::File>,
    remove_on_drop: bool,
}

impl OwnershipMarker {
    fn reserve(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let marker = Self {
            path,
            file: Some(file),
            remove_on_drop: true,
        };
        platform::restrict_path_to_current_user(&marker.path)?;
        marker.file.as_ref().unwrap().sync_all()?;
        Ok(marker)
    }

    fn finalize(&mut self, identity: AfUnixEndpointIdentity) -> io::Result<()> {
        let file = self.file.as_mut().unwrap();
        file.set_len(0)?;
        file.write_all(&marker_content(identity))?;
        file.sync_all()
    }

    fn preserve(&mut self) {
        self.remove_on_drop = false;
    }

    fn remove_on_drop(&mut self) {
        self.remove_on_drop = true;
    }
}

impl Drop for OwnershipMarker {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn cleanup_failed_bind(
    socket: Socket,
    path: &Path,
    identity: AfUnixEndpointIdentity,
    error: io::Error,
) -> io::Error {
    drop(socket);
    match platform::remove_af_unix_endpoint(path, identity) {
        Ok(()) => error,
        Err(cleanup_error) => io::Error::new(
            error.kind(),
            format!("{error}; endpoint cleanup failed: {cleanup_error}"),
        ),
    }
}

fn contextual_error(error: io::Error, context: String) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}
