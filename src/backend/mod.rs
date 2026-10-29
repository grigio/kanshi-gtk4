//! Output backends. The native `zwlr_output_manager_v1` backend is the
//! default; the `wlr-randr` shell-out backend is kept as an escape hatch (and
//! for tests) behind `KANSHI_GTK4_BACKEND=wlr-randr`.

pub mod cosmic;
#[cfg(test)]
pub mod fake;
pub mod native;
pub mod wlr_randr;

use anyhow::Result;

use crate::model::layout::LayoutPlan;
use crate::model::output::Output;

/// Force one backend: `native` (default) or `wlr-randr`.
pub const ENV_BACKEND: &str = "KANSHI_GTK4_BACKEND";

/// Backend name for diagnostics (`--verbose`, UI labels).
pub fn selected_name() -> &'static str {
    if cfg!(test) {
        return "wlr-randr";
    }
    match std::env::var(ENV_BACKEND).ok().as_deref() {
        Some("wlr-randr") | Some("wlr") => "wlr-randr",
        _ => "native",
    }
}

/// The backend for this session. The native protocol backend implements the
/// same `zwlr_output_manager_v1` interface `wlr-randr` speaks, so the shell-out
/// version is only useful for debugging or as a fallback.
pub fn create() -> Box<dyn OutputBackend> {
    // Unit tests run inside the developer's session: they must never talk to
    // the real compositor. The integration tests select the wlr backend
    // explicitly instead (the lib there is not compiled with `cfg(test)`).
    if cfg!(test) {
        return Box::new(wlr_randr::WlrRandrBackend::new());
    }
    match std::env::var(ENV_BACKEND).ok().as_deref() {
        Some("wlr-randr") | Some("wlr") => Box::new(wlr_randr::WlrRandrBackend::new()),
        _ => Box::new(native::NativeBackend::new()),
    }
}

#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub supported: bool,
    pub reason: Option<String>,
    pub protocol_version: Option<u32>,
    /// Non-fatal caveat about this compositor: the backend works, but some
    /// features behave differently (Hyprland ignoring positions, for example).
    pub warning: Option<String>,
}

impl BackendInfo {
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            supported: false,
            reason: Some(reason.into()),
            protocol_version: None,
            warning: None,
        }
    }

    pub fn supported(protocol_version: Option<u32>) -> Self {
        Self {
            supported: true,
            reason: None,
            protocol_version,
            warning: compositor_warning(),
        }
    }
}

/// Caveats that apply even when `zwlr_output_manager_v1` is present.
pub fn compositor_warning() -> Option<String> {
    // Hyprland advertises the protocol and applies modes, but positions set
    // through it are ignored: layouts there must come from `monitor=` rules.
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return Some(
            "Hyprland accepts zwlr_output_manager_v1 but ignores the positions set through it. \
             The layout may not match what you see; configure monitors with monitor= rules \
             in hyprland.conf instead."
                .to_string(),
        );
    }
    None
}

/// Abstraction over output-management transports.
pub trait OutputBackend {
    fn detect(&self) -> Result<BackendInfo>;
    fn list(&self) -> Result<Vec<Output>>;
    fn dry_run(&self, plan: &LayoutPlan) -> Result<()>;
    fn apply(&self, plan: &LayoutPlan) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::fake::FakeBackend;
    use super::*;
    use crate::model::layout::LayoutKind;
    use crate::model::output::{Mode, Transform};
    use std::collections::BTreeMap;

    fn output(name: &str) -> Output {
        let mut mode = Mode::new(1920, 1080, 60_000);
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
            scale: 1.0,
            transform: Transform::Normal,
            adaptive_sync: false,
        }
    }

    #[test]
    fn fake_backend_applies_and_reports_state() {
        let backend = FakeBackend::new(vec![output("eDP-1")]);
        let plan = LayoutPlan::from_outputs(
            &backend.snapshot(),
            LayoutKind::Single,
            Some("eDP-1"),
            None,
            &BTreeMap::new(),
        );
        backend.detect().unwrap();
        backend.dry_run(&plan).unwrap();
        backend.apply(&plan).unwrap();
        assert_eq!(backend.snapshot().len(), 1);
    }
}
