//! Startup self-update from GitHub Releases (compiled only with the `updater` feature).
//!
//! * Binary in a writable folder (tar.gz release, `~/assistant/bin`): replaced in place.
//! * Binary in a read-only folder (the .deb installs to `/usr/bin`): the new version is
//!   downloaded to `~/assistant/bin/assistant`. On later starts `/usr/bin/assistant` hands
//!   over to that copy while it is newer than itself, so a newer .deb wins automatically.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

const OWNER: &str = "ostrbor";
const REPO: &str = "assistant";
const BIN: &str = "assistant";
const TARGET: &str = "x86_64-unknown-linux-gnu";
const VERSION: &str = env!("CARGO_PKG_VERSION");

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// Where updates of a read-only installation are kept: `~/assistant/bin/assistant`.
fn user_bin() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| Path::new(&h).join("assistant").join("bin").join(BIN))
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".{BIN}-write-test-{}", std::process::id()));
    let ok = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_ok();
    let _ = std::fs::remove_file(probe);
    ok
}

/// Version reported by another copy of this program (`--version`).
fn version_of(exe: &Path) -> Option<String> {
    let out = std::process::Command::new(exe).arg("--version").output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().last().map(str::to_string)
}

fn restart(exe: &Path) -> ! {
    let err = std::process::Command::new(exe).args(std::env::args().skip(1)).exec();
    eprintln!("Restart failed: {err}; please start the app again");
    std::process::exit(1);
}

/// Hands over to a newer user copy, then checks the latest GitHub release; if newer, installs
/// it and restarts. Never fails startup: any error is printed as a warning.
pub fn check_and_restart() {
    let Ok(exe) = std::env::current_exe() else { return };
    if let Some(user) = user_bin().filter(|u| *u != exe && u.exists()) {
        let newer = version_of(&user).is_some_and(|v| self_update::version::bump_is_greater(VERSION, &v).unwrap_or(false));
        if newer {
            restart(&user);
        }
    }
    if std::env::args().any(|a| a == "--no-update") || std::env::var_os("ASSISTANT_NO_UPDATE").is_some() {
        return;
    }
    match try_update(&exe) {
        Ok(Some((v, new_exe))) => {
            eprintln!("Updated to v{v}, restarting…");
            restart(&new_exe);
        }
        Ok(None) => {}
        Err(e) => eprintln!("Update check skipped: {e}"),
    }
}

/// Returns the new version and the binary to restart into.
fn try_update(exe: &Path) -> Res<Option<(String, PathBuf)>> {
    let updater = self_update::backends::github::Update::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .bin_name(BIN)
        .target(TARGET)
        .current_version(VERSION)
        .no_confirm(true)
        .show_download_progress(false)
        .show_output(false)
        .build()?;
    if exe.parent().is_some_and(dir_writable) {
        let status = updater.update()?;
        return Ok(status.updated().then(|| (status.version().to_string(), exe.to_path_buf())));
    }
    // Read-only installation: put the new binary into the user's folder.
    let dest = user_bin().ok_or("HOME is not set")?;
    let release = updater.get_latest_release()?;
    if !self_update::version::bump_is_greater(VERSION, &release.version)? {
        return Ok(None);
    }
    let asset = release.asset_for(TARGET, None).ok_or("the release has no binary for this platform")?;
    let dir = dest.parent().ok_or("bad install path")?;
    std::fs::create_dir_all(dir)?;
    let tmp = self_update::TempDir::new_in(dir)?;
    let archive = tmp.path().join(&asset.name);
    let mut headers = updater.api_headers(&updater.auth_token())?;
    headers.insert(reqwest::header::ACCEPT, "application/octet-stream".parse()?);
    self_update::Download::from_url(&asset.download_url).set_headers(headers).download_to(&mut std::fs::File::create(&archive)?)?;
    self_update::Extract::from_source(&archive).extract_file(tmp.path(), BIN)?;
    let new_bin = tmp.path().join(BIN);
    std::fs::set_permissions(&new_bin, std::fs::Permissions::from_mode(0o755))?;
    // Same filesystem, so this replaces the old copy atomically (even if it is running).
    std::fs::rename(&new_bin, &dest)?;
    Ok(Some((release.version, dest)))
}
