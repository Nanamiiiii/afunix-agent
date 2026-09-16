use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::ExitStatusExt;
use std::path::Path;
use std::process::ExitStatus;
use std::ptr;

use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GetLastError, HANDLE, HLOCAL, LocalFree, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo, FileDispositionInfo, FileIdInfo,
    GetFileInformationByHandleEx, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, SetFileInformationByHandle,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CreateMutexW, CreateProcessW, DETACHED_PROCESS, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows::core::{BOOL, HRESULT, PCWSTR, PWSTR};

#[cfg(test)]
mod spawn_tests {
    use super::*;
    use windows::Win32::Foundation::ERROR_BROKEN_PIPE;
    use windows::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};

    #[test]
    #[ignore = "helper process for the detached spawn test"]
    fn detached_child() {
        thread::sleep(Duration::from_secs(30));
    }

    #[test]
    fn detached_child_does_not_inherit_pipe_writer() {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0) }.unwrap();
        let read = unsafe { owned_handle(read) };
        let write = unsafe { owned_handle(write) };
        let mut child = spawn_detached_current_exe([
            "--ignored",
            "--exact",
            "platform::windows::spawn_tests::detached_child",
        ])
        .unwrap();
        drop(write);

        let result =
            unsafe { PeekNamedPipe(HANDLE(read.as_raw_handle()), None, 0, None, None, None) };
        let running = child.try_wait();
        let _ = child.kill();
        child.wait().unwrap();

        assert!(running.unwrap().is_none(), "child must still be running");
        assert_eq!(
            result.unwrap_err().code(),
            HRESULT::from_win32(ERROR_BROKEN_PIPE.0)
        );
    }
}

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;
const PIPE_OPEN_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;

pub fn current_user_sid() -> io::Result<String> {
    process_user_sid(unsafe { GetCurrentProcess() })
}

fn process_user_sid(process: HANDLE) -> io::Result<String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(process, TOKEN_QUERY, &mut token)
            .map_err(|error| windows_error("failed to open the process token", error))?;
    }
    let token = unsafe { owned_handle(token) };

    let mut required = 0;
    let sizing_result = unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            None,
            0,
            &mut required,
        )
    };
    match sizing_result {
        Err(error) if error.code() == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) => {}
        Err(error) => {
            return Err(windows_error(
                "failed to determine the process token user size",
                error,
            ));
        }
        Ok(()) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "process token user query returned no data",
            ));
        }
    }

    if required < size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process token user data is too short",
        ));
    }

    let word_count = (required as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0usize; word_count];
    unsafe {
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
        .map_err(|error| windows_error("failed to read the process token user", error))?;
    }

    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut string_sid = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid)
            .map_err(|error| windows_error("failed to convert the process user SID", error))?;
    }
    let allocation = LocalAllocation(string_sid.0.cast());
    let sid = unsafe { string_sid.to_string() }.map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to decode the process user SID: {error}"),
        )
    })?;
    drop(allocation);
    Ok(sid)
}

pub fn control_pipe_name() -> io::Result<String> {
    current_user_sid().map(|sid| control_pipe_name_for_sid(&sid))
}

pub fn control_pipe_name_for_sid(sid: &str) -> String {
    format!(r"\\.\pipe\afunix-agent-control-v1-{sid}")
}

pub fn singleton_mutex_name() -> io::Result<String> {
    current_user_sid().map(|sid| singleton_mutex_name_for_sid(&sid))
}

pub fn singleton_mutex_name_for_sid(sid: &str) -> String {
    format!(r"Local\afunix-agent-singleton-{sid}")
}

pub struct SingleInstanceGuard {
    _handle: OwnedHandle,
}

impl SingleInstanceGuard {
    pub fn acquire() -> io::Result<Self> {
        let sid = current_user_sid()?;
        Self::acquire_for_sid(&sid)
    }

    pub fn acquire_for_sid(sid: &str) -> io::Result<Self> {
        let descriptor = SecurityDescriptor::for_sid(sid)?;
        let attributes = descriptor.attributes();
        let name = to_wide(OsStr::new(&singleton_mutex_name_for_sid(sid)))?;

        let handle = unsafe {
            CreateMutexW(
                Some(&attributes as *const SECURITY_ATTRIBUTES),
                false,
                PCWSTR(name.as_ptr()),
            )
        }
        .map_err(|error| windows_error("failed to create the singleton mutex", error))?;
        let last_error = unsafe { GetLastError() };
        let handle = unsafe { owned_handle(handle) };

        if last_error == ERROR_ALREADY_EXISTS {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "daemon is already running",
            ));
        }

        Ok(Self { _handle: handle })
    }
}

pub struct NamedPipeServer {
    name: String,
    sid: String,
    pending: Option<OwnedHandle>,
}

impl NamedPipeServer {
    pub fn for_current_user() -> io::Result<Self> {
        let sid = current_user_sid()?;
        Self::new(control_pipe_name_for_sid(&sid), sid)
    }

    pub fn new(name: impl Into<String>, sid: impl Into<String>) -> io::Result<Self> {
        let name = name.into();
        let sid = sid.into();
        to_wide(OsStr::new(&name))?;
        let pending = Some(create_pipe_instance(&name, &sid, true)?);
        Ok(Self { name, sid, pending })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn accept(&mut self) -> io::Result<File> {
        let handle = self.pending.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "control pipe has no pending instance",
            )
        })?;

        match unsafe { ConnectNamedPipe(HANDLE(handle.as_raw_handle()), None) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => {}
            Err(error) => {
                return Err(windows_error(
                    "failed to connect the named pipe server",
                    error,
                ));
            }
        }

        self.pending = Some(create_pipe_instance(&self.name, &self.sid, false)?);
        Ok(File::from(handle))
    }
}

fn create_pipe_instance(name: &str, sid: &str, first: bool) -> io::Result<OwnedHandle> {
    let descriptor = SecurityDescriptor::for_sid(sid)?;
    let attributes = descriptor.attributes();
    let name = to_wide(OsStr::new(name))?;
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_SIZE,
            PIPE_BUFFER_SIZE,
            0,
            Some(&attributes as *const SECURITY_ATTRIBUTES),
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { owned_handle(handle) })
}

pub fn open_named_pipe_client(path: impl AsRef<Path>) -> io::Result<File> {
    let started = Instant::now();
    loop {
        match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(pipe) => {
                verify_named_pipe_server(&pipe)?;
                return Ok(pipe);
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
                    && started.elapsed() < PIPE_OPEN_TIMEOUT =>
            {
                thread::sleep(PIPE_OPEN_RETRY_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn verify_named_pipe_server(pipe: &File) -> io::Result<()> {
    let mut process_id = 0;
    unsafe {
        GetNamedPipeServerProcessId(HANDLE(pipe.as_raw_handle()), &mut process_id)
            .map_err(|error| windows_error("failed to identify the control pipe server", error))?;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .map_err(|error| windows_error("failed to open the control pipe server process", error))?;
    let process = unsafe { owned_handle(process) };
    let server_sid = process_user_sid(HANDLE(process.as_raw_handle()))?;
    if server_sid != current_user_sid()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control pipe server belongs to a different user",
        ));
    }
    Ok(())
}

#[derive(Debug)]
pub struct DetachedChild {
    process: OwnedHandle,
    id: u32,
}

impl DetachedChild {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn kill(&mut self) -> io::Result<()> {
        match unsafe { TerminateProcess(HANDLE(self.process.as_raw_handle()), 1) } {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.try_wait()?.is_some() {
                    Ok(())
                } else {
                    Err(windows_error("failed to terminate detached process", error))
                }
            }
        }
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_for_exit(INFINITE)?.ok_or_else(|| {
            io::Error::other("infinite wait for detached process unexpectedly timed out")
        })
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_for_exit(0)
    }

    fn wait_for_exit(&self, timeout: u32) -> io::Result<Option<ExitStatus>> {
        let process = HANDLE(self.process.as_raw_handle());
        match unsafe { WaitForSingleObject(process, timeout) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                unsafe { GetExitCodeProcess(process, &mut code) }.map_err(|error| {
                    windows_error("failed to read detached process exit code", error)
                })?;
                Ok(Some(ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => Err(io::Error::last_os_error()),
            _ => Err(io::Error::other("unexpected detached process wait result")),
        }
    }
}

fn append_quoted_arg(command_line: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
    let arg: Vec<u16> = arg.encode_wide().collect();
    if arg.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process argument contains an embedded NUL",
        ));
    }
    command_line.push(b'"' as u16);
    let mut backslashes = 0;
    for unit in arg {
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        // CRT parsing halves backslashes before quotes, including the closing quote.
        let count = if unit == b'"' as u16 {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        command_line.extend(std::iter::repeat_n(b'\\' as u16, count));
        command_line.push(unit);
        backslashes = 0;
    }
    command_line.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    command_line.push(b'"' as u16);
    Ok(())
}

pub fn spawn_detached_current_exe<I, S>(args: I) -> io::Result<DetachedChild>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let executable = std::env::current_exe()?;
    let application = to_wide(executable.as_os_str())?;
    let mut command_line = Vec::new();
    // argv[0] has special CRT parsing, but an executable file path cannot contain
    // quotes or end in a backslash, so ordinary quoting is also correct here.
    append_quoted_arg(&mut command_line, executable.as_os_str())?;
    for arg in args {
        command_line.push(b' ' as u16);
        append_quoted_arg(&mut command_line, arg.as_ref())?;
    }
    command_line.push(0);
    if command_line.len() > 32_767 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process command line exceeds the Windows UTF-16 length limit",
        ));
    }
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: HANDLE::default(),
        hStdOutput: HANDLE::default(),
        hStdError: HANDLE::default(),
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            None,
            PCWSTR::null(),
            &startup,
            &mut information,
        )
    }
    .map_err(|error| windows_error("failed to spawn detached process", error))?;
    let process = unsafe { owned_handle(information.hProcess) };
    let thread = unsafe { owned_handle(information.hThread) };
    drop(thread);
    Ok(DetachedChild {
        process,
        id: information.dwProcessId,
    })
}

#[cfg(test)]
mod quoting_tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn quotes_crt_arguments() {
        for (input, expected) in [
            ("", "\"\""),
            ("plain", "\"plain\""),
            ("space and\ttab", "\"space and\ttab\""),
            (r#"a"b"#, r#""a\"b""#),
            (r"a\b", r#""a\b""#),
            (r"a\", r#""a\\""#),
            (r#"a\"b"#, r#""a\\\"b""#),
            (r#"a\\"b"#, r#""a\\\\\"b""#),
            ("日本語😀", "\"日本語😀\""),
        ] {
            let mut actual = Vec::new();
            append_quoted_arg(&mut actual, OsStr::new(input)).unwrap();
            assert_eq!(
                actual,
                expected.encode_utf16().collect::<Vec<_>>(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn preserves_unpaired_utf16_surrogates() {
        let mut actual = Vec::new();
        append_quoted_arg(&mut actual, &OsString::from_wide(&[0xd800])).unwrap();
        assert_eq!(actual, [b'"' as u16, 0xd800, b'"' as u16]);
    }

    #[test]
    fn rejects_embedded_nul_without_appending() {
        let mut actual = vec![42];
        let error = append_quoted_arg(&mut actual, OsStr::new("a\0b")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(actual, [42]);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AfUnixEndpointIdentity {
    pub(crate) volume_serial_number: u64,
    pub(crate) file_id: [u8; 16],
}

pub fn is_af_unix_endpoint(path: impl AsRef<Path>) -> io::Result<bool> {
    af_unix_endpoint_identity(path).map(|identity| identity.is_some())
}

pub fn af_unix_endpoint_identity(
    path: impl AsRef<Path>,
) -> io::Result<Option<AfUnixEndpointIdentity>> {
    let handle = open_path_handle(path.as_ref(), FILE_READ_ATTRIBUTES.0)?;
    if !handle_is_af_unix(&handle)? {
        return Ok(None);
    }
    endpoint_identity(&handle).map(Some)
}

pub fn remove_af_unix_endpoint(
    path: impl AsRef<Path>,
    expected: AfUnixEndpointIdentity,
) -> io::Result<()> {
    let handle = open_path_handle(path.as_ref(), FILE_READ_ATTRIBUTES.0 | DELETE.0)?;
    if !handle_is_af_unix(&handle)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint was replaced by a non-AF_UNIX path",
        ));
    }
    if endpoint_identity(&handle)? != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AF_UNIX endpoint identity changed before cleanup",
        ));
    }

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(handle.as_raw_handle()),
            FileDispositionInfo,
            ptr::from_ref(&disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
        .map_err(|error| {
            windows_error("failed to mark the AF_UNIX endpoint for deletion", error)
        })?;
    }
    Ok(())
}

pub fn restrict_path_to_current_user(path: impl AsRef<Path>) -> io::Result<()> {
    let sid = current_user_sid()?;
    let descriptor = SecurityDescriptor::for_sid(&sid)?;
    let mut dacl_present = BOOL::default();
    let mut dacl_defaulted = BOOL::default();
    let mut dacl = ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(
            descriptor.pointer(),
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
        .map_err(|error| windows_error("failed to read the endpoint security descriptor", error))?;
    }
    if !dacl_present.as_bool() || dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "endpoint security descriptor has no DACL",
        ));
    }

    let path = to_wide(path.as_ref().as_os_str())?;
    let result = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl.cast_const()),
            None,
        )
    };
    if result.0 != 0 {
        return Err(io::Error::from_raw_os_error(result.0 as i32));
    }
    Ok(())
}

fn open_path_handle(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let path = to_wide(path.as_os_str())?;
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|error| windows_error("failed to open the endpoint for inspection", error))?;
    Ok(unsafe { owned_handle(handle) })
}

fn handle_is_af_unix(handle: &OwnedHandle) -> io::Result<bool> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_raw_handle()),
            FileAttributeTagInfo,
            ptr::from_mut(&mut info).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
        .map_err(|error| windows_error("failed to inspect the endpoint reparse tag", error))?;
    }
    Ok(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        && info.ReparseTag == IO_REPARSE_TAG_AF_UNIX)
}

fn endpoint_identity(handle: &OwnedHandle) -> io::Result<AfUnixEndpointIdentity> {
    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(handle.as_raw_handle()),
            FileIdInfo,
            ptr::from_mut(&mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
        .map_err(|error| windows_error("failed to read the AF_UNIX endpoint identity", error))?;
    }
    Ok(AfUnixEndpointIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

struct SecurityDescriptor(LocalAllocation);

impl SecurityDescriptor {
    fn for_sid(sid: &str) -> io::Result<Self> {
        let sddl = format!("D:P(A;;GA;;;{sid})");
        let sddl = to_wide(OsStr::new(&sddl))?;
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .map_err(|error| windows_error("failed to build the security descriptor", error))?;
        }
        Ok(Self(LocalAllocation(descriptor.0)))
    }

    fn pointer(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.0.0)
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

unsafe fn owned_handle(handle: HANDLE) -> OwnedHandle {
    unsafe { OwnedHandle::from_raw_handle(handle.0) }
}

fn to_wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows object name contains an embedded NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn windows_error(context: &'static str, error: windows::core::Error) -> io::Error {
    let hresult = error.code().0 as u32;
    let kind = if hresult & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((hresult & 0xffff) as i32).kind()
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, format!("{context}: {error}"))
}
