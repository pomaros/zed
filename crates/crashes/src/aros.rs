//! Crash reporting on AROS.
//!
//! The real implementation captures a minidump in a subprocess and ships it
//! over an IPC socket (`crash-handler` + `minidumper`). Neither builds for
//! AROS: they need per-platform exception handling and a unix-socket IPC
//! channel. Rust panics are recorded in `T:zed-panic.log` before the normal
//! panic hook runs. Native signals and exceptions require independent OS
//! inspection; this hook does not claim to capture them or produce a backtrace.
//!
//! This module mirrors the crate's public API exactly so callers need no
//! `cfg`s; subprocess crash reporting remains unavailable.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::panic::Location;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Once};
use std::time::Duration;

use system_specs::GpuSpecs;

/// Stands in for `minidumper::Client`. Never connected to anything.
pub struct Client;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CrashInfo {
    pub init: InitCrashHandler,
    pub panic: Option<CrashPanic>,
    pub minidump_error: Option<String>,
    #[serde(default)]
    pub abort_message: Option<String>,
    pub gpus: Vec<system_specs::GpuInfo>,
    pub active_gpu: Option<system_specs::GpuSpecs>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct InitCrashHandler {
    pub session_id: String,
    pub zed_version: String,
    pub binary: String,
    pub release_channel: String,
    pub commit_sha: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CrashPanic {
    pub message: String,
    pub span: String,
}

/// The Sentry field identifying the crashing user.
pub const SENTRY_USER_ID: &str = "sentry[user][id]";

#[derive(Debug, Clone)]
pub struct UserInfo {
    pub metrics_id: Option<String>,
    pub is_staff: Option<bool>,
}

/// Retain Rust panic location and payload without requiring a subprocess or
/// a working stderr console. Repeated installation leaves the hook unchanged.
pub fn force_backtrace() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("T:zed-panic.log")
            {
                let _ = writeln!(file, "[ZED-PANIC] {info}");
                let _ = file.flush();
            }
            previous(info);
        }));
    });
}

pub fn init<F, S, C, P>(
    _crash_init: InitCrashHandler,
    _spawn: S,
    _socket_path: P,
    _wait_timer: C,
) -> impl Future<Output = Arc<Client>> + use<F, C, S, P>
where
    F: Future<Output = ()> + Send + Sync + 'static,
    C: (Fn(Duration) -> F) + Send + Sync + 'static,
    S: FnOnce(Pin<Box<dyn Future<Output = ()> + Send + 'static>>),
    P: FnOnce(u32) -> PathBuf,
{
    force_backtrace();
    async { Arc::new(Client) }
}

pub fn set_gpu_info(_crash_client: &Arc<Client>, _specs: GpuSpecs) {}

pub fn set_user_info(_crash_client: &Arc<Client>, _info: UserInfo) {}

pub fn set_tag(_crash_client: &Arc<Client>, _key: impl Into<String>, _value: Option<String>) {}

pub fn set_tags<K>(
    _crash_client: &Arc<Client>,
    _tags: impl IntoIterator<Item = (K, Option<String>)>,
) where
    K: Into<String>,
{
}

pub fn shutdown_crash_handler(_crash_client: &Arc<Client>) {}

pub fn panic_hook(_crash_client: Arc<Client>, _message: &str, _location: Option<&Location>) {}

pub fn crash_server(_socket: &Path, _logs_dir: PathBuf) {}
