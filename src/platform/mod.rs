#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::{
    AfUnixEndpointIdentity, NamedPipeServer, SingleInstanceGuard, af_unix_endpoint_identity,
    control_pipe_name, control_pipe_name_for_sid, current_user_sid, is_af_unix_endpoint,
    open_named_pipe_client, remove_af_unix_endpoint, restrict_path_to_current_user,
    singleton_mutex_name, singleton_mutex_name_for_sid, spawn_detached_current_exe,
};
