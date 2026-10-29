//! Rollback snapshot stored in `pending-<uuid>/state.json`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::layout::{LayoutPlan, OutputPlan};
use super::output::{Mode, Output, Transform};

/// Schema version of `pending-*/state.json`.
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    Kanshi,
    Wlr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotMode {
    pub width: u32,
    pub height: u32,
    pub refresh: f64,
    #[serde(default)]
    pub custom: bool,
}

impl From<&Mode> for SnapshotMode {
    fn from(mode: &Mode) -> Self {
        Self {
            width: mode.width,
            height: mode.height,
            refresh: mode.refresh_hz(),
            custom: mode.custom,
        }
    }
}

impl SnapshotMode {
    pub fn to_mode(&self) -> Mode {
        let mut mode = Mode::new(
            self.width,
            self.height,
            (self.refresh * 1000.0).round().max(0.0) as u32,
        );
        mode.custom = self.custom;
        mode
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotOutput {
    pub name: String,
    pub enabled: bool,
    pub mode: SnapshotMode,
    pub position: SnapshotPosition,
    pub scale: f64,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default)]
    pub adaptive_sync: bool,
}

impl SnapshotOutput {
    pub fn from_output(output: &Output) -> Self {
        Self {
            name: output.name.clone(),
            enabled: output.enabled,
            mode: SnapshotMode::from(&output.active_mode()),
            position: SnapshotPosition {
                x: output.position.0,
                y: output.position.1,
            },
            scale: output.scale,
            transform: output.transform,
            adaptive_sync: output.adaptive_sync,
        }
    }

    pub fn to_plan_output(&self) -> OutputPlan {
        OutputPlan {
            enabled: self.enabled,
            mode: self.mode.to_mode(),
            position: (self.position.x, self.position.y),
            scale: self.scale,
            transform: self.transform,
            adaptive_sync: self.adaptive_sync,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub created: String,
    pub engine: EngineKind,
    pub kanshi_running: bool,
    pub config_path: String,
    #[serde(default)]
    pub state_path: String,
    pub config_existed: bool,
    #[serde(default)]
    pub state_existed: bool,
    pub app_pid: i32,
    /// `/proc/<pid>/stat` start time of the app, so a recycled PID is not
    /// mistaken for the process that created the transaction.
    #[serde(default)]
    pub app_start: Option<u64>,
    pub watchdog_pid: i32,
    #[serde(default)]
    pub watchdog_start: Option<u64>,
    pub connected: Vec<String>,
    pub outputs: Vec<SnapshotOutput>,
    /// Native compositor mirrors that existed before the apply, as
    /// `mirrored head -> source`. The output protocol cannot express them, so a revert
    /// has to re-create them explicitly.
    #[serde(default)]
    pub mirrored: Vec<(String, String)>,
}

impl Snapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: EngineKind,
        kanshi_running: bool,
        config_path: String,
        state_path: String,
        config_existed: bool,
        state_existed: bool,
        app_pid: i32,
        watchdog_pid: i32,
        outputs: &[Output],
    ) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            created: crate::config::now_rfc3339(),
            engine,
            kanshi_running,
            config_path,
            state_path,
            config_existed,
            state_existed,
            app_pid,
            app_start: crate::util::process_start_time(app_pid),
            watchdog_pid,
            watchdog_start: crate::util::process_start_time(watchdog_pid),
            connected: outputs.iter().map(|o| o.name.clone()).collect(),
            outputs: outputs.iter().map(SnapshotOutput::from_output).collect(),
            mirrored: Vec::new(),
        }
    }

    pub fn record_watchdog(&mut self, pid: i32) {
        self.watchdog_pid = pid;
        self.watchdog_start = crate::util::process_start_time(pid);
    }

    /// Snapshot outputs as a complete plan. Missing outputs are never
    /// re-enabled; heads connected since the snapshot are added with their
    /// live settings so the restore always sends a full configuration.
    pub fn restore_plan(&self, connected: &[Output]) -> LayoutPlan {
        let mut outputs = BTreeMap::new();
        for snap in &self.outputs {
            if connected.iter().any(|o| o.name == snap.name) {
                outputs.insert(snap.name.clone(), snap.to_plan_output());
            }
        }
        for output in connected {
            outputs
                .entry(output.name.clone())
                .or_insert_with(|| OutputPlan::from_output(output));
        }
        LayoutPlan {
            kind: super::layout::LayoutKind::Custom,
            outputs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::output::Mode;

    #[test]
    fn roundtrip_snapshot_plan() {
        let output = Output {
            name: "eDP-1".into(),
            description: "EDO".into(),
            make: Some("EDO".into()),
            model: None,
            serial: None,
            enabled: true,
            modes: vec![Mode::new(3120, 2080, 120_000)],
            position: (0, 0),
            scale: 2.0,
            transform: Transform::Normal,
            adaptive_sync: false,
        };
        let snap = Snapshot::new(
            EngineKind::Kanshi,
            true,
            "/tmp/config".into(),
            "/tmp/state.json".into(),
            true,
            true,
            1,
            2,
            std::slice::from_ref(&output),
        );
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.connected, vec!["eDP-1"]);
        assert_eq!(parsed.outputs[0].mode.width, 3120);
        let plan = parsed.restore_plan(&[output]);
        assert_eq!(plan.outputs["eDP-1"].mode.refresh_mhz, 120_000);
    }
}
