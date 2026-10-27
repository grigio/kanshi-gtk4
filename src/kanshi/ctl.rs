//! kanshi daemon detection and `kanshictl` commands.

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util;

pub const ENV_KANSHICTL: &str = "KANSHI_GTK4_KANSHICTL";

#[derive(Debug, Clone, Default)]
pub struct KanshiInfo {
    pub installed: bool,
    pub running: bool,
    pub current_profile: Option<String>,
    pub daemon_pid: Option<i32>,
    pub custom_config: Option<PathBuf>,
    pub config_writable: bool,
    pub engine_usable: bool,
    pub note: Option<String>,
}

impl KanshiInfo {
    pub fn engine_label(&self) -> &'static str {
        if self.engine_usable {
            "kanshi"
        } else {
            "wlr-randr"
        }
    }

    pub fn status_text(&self) -> String {
        let base = if self.installed && self.running {
            match &self.current_profile {
                Some(profile) => format!("kanshi running · current profile: {profile}"),
                None => "kanshi running".to_string(),
            }
        } else if self.installed {
            "kanshi not running · this layout applies now but will not re-apply at login"
                .to_string()
        } else {
            "kanshi not installed · this layout applies now but will not persist".to_string()
        };
        match &self.note {
            Some(note) => format!("{base} · {note}"),
            None => base,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KanshiClient {
    program: PathBuf,
}

impl Default for KanshiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl KanshiClient {
    pub fn new() -> Self {
        Self {
            program: util::program_from_env(ENV_KANSHICTL, "kanshictl"),
        }
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Exit 0 means the daemon is running and answered.
    pub fn status(&self) -> Result<(bool, Option<String>)> {
        let out = util::run_command(&self.program, &["status".into()], Duration::from_secs(5))?;
        let current = util::parse_current_profile(&out.stdout);
        Ok((out.success(), current))
    }

    pub fn reload(&self) -> Result<()> {
        let out = util::run_command(&self.program, &["reload".into()], Duration::from_secs(8))?;
        if !out.success() {
            bail!("kanshictl reload failed: {}", out.error_message());
        }
        Ok(())
    }

    /// Full detection. Never fails: unavailable daemons are described in the
    /// returned struct so the UI can explain them.
    pub fn detect(&self, managed_config: &Path) -> KanshiInfo {
        let installed = util::command_exists(&self.program);
        let mut info = KanshiInfo {
            installed,
            config_writable: util::writable_target(managed_config),
            ..KanshiInfo::default()
        };
        if !installed {
            info.note = Some("generated profiles cannot be written".into());
            return info;
        }
        match self.status() {
            Ok((true, profile)) => {
                info.running = true;
                info.current_profile = profile;
            }
            Ok((false, _)) => {
                info.running = false;
            }
            Err(err) => {
                info.running = false;
                info.note = Some(format!("kanshictl status failed: {err:#}"));
            }
        }

        info.daemon_pid = find_daemon_pid();
        if let Some(pid) = info.daemon_pid {
            info.custom_config = read_config_arg(pid);
        }
        if let Some(custom) = &info.custom_config {
            if !same_path(custom, managed_config) {
                info.note = Some(format!(
                    "kanshi uses a custom config ({}), applying through wlr-randr",
                    custom.display()
                ));
            } else {
                info.custom_config = None;
            }
        }
        if info.running && !info.config_writable {
            info.note = Some(format!(
                "{} is not writable, applying through wlr-randr",
                managed_config.display()
            ));
        }
        info.engine_usable = info.running
            && info.custom_config.is_none()
            && info.config_writable
            && !managed_config
                .components()
                .any(|c| c.as_os_str().is_empty());
        info
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canon_a = fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let canon_b = fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    canon_a == canon_b
}

/// Find the kanshi daemon by scanning `/proc`. More reliable than parsing
/// pgrep output, and needs no extra dependency.
pub fn find_daemon_pid() -> Option<i32> {
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim() == "kanshi" {
            return Some(pid);
        }
    }
    None
}

/// Read `-c`/`--config` from `/proc/<pid>/cmdline`.
pub fn read_config_arg(pid: i32) -> Option<PathBuf> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "-c" || arg == "--config" {
            return iter.peek().map(|value| PathBuf::from(value.as_str()));
        }
        if let Some(value) = arg.strip_prefix("--config=") {
            return Some(PathBuf::from(value));
        }
        if let Some(value) = arg.strip_prefix("-c") {
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_long_config_args() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("kanshi");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        // Fake /proc is not writable, so test the parser through a helper
        // command line instead.
        assert_eq!(
            parse_config_arg(&["kanshi", "-c", "/tmp/a"]),
            Some("/tmp/a".into())
        );
        assert_eq!(
            parse_config_arg(&["kanshi", "--config", "/tmp/b"]),
            Some("/tmp/b".into())
        );
        assert_eq!(
            parse_config_arg(&["kanshi", "--config=/tmp/c"]),
            Some("/tmp/c".into())
        );
        assert_eq!(parse_config_arg(&["kanshi"]), None);
    }

    fn parse_config_arg(args: &[&str]) -> Option<PathBuf> {
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            if *arg == "-c" || *arg == "--config" {
                return iter.peek().map(|value| PathBuf::from(**value));
            }
            if let Some(value) = arg.strip_prefix("--config=") {
                return Some(PathBuf::from(value));
            }
        }
        None
    }
}
