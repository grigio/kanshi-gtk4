//! Output backends. v1 ships the `wlr-randr` shell-out backend.

pub mod cosmic;
#[cfg(test)]
pub mod fake;
pub mod wlr_randr;

use anyhow::Result;

use crate::model::layout::LayoutPlan;
use crate::model::output::Output;

#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub supported: bool,
    pub reason: Option<String>,
    pub protocol_version: Option<u32>,
}

impl BackendInfo {
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            supported: false,
            reason: Some(reason.into()),
            protocol_version: None,
        }
    }
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
