//! When the app has no terminal (started from the menu), stderr is redirected to
//! `assistant.log` in the working directory so that warnings and errors can be found.

use std::io::{IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

const LOG: &str = "assistant.log";
const MAX_BYTES: u64 = 1_000_000;

/// Redirects stderr to `<workdir>/assistant.log` unless it is a terminal. Never fails startup.
pub fn redirect_stderr(workdir: &Path) {
    if std::io::stderr().is_terminal() {
        return;
    }
    let path = workdir.join(LOG);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, workdir.join(format!("{LOG}.old")));
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path) else { return };
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = writeln!(file, "=== assistant v{} started, unix {secs} ===", env!("CARGO_PKG_VERSION"));
    unsafe {
        libc::dup2(file.as_raw_fd(), 2);
    }
}
