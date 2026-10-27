#![allow(dead_code)]

//! Shared fixtures for the integration tests: a mock `wlr-randr`, a mock
//! `kanshictl` and a temporary set of app paths.
//!
//! Environment variables are process-global, so every test that creates a
//! [`MockSession`] holds a mutex for its whole lifetime.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use kanshi_gtk4::apply::ApplyParams;
use kanshi_gtk4::backend::wlr_randr::WlrRandrBackend;
use kanshi_gtk4::config::{AppState, Paths};
use kanshi_gtk4::model::layout::{set_key as build_set_key, LayoutKind, LayoutPlan};
use kanshi_gtk4::model::output::Output;

pub const FIXTURE: &str = include_str!("../fixtures/wlr-randr.json");
/// Internal panel `eDP-2` (2880x1800, scale 1.75) plus `DP-2`
/// (3840x2160, scale 1.5): different names and modes from [`FIXTURE`].
pub const ALT_FIXTURE: &str = include_str!("../fixtures/alt-displays.json");
/// Two external monitors and no built-in panel: `DP-1` (2560x1440, scale
/// 1.25) and `DP-2` (3840x2160, scale 1.5).
pub const DESKTOP_FIXTURE: &str = include_str!("../fixtures/desktop-displays.json");
pub const LEGACY_CONFIG: &str = "# hand-written, must survive\n\n\
profile single {\n\
\toutput eDP-1 {\n\
\t\tmode 3120x2080@120\n\
\t\tposition 0,0\n\
\t\tscale 2\n\
\t}\n\
}\n";

const ENV_VARS: &[&str] = &[
    "KANSHI_GTK4_WLR_RANDR",
    "KANSHI_GTK4_KANSHICTL",
    "KANSHI_GTK4_WAYLAND_INFO",
    "KANSHI_GTK4_WATCHDOG_BIN",
    "KANSHI_GTK4_NOTIFY_SEND",
    "MOCK_WLR_STATE",
    "MOCK_KANSHI_CONFIG",
    "MOCK_WLR_LOG",
    "MOCK_KANSHI_LOG",
    "MOCK_WLR_DRYRUN_FAIL",
    "MOCK_WLR_CANCEL",
    "MOCK_WLR_IGNORE_APPLY",
    "MOCK_KANSHI_STATUS_FAIL",
    "MOCK_KANSHI_RELOAD_FAIL",
    "MOCK_KANSHI_IGNORE_ENABLE",
    "WAYLAND_DISPLAY",
];

fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct MockSession {
    pub dir: tempfile::TempDir,
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub runtime_dir: PathBuf,
    pub display_state: PathBuf,
    pub kanshi_ctl: PathBuf,
    pub wlr_log: PathBuf,
    pub kanshi_log: PathBuf,
    pub set_key: String,
    pub internal: Option<String>,
    pub external: Option<String>,
    _guard: MutexGuard<'static, ()>,
}

impl MockSession {
    pub fn new() -> Self {
        Self::with_fixture(FIXTURE, Some("eDP-1"), Some("DP-1"))
    }

    /// A session with a custom output list: `internal` is the built-in panel
    /// (`None` on a desktop) and `external` is the preset partner.
    pub fn with_fixture(fixture: &str, internal: Option<&str>, external: Option<&str>) -> Self {
        let guard = env_lock();
        for var in ENV_VARS {
            std::env::remove_var(var);
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let bin = root.join("bin");
        fs::create_dir(&bin).expect("bin dir");

        let wlr = copy_script("mock-wlr-randr", &bin.join("wlr-randr"));
        let kanshictl = copy_script("mock-kanshictl", &bin.join("kanshictl"));
        let wayland_info = bin.join("wayland-info");
        fs::write(
            &wayland_info,
            "#!/bin/sh\n\
             echo \"interface: 'zwlr_output_manager_v1', version: 4, name: 42\"\n",
        )
        .expect("wayland-info mock");
        make_executable(&wayland_info);

        let display_state = root.join("displays.json");
        fs::write(&display_state, fixture).expect("display state");
        let names: Vec<String> = WlrRandrBackend::parse_json(fixture.as_bytes())
            .expect("fixture")
            .into_iter()
            .map(|output| output.name)
            .collect();
        let set_key = build_set_key(&names, internal);

        let config_path = root.join("kanshi/config");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(&config_path, LEGACY_CONFIG).expect("legacy config");

        let state_path = root.join("state/state.json");
        let wlr_log = root.join("wlr-randr.log");
        let kanshi_log = root.join("kanshictl.log");

        std::env::set_var("KANSHI_GTK4_WLR_RANDR", &wlr);
        std::env::set_var("KANSHI_GTK4_KANSHICTL", &kanshictl);
        std::env::set_var("KANSHI_GTK4_WAYLAND_INFO", &wayland_info);
        std::env::set_var(
            "KANSHI_GTK4_WATCHDOG_BIN",
            env!("CARGO_BIN_EXE_kanshi-gtk4"),
        );
        std::env::set_var("KANSHI_GTK4_NOTIFY_SEND", "/bin/true");
        std::env::set_var("MOCK_WLR_STATE", &display_state);
        std::env::set_var("MOCK_KANSHI_CONFIG", &config_path);
        std::env::set_var("MOCK_WLR_LOG", &wlr_log);
        std::env::set_var("MOCK_KANSHI_LOG", &kanshi_log);
        std::env::set_var("WAYLAND_DISPLAY", "wayland-test");

        Self {
            dir,
            config_path,
            state_path,
            runtime_dir: root.join("runtime"),
            display_state,
            kanshi_ctl: kanshictl,
            wlr_log,
            kanshi_log,
            set_key,
            internal: internal.map(str::to_string),
            external: external.map(str::to_string),
            _guard: guard,
        }
    }

    /// Use `WlrEngine`: pretend kanshictl is not installed.
    pub fn use_wlr_engine(&self) {
        std::env::set_var("KANSHI_GTK4_KANSHICTL", "/nonexistent/kanshictl");
    }

    pub fn paths(&self) -> Paths {
        Paths {
            config_path: self.config_path.clone(),
            state_path: self.state_path.clone(),
            runtime_dir: self.runtime_dir.clone(),
        }
    }

    pub fn outputs(&self) -> Vec<Output> {
        let bytes = fs::read(&self.display_state).expect("display state");
        WlrRandrBackend::parse_json(&bytes).expect("parsing mock display state")
    }

    pub fn config(&self) -> Vec<u8> {
        fs::read(&self.config_path).expect("config")
    }

    pub fn config_text(&self) -> String {
        fs::read_to_string(&self.config_path).expect("config")
    }

    pub fn pending_dirs(&self) -> Vec<PathBuf> {
        let root = self.paths().transaction_root();
        match fs::read_dir(root) {
            Ok(entries) => entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn extend_plan(&self) -> LayoutPlan {
        let outputs = self.outputs();
        let scales: BTreeMap<String, f64> = outputs
            .iter()
            .map(|output| (output.name.clone(), output.scale))
            .collect();
        LayoutPlan::from_outputs(
            &outputs,
            LayoutKind::Extend,
            self.internal.as_deref(),
            self.external.as_deref(),
            &scales,
        )
    }

    pub fn apply_params(&self, plan: LayoutPlan, timeout_secs: u32) -> ApplyParams {
        let outputs = self.outputs();
        let desktop_scales: BTreeMap<String, f64> = outputs
            .iter()
            .map(|output| (output.name.clone(), output.scale))
            .collect();
        ApplyParams {
            paths: self.paths(),
            plan,
            state: AppState::default(),
            set_key: self.set_key.clone(),
            internal: self.internal.clone(),
            external: self.external.clone(),
            desktop_scales,
            timeout_secs,
        }
    }

    pub fn output(&self, name: &str) -> Output {
        self.outputs()
            .into_iter()
            .find(|output| output.name == name)
            .unwrap_or_else(|| panic!("output {name} not found"))
    }
}

impl Drop for MockSession {
    fn drop(&mut self) {
        for var in ENV_VARS {
            std::env::remove_var(var);
        }
        let _ = fs::remove_dir_all(self.dir.path());
    }
}

fn copy_script(name: &str, target: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name);
    fs::copy(&source, target).unwrap_or_else(|err| {
        panic!(
            "copying {} to {}: {err}",
            source.display(),
            target.display()
        )
    });
    make_executable(target);
    target.to_path_buf()
}

fn make_executable(path: &Path) {
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod");
}
