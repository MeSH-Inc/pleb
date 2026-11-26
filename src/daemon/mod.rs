pub mod protocol;

#[cfg(unix)]
pub mod client;
#[cfg(unix)]
pub mod server;

#[cfg(unix)]
pub use client::{daemon_cef_render, daemon_http_get, daemon_ping, daemon_stop, is_daemon_running};
pub use protocol::{
    lock_path, log_path, socket_path, storage_key, DaemonRequest, DaemonResponseHeader,
    DaemonStatus, BUILD_ID, LOCK_NAME, LOG_NAME, MAX_FRAME_BODY_BYTES, MAX_HEADER_LINE_BYTES,
    PROTOCOL_VERSION, SOCKET_NAME,
};
#[cfg(unix)]
pub use server::run_daemon;
