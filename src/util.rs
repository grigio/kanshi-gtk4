//! Small filesystem and subprocess helpers.

use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// Result of running a subprocess with a timeout.
#[derive(Debug)]
pub struct CmdOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn error_message(&self) -> String {
        let stderr = self.stderr.trim();
        if !stderr.is_empty() {
            stderr.to_string()
        } else {
            format!("process exited with {}", self.status)
        }
    }
}

/// Run a process without a shell, capturing output, killing it on timeout.
pub fn run_command(program: &Path, args: &[OsString], timeout: Duration) -> Result<CmdOutput> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Put the child in its own process group so a timeout can kill any
        // helper it spawned as well. Without this a grandchild can keep the
        // stdout pipe open and the reader threads block forever.
        .process_group(0)
        .spawn()
        .with_context(|| format!("running {}", program.display()))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = stdout {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send((0u8, buf));
        });
    }
    if let Some(mut pipe) = stderr {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            let _ = tx.send((1u8, buf));
        });
    }
    drop(tx);

    let mut out = Vec::new();
    let mut err = Vec::new();
    let status = match child.wait_timeout(timeout)? {
        Some(status) => {
            // The child has exited, so its write ends of both pipes are
            // closed. Drain both readers instead of polling for a few
            // hundred milliseconds: a slow but successful command must not
            // lose its output. The grace period only guards against a
            // grandchild that escaped the process group and still holds a
            // pipe open.
            drain_pipes(&rx, &mut out, &mut err, Duration::from_secs(5));
            Some(status)
        }
        None => {
            // Kill the whole group: the direct child may have exited already
            // while a helper still holds the pipes.
            let pgid = -(child.id() as i32);
            unsafe {
                libc::kill(pgid, libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            drain_pipes(&rx, &mut out, &mut err, Duration::from_secs(2));
            None
        }
    };

    match status {
        Some(status) => Ok(CmdOutput {
            status,
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        }),
        None => bail!(
            "{} timed out after {}s",
            program.display(),
            timeout.as_secs_f64().round() as u64
        ),
    }
}

/// Wait for both pipe readers, up to `wait`. Output that arrives later is
/// still collected by the detached reader thread, but cannot be returned.
fn drain_pipes(
    rx: &mpsc::Receiver<(u8, Vec<u8>)>,
    out: &mut Vec<u8>,
    err: &mut Vec<u8>,
    wait: Duration,
) {
    let deadline = Instant::now() + wait;
    let mut received = 0;
    while received < 2 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((tag, buf)) => {
                received += 1;
                if tag == 0 {
                    *out = buf;
                } else {
                    *err = buf;
                }
            }
            Err(_) => break,
        }
    }
}

/// True when the program can be found (absolute path or `PATH`).
pub fn command_exists(program: &Path) -> bool {
    if program.components().count() > 1 || program.is_absolute() {
        return program.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
    })
}

/// Environment override for a test/injection program.
pub fn program_from_env(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

pub fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Resolve a symlink so writes go to the real target.
fn resolve_target(path: &Path) -> PathBuf {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Atomic write: temp file in the same directory, fsync, rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = resolve_target(path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = parent.join(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));

    let existing = fs::metadata(&target).ok();
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        if let Some(meta) = &existing {
            let _ = file.set_permissions(meta.permissions());
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target).with_context(|| format!("replacing {}", target.display()))?;
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Copy the current contents of `path` to `backup` atomically.
pub fn write_backup(path: &Path, backup: &Path) -> Result<()> {
    if let Some(bytes) = read_optional(path)? {
        atomic_write(backup, &bytes)?;
    }
    Ok(())
}

/// True when the file exists and can be opened for writing, or the directory
/// is writable so the file can be created.
pub fn writable_target(path: &Path) -> bool {
    let target = resolve_target(path);
    if target.exists() {
        return OpenOptions::new().append(true).open(&target).is_ok();
    }
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // A missing parent directory is fine: atomic_write creates it. Walk up to
    // the nearest existing directory, then check that entries can be created
    // there. Opening the directory itself is not a writability test: Linux
    // refuses to open directories for writing even when they are writable.
    let Some(parent) = parent.ancestors().find(|p| p.exists()) else {
        return false;
    };
    if !parent.is_dir() {
        return false;
    }
    let probe = parent.join(format!(
        ".kanshi-gtk4-write-test-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    match OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Kernel start time of a process, in clock ticks since boot (field 22 of
/// `/proc/<pid>/stat`). Together with the PID this identifies a process
/// uniquely, so a recycled PID is not mistaken for the original one.
pub fn process_start_time(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field is parenthesised and may contain spaces and `)`, so
    // split after the last `)`.
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// True when `pid` is alive and, if a start time is known, still the same
/// process that was recorded.
pub fn pid_alive_since(pid: i32, start_time: Option<u64>) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    match start_time {
        Some(expected) => process_start_time(pid) == Some(expected),
        None => true,
    }
}

/// An exclusive `flock` on a transaction directory.
///
/// The UI and the detached watchdog both restore the same transaction. The
/// lock serialises `finalize_keep`, `restore_transaction` and the watchdog's
/// timeout path so a keep cannot be overtaken by a restore at the 10-second
/// boundary. `flock` locks are released automatically when the file closes.
pub struct DirLock {
    file: File,
}

impl DirLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        // Never create the directory: a transaction that has already been
        // kept or reverted must not be resurrected by a waiter that lost the
        // race (the watchdog would then try to restore a snapshot that is
        // already gone).
        if !dir.is_dir() {
            bail!("{} is not a transaction directory", dir.display());
        }
        let path = dir.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("locking {}", path.display()));
        }
        Ok(Self { file })
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        unsafe {
            libc::flock(fd, libc::LOCK_UN);
        }
    }
}

pub fn notify(summary: &str, body: &str) {
    // Unit tests exercise the failure paths that notify the user (the
    // watchdog restoring with a missing backend helper, for example). They must
    // never pop a real desktop notification on the developer's session.
    if cfg!(test) {
        return;
    }
    let program = program_from_env("KANSHI_GTK4_NOTIFY_SEND", "notify-send");
    if !command_exists(&program) {
        return;
    }
    let _ = Command::new(program)
        .arg("--app-name=kanshi-gtk4")
        .arg(summary)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Parse `Current profile: name` from kanshictl status output.
pub fn parse_current_profile(stdout: &str) -> Option<String> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Current profile:")
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sub/file.txt");
        atomic_write(&path, b"one").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        atomic_write(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn writable_target_accepts_a_new_file_in_a_writable_directory() {
        let dir = tempdir().unwrap();
        assert!(writable_target(&dir.path().join("config")));
        assert!(writable_target(&dir.path().join("sub/dir/config")));

        let existing = dir.path().join("existing");
        fs::write(&existing, b"x").unwrap();
        assert!(writable_target(&existing));
        let _ = fs::remove_file(existing);
    }

    #[test]
    fn parses_profile_name() {
        assert_eq!(
            parse_current_profile("Current profile: single\n"),
            Some("single".into())
        );
        assert_eq!(parse_current_profile(""), None);
    }

    #[test]
    fn run_command_collects_large_output_without_polling_losses() {
        let out = run_command(
            Path::new("/bin/sh"),
            &["-c".into(), "head -c 300000 /dev/zero | tr '\\0' x".into()],
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[test]
    fn run_command_times_out_and_kills_the_process_group() {
        let dir = tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        // The shell spawns a child that outlives it and keeps stdout open.
        let script = format!("sleep 60 & echo $! > {}; wait", pid_file.to_string_lossy());
        let started = Instant::now();
        let err = run_command(
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
        let pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The grandchild shares the process group, so it must be gone too.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!pid_alive(pid), "helper {pid} survived the timeout");
    }

    #[test]
    fn process_start_time_identifies_this_process() {
        let pid = std::process::id() as i32;
        let start = process_start_time(pid).expect("current process has a start time");
        assert!(start > 0);
        assert!(pid_alive_since(pid, Some(start)));
        // A different start time is a recycled PID, not this process.
        assert!(!pid_alive_since(pid, Some(start + 1)));
        assert!(!pid_alive_since(-1, None));
    }

    #[test]
    fn dir_lock_serialises_waiters() {
        let dir = tempdir().unwrap();
        let lock = DirLock::acquire(dir.path()).unwrap();
        let path = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let second = DirLock::acquire(&path).unwrap();
            drop(second);
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "the second lock was granted while the first was held"
        );
        drop(lock);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the waiter gets the lock after it is released");
        waiter.join().unwrap();
    }
}
