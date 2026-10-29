//! Detached rollback watchdog and shared restore logic.
//!
//! Every apply writes a snapshot and marker protocol into a transaction
//! directory. The watchdog restores the snapshot if the UI neither keeps nor
//! cancels within the timeout, even when the app is killed with SIGKILL.

use anyhow::{bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::backend::wlr_randr::WlrRandrBackend;
use crate::backend::OutputBackend;
use crate::config::now_rfc3339;
use crate::kanshi::config::restore_config;
use crate::kanshi::ctl::KanshiClient;
use crate::model::snapshot::{EngineKind, Snapshot};
use crate::util;

pub const MARKER_KEEP: &str = "keep";
pub const MARKER_CANCEL: &str = "cancel";
pub const DEFAULT_TIMEOUT: u32 = 10;

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub config_restored: bool,
    pub reloaded: bool,
    pub outputs_restored: usize,
    pub missing_outputs: Vec<String>,
    pub fallback_used: bool,
}

impl RestoreReport {
    pub fn fully_restored(&self) -> bool {
        self.missing_outputs.is_empty()
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.config_restored {
            parts.push("config restored".to_string());
        }
        if self.reloaded {
            parts.push("kanshi reloaded".to_string());
        }
        if self.outputs_restored > 0 {
            parts.push(format!("{} output(s) restored", self.outputs_restored));
        }
        if !self.missing_outputs.is_empty() {
            parts.push(format!("missing: {}", self.missing_outputs.join(", ")));
        }
        if parts.is_empty() {
            "nothing needed restoring".to_string()
        } else {
            parts.join(", ")
        }
    }
}

pub fn read_marker(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("marker"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_marker(dir: &Path, value: &str) -> Result<()> {
    util::atomic_write(&dir.join("marker"), value.as_bytes())
}

fn read_state(dir: &Path) -> Result<Snapshot> {
    let path = dir.join("state.json");
    let bytes = fs::read(&path)
        .with_context(|| format!("reading transaction snapshot {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing transaction snapshot {}", path.display()))
}

/// Restore the snapshot in a transaction directory. Idempotent: racing with
/// the UI's own revert is harmless.
pub fn restore_transaction(dir: &Path) -> Result<RestoreReport> {
    let snapshot = read_state(dir)?;
    let mut report = RestoreReport::default();

    // 1. App state, byte for byte.
    let state_path = PathBuf::from(&snapshot.state_path);
    if !snapshot.state_path.is_empty() {
        match util::read_optional(&dir.join("state.before"))? {
            Some(bytes) => util::atomic_write(&state_path, &bytes)
                .with_context(|| format!("restoring {}", state_path.display()))?,
            None if !snapshot.state_existed => {
                let _ = fs::remove_file(&state_path);
            }
            None => {}
        }
    }

    // 2. Config and/or compositor state.
    let config_path = PathBuf::from(&snapshot.config_path);
    let config_before = util::read_optional(&dir.join("config.before"))?;

    match snapshot.engine {
        EngineKind::Kanshi => {
            if !snapshot.config_path.is_empty() {
                restore_config(
                    &config_path,
                    config_before.as_deref(),
                    snapshot.config_existed,
                )
                .with_context(|| format!("restoring {}", config_path.display()))?;
                report.config_restored = true;

                let client = KanshiClient::new();
                if util::command_exists(client.program()) {
                    report.reloaded = client.reload().is_ok();
                }
            }
            let backend = WlrRandrBackend::new();
            let matched = backend
                .list()
                .ok()
                .map(|live| {
                    let plan = snapshot.restore_plan(&live);
                    !plan.outputs.is_empty() && plan.matches_live_relaxed(&live)
                })
                .unwrap_or(false);
            if !report.reloaded || !matched {
                report.fallback_used = true;
                apply_snapshot(&snapshot, &backend, &mut report)?;
            } else {
                report.outputs_restored = snapshot.outputs.iter().filter(|o| o.enabled).count();
            }
        }
        EngineKind::Wlr => {
            // Normally the config is untouched with this engine, but a Keep
            // failure can happen after the candidate config was written. Only
            // restore when the file is still exactly our candidate, so edits
            // made during the countdown are preserved.
            if !snapshot.config_path.is_empty() {
                if let Some(candidate) = util::read_optional(&dir.join("candidate.config"))? {
                    let current = util::read_optional(&config_path)?;
                    if current.as_deref() == Some(candidate.as_slice()) {
                        restore_config(
                            &config_path,
                            config_before.as_deref(),
                            snapshot.config_existed,
                        )
                        .with_context(|| format!("restoring {}", config_path.display()))?;
                        report.config_restored = true;
                    }
                }
            }
            let backend = WlrRandrBackend::new();
            apply_snapshot(&snapshot, &backend, &mut report)?;
        }
    }

    Ok(report)
}

fn apply_snapshot(
    snapshot: &Snapshot,
    backend: &WlrRandrBackend,
    report: &mut RestoreReport,
) -> Result<()> {
    let live = backend
        .list()
        .context("reading outputs before restoring the snapshot")?;
    let plan = snapshot.restore_plan(&live);
    report.missing_outputs = snapshot
        .connected
        .iter()
        .filter(|name| !live.iter().any(|o| &o.name == *name))
        .cloned()
        .collect();
    if plan.outputs.is_empty() {
        bail!("none of the outputs from the snapshot are connected");
    }
    backend.dry_run(&plan)?;
    backend.apply(&plan)?;
    let after = backend.list()?;
    if !plan.matches_live_relaxed(&after) {
        bail!("restored configuration does not match the compositor state");
    }
    report.outputs_restored = plan.outputs.iter().filter(|(_, p)| p.enabled).count();
    Ok(())
}

/// Restore, then stop the watchdog and remove the transaction directory.
pub fn revert_and_finish(dir: &Path) -> Result<RestoreReport> {
    let report = restore_transaction(dir)?;
    // Marker last: if the app dies during the restore, the watchdog retries.
    write_marker(dir, MARKER_CANCEL)?;
    let _ = fs::remove_dir_all(dir);
    Ok(report)
}

pub fn cleanup_transaction(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

/// Stop a watchdog without restoring (the layout is already the desired one).
pub fn cancel_and_cleanup(dir: &Path) {
    let _ = write_marker(dir, MARKER_CANCEL);
    let _ = fs::remove_dir_all(dir);
}

pub fn log(dir: &Path, message: &str) {
    let line = format!("[{}] {message}\n", now_rfc3339());
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("watchdog.log"))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Spawn `kanshi-gtk4 --watchdog DIR --timeout N` in a new session, with
/// stdio redirected to `watchdog.log`.
pub fn spawn_watchdog(dir: &Path, timeout_secs: u32) -> Result<i32> {
    let exe = watchdog_exe()?;
    let log_path = dir.join("watchdog.log");
    let log = File::options()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_err = log.try_clone()?;

    let mut command = Command::new(exe);
    command
        .arg("--watchdog")
        .arg(dir)
        .arg("--timeout")
        .arg(timeout_secs.to_string())
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().context("spawning the rollback watchdog")?;
    Ok(child.id() as i32)
}

/// Locate a runnable copy of this program for the watchdog.
///
/// `current_exe()` names the inode the process was started from. Rebuild the
/// binary (or upgrade the package) while the app is open and the kernel starts
/// reporting `/path/to/bin (deleted)`; that path no longer exists, so spawning
/// the watchdog fails and every Apply dies with "No such file or directory".
/// The replacement sitting at the same path is exactly what we want to run.
fn watchdog_exe() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KANSHI_GTK4_WATCHDOG_BIN") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().context("locating the executable for the watchdog")?;
    if let Some(path) = runnable_candidate(&exe) {
        return Ok(path);
    }
    // Installed system-wide: fall back to the same name on PATH.
    let shown = exe.to_string_lossy().into_owned();
    if let Some(name) = exe.file_name().map(|name| name.to_owned()) {
        if let Some(path) = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(&name))
                .find(|candidate| candidate.is_file())
        }) {
            return Ok(path);
        }
    }
    bail!(
        "cannot find a runnable {} for the rollback watchdog: {} no longer exists \
         (the app was rebuilt or upgraded while it was running — restart it)",
        env!("CARGO_PKG_NAME"),
        shown
    )
}

/// Pick a path that can actually be executed, tolerating the ` (deleted)`
/// marker the kernel appends when the on-disk binary was replaced.
fn runnable_candidate(exe: &Path) -> Option<PathBuf> {
    if exe.is_file() {
        return Some(exe.to_path_buf());
    }
    let shown = exe.to_string_lossy();
    let replaced = PathBuf::from(shown.strip_suffix(" (deleted)")?);
    replaced.is_file().then_some(replaced)
}

/// Hidden CLI entry point. Returns the process exit code.
pub fn run_watchdog(dir: &Path, timeout_secs: u32) -> i32 {
    if !dir.exists() {
        return 0;
    }
    let deadline = Instant::now() + Duration::from_secs(u64::from(timeout_secs));
    loop {
        if !dir.exists() {
            return 0;
        }
        if let Some(marker) = read_marker(dir) {
            if marker == MARKER_KEEP || marker == MARKER_CANCEL {
                return 0;
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(marker) = read_marker(dir) {
        if marker == MARKER_KEEP || marker == MARKER_CANCEL {
            return 0;
        }
    }

    log(
        dir,
        &format!("no keep/cancel marker after {timeout_secs}s, restoring the previous layout"),
    );
    match restore_transaction(dir) {
        Ok(report) => {
            log(dir, &format!("restored: {}", report.summary()));
            let _ = write_marker(dir, MARKER_CANCEL);
            util::notify(
                "Display layout reverted",
                "The new layout was not confirmed, so the previous one was restored.",
            );
            1
        }
        Err(err) => {
            log(dir, &format!("restore failed: {err:#}"));
            util::notify(
                "Could not restore the display layout",
                &format!("{err:#}. Try: kanshi-gtk4 --revert {}", dir.display()),
            );
            2
        }
    }
}

/// Create a transaction directory with 0700 permissions.
pub fn create_transaction_dir(root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let _ = fs::set_permissions(root, fs::Permissions::from_mode(0o700));
    let dir = root.join(format!("pending-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Cancels and restores any older pending transaction before a new apply.
pub fn recover_older_transactions(root: &Path, keep: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("pending-") {
            continue;
        }
        // A kept transaction is already the desired state: stop its watchdog
        // and clean up, but never roll back a layout the user confirmed.
        if read_marker(&path).is_some() {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        // Stop its watchdog first, then restore its snapshot.
        let _ = write_marker(&path, MARKER_CANCEL);
        let _ = restore_transaction(&path);
        let _ = fs::remove_dir_all(&path);
    }
}

/// Find abandoned transactions (app and watchdog gone, no marker).
pub fn scan_stale(root: &Path) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return stale;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if !path.is_dir() || !name.to_string_lossy().starts_with("pending-") {
            continue;
        }
        if read_marker(&path).is_some() {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        let snapshot = match read_state(&path) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                let _ = fs::remove_dir_all(&path);
                continue;
            }
        };
        let active = util::pid_alive(snapshot.app_pid) || util::pid_alive(snapshot.watchdog_pid);
        if !active {
            stale.push(path);
        }
    }
    stale
}

/// `--revert DIR` implementation.
pub fn revert_cli(dir: &Path) -> Result<RestoreReport> {
    if !dir.is_dir() {
        bail!("{} is not a transaction directory", dir.display());
    }
    let report = restore_transaction(dir)?;
    let _ = write_marker(dir, MARKER_CANCEL);
    let _ = fs::remove_dir_all(dir);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::output::{Mode, Output, Transform};

    /// A rebuild replaces the binary inode; the kernel then reports
    /// "/path (deleted)" and spawning that path fails. We must fall back to the
    /// file that now lives at the same path.
    #[test]
    fn deleted_executable_falls_back_to_the_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("kanshi-gtk4");
        fs::write(&binary, b"#!/bin/sh\n").unwrap();

        let replaced = PathBuf::from(format!("{} (deleted)", binary.display()));
        assert_eq!(runnable_candidate(&replaced), Some(binary.clone()));
        assert_eq!(runnable_candidate(&binary), Some(binary));
        assert_eq!(runnable_candidate(Path::new("/nonexistent/nope")), None);
    }

    fn output(name: &str, w: u32, h: u32, scale: f64) -> Output {
        let mut mode = Mode::new(w, h, 60_000);
        mode.current = true;
        Output {
            name: name.into(),
            description: String::new(),
            make: None,
            model: None,
            serial: None,
            enabled: true,
            modes: vec![mode],
            position: (0, 0),
            scale,
            transform: Transform::Normal,
            adaptive_sync: false,
        }
    }

    #[test]
    fn marker_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_marker(dir.path()), None);
        write_marker(dir.path(), MARKER_KEEP).unwrap();
        assert_eq!(read_marker(dir.path()).as_deref(), Some(MARKER_KEEP));
    }

    #[test]
    fn watchdog_exits_without_marker_after_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::new(
            EngineKind::Wlr,
            false,
            String::new(),
            String::new(),
            false,
            false,
            0,
            0,
            &[output("eDP-1", 3120, 2080, 2.0)],
        );
        fs::write(
            dir.path().join("state.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        // No wlr-randr available under this name: restore fails, watchdog
        // must report code 2 and keep the directory untouched. Point the
        // notifier at a missing program as well: the failure body names these
        // fake paths, so a real notification would only confuse the user.
        std::env::set_var("KANSHI_GTK4_WLR_RANDR", "/nonexistent/wlr-randr");
        std::env::set_var("KANSHI_GTK4_NOTIFY_SEND", "/nonexistent/notify-send");
        let code = run_watchdog(dir.path(), 0);
        std::env::remove_var("KANSHI_GTK4_WLR_RANDR");
        std::env::remove_var("KANSHI_GTK4_NOTIFY_SEND");
        assert_eq!(code, 2);
        assert!(dir.path().join("watchdog.log").exists());
    }

    #[test]
    fn watchdog_cancel_marker_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), MARKER_CANCEL).unwrap();
        assert_eq!(run_watchdog(dir.path(), 0), 0);
    }

    #[test]
    fn scans_stale_but_not_active() {
        let root = tempfile::tempdir().unwrap();
        let stale = root.path().join("pending-stale");
        fs::create_dir(&stale).unwrap();
        let snapshot = Snapshot::new(
            EngineKind::Wlr,
            false,
            String::new(),
            String::new(),
            false,
            false,
            -1,
            -1,
            &[],
        );
        fs::write(
            stale.join("state.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let found = scan_stale(root.path());
        assert_eq!(found, vec![stale]);
    }
}
