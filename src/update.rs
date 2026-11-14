//! Startup self-update from GitHub Releases (compiled only with the `updater` feature).

use std::os::unix::process::CommandExt;

const OWNER: &str = "ostrbor";
const REPO: &str = "assistant";
const TARGET: &str = "x86_64-unknown-linux-gnu";

/// Checks the latest GitHub release; if newer, replaces this binary and restarts.
/// Never fails startup: any error is printed as a warning.
pub fn check_and_restart() {
    if std::env::args().any(|a| a == "--no-update") || std::env::var_os("ASSISTANT_NO_UPDATE").is_some() {
        return;
    }
    match try_update() {
        Ok(Some(v)) => {
            eprintln!("Updated to v{v}, restarting…");
            match std::env::current_exe() {
                Ok(exe) => {
                    let err = std::process::Command::new(exe).args(std::env::args().skip(1)).exec();
                    eprintln!("Restart failed: {err}; please start the app again");
                    std::process::exit(1);
                }
                Err(e) => eprintln!("Restart failed: {e}; please start the app again"),
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("Update check skipped: {e}"),
    }
}

fn try_update() -> Result<Option<String>, Box<dyn std::error::Error>> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name("assistant")
        .target(TARGET)
        .current_version(env!("CARGO_PKG_VERSION"))
        .no_confirm(true)
        .show_download_progress(false)
        .show_output(false)
        .build()?
        .update()?;
    Ok(status.updated().then(|| status.version().to_string()))
}
