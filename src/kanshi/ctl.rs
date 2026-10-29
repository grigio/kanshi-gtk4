//! kanshi daemon detection and `kanshictl` commands.

use anyhow::{bail, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util;

pub const ENV_KANSHICTL: &str = "KANSHI_GTK4_KANSHICTL";

/// The config kanshi reads when it is started without `-c`/`--config`.
pub fn default_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    xdg_config.join("kanshi/config")
}

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
            "native"
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
        if info.running {
            // kanshi re-reads its *effective* config on `kanshictl reload`.
            // When the daemon has no `-c`, that is the default path; when the
            // app was pointed at another file (`--config`, env override), the
            // write would be silently ignored by the running daemon.
            //
            // Only decide this when the daemon process was actually found: if
            // we cannot inspect its command line we have no evidence that the
            // effective path differs, and disabling the kanshi engine would
            // break a working setup (or a test double).
            if let Some(pid) = info.daemon_pid {
                // `effective_config` is `None` only when `/proc/<pid>/cmdline`
                // cannot be read: then we do not know which file kanshi uses
                // and must not disable a working setup on a guess.
                if let Some(effective) = effective_config(pid) {
                    if !same_path(&effective, managed_config) {
                        info.custom_config = Some(effective.clone());
                        info.note = Some(format!(
                            "kanshi reads {} but the app writes {}; applying through the native protocol",
                            effective.display(),
                            managed_config.display()
                        ));
                    }
                }
            }
        }
        if info.running && !info.config_writable {
            info.note = Some(format!(
                "{} is not writable, applying through the native protocol",
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
///
/// Only processes of the current user that share our `XDG_RUNTIME_DIR` are
/// considered: a second user's daemon, or one started by a system unit with a
/// different runtime directory, would otherwise be picked and its config
/// argument would be mistaken for ours.
pub fn find_daemon_pid() -> Option<i32> {
    let uid = unsafe { libc::geteuid() };
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    let entries = fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim() != "kanshi" {
            continue;
        }
        if process_uid(pid) != Some(uid) {
            continue;
        }
        if let Some(runtime) = &runtime {
            if let Some(environ) = process_environ(pid) {
                let has_value = environ
                    .split('\0')
                    .any(|entry| entry.starts_with("XDG_RUNTIME_DIR="));
                let matches = environ
                    .split('\0')
                    .filter_map(|entry| entry.strip_prefix("XDG_RUNTIME_DIR="))
                    .any(|value| std::ffi::OsStr::new(value) == runtime.as_os_str());
                if has_value && !matches {
                    continue;
                }
            }
        }
        return Some(pid);
    }
    None
}

fn process_uid(pid: i32) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

fn process_environ(pid: i32) -> Option<String> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The config the daemon at `pid` re-reads on `kanshictl reload`: its
/// `-c`/`--config` argument, or kanshi's default when it has none.
///
/// Returns `None` when the command line cannot be read, so callers do not
/// mistake "unknown" for "uses the default config".
pub fn effective_config(pid: i32) -> Option<PathBuf> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    Some(parse_config_arg(&args).unwrap_or_else(default_config_path))
}

/// Read `-c`/`--config` from `/proc/<pid>/cmdline`.
pub fn read_config_arg(pid: i32) -> Option<PathBuf> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    parse_config_arg(&args)
}

fn parse_config_arg<S: AsRef<str>>(args: &[S]) -> Option<PathBuf> {
    let mut iter = args.iter().map(AsRef::as_ref).peekable();
    while let Some(arg) = iter.next() {
        if arg == "-c" || arg == "--config" {
            return iter.peek().map(|value| PathBuf::from(*value));
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
        assert_eq!(
            parse_config_arg(&["kanshi", "-c/tmp/d"]),
            Some("/tmp/d".into())
        );
    }

    #[test]
    fn default_config_follows_xdg_config_home() {
        // The default path is what `effective_config` assumes when the daemon
        // has no `-c`. Both variables are read from the environment.
        let expected = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
            })
            .join("kanshi/config");
        assert_eq!(default_config_path(), expected);
    }
}
