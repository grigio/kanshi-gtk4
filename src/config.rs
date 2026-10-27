//! Paths and persistent application state.

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::layout::{LayoutKind, LayoutPlan};
use crate::model::output::{is_internal_name, Output, Transform};
#[cfg(test)]
use crate::util;

pub const ENV_STATE_DIR: &str = "KANSHI_GTK4_STATE_DIR";
pub const ENV_KANSHI_CONFIG: &str = "KANSHI_GTK4_KANSHI_CONFIG";
pub const APP_ID: &str = "io.github.kanshi_gtk4";

pub fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Resolved filesystem locations used by the app.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    pub fn discover(config_override: Option<PathBuf>, state_override: Option<PathBuf>) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let uid = unsafe { libc::geteuid() };
                PathBuf::from(format!("/tmp/kanshi-gtk4-{uid}"))
            });

        let config_path = config_override
            .or_else(|| std::env::var_os(ENV_KANSHI_CONFIG).map(PathBuf::from))
            .unwrap_or_else(|| xdg_config.join("kanshi/config"));
        let state_dir = state_override
            .or_else(|| std::env::var_os(ENV_STATE_DIR).map(PathBuf::from))
            .unwrap_or_else(|| xdg_config.join("kanshi-gtk4"));

        Self {
            state_path: state_dir.join("state.json"),
            config_path,
            runtime_dir: runtime,
        }
    }

    pub fn transaction_root(&self) -> PathBuf {
        self.runtime_dir.join("kanshi-gtk4")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputState {
    /// Kanshi-style mode string, e.g. `3120x2080@120`.
    pub mode: String,
    /// The mode is not advertised by the panel and needs `mode --custom`.
    #[serde(default)]
    pub custom: bool,
    pub scale: f64,
    #[serde(default)]
    pub desktop_scale: Option<f64>,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default)]
    pub position: Option<(i32, i32)>,
    #[serde(default)]
    pub adaptive_sync: bool,
}

impl OutputState {
    pub fn desktop_scale(&self) -> f64 {
        self.desktop_scale.unwrap_or(self.scale)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetState {
    pub last_seen: String,
    #[serde(default = "default_layout_kind")]
    pub default_layout: LayoutKind,
    pub outputs: BTreeMap<String, OutputState>,
}

fn default_layout_kind() -> LayoutKind {
    LayoutKind::Custom
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub version: u32,
    #[serde(default)]
    pub internal_outputs: Vec<String>,
    #[serde(default)]
    pub sets: BTreeMap<String, SetState>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            version: 1,
            internal_outputs: Vec::new(),
            sets: BTreeMap::new(),
        }
    }
}

impl AppState {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing app state {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading app state {}", path.display())),
        }
    }

    #[cfg(test)]
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        util::atomic_write(path, &bytes)?;
        util::write_backup(path, &path.with_extension("json.bak"))?;
        Ok(())
    }

    pub fn set_mut_or_default(&mut self, key: &str) -> &mut SetState {
        self.sets
            .entry(key.to_string())
            .or_insert_with(|| SetState {
                last_seen: now_rfc3339(),
                default_layout: LayoutKind::Custom,
                outputs: BTreeMap::new(),
            })
    }

    /// Record the current arrangement for a set. Called on Apply (in memory)
    /// and persisted on Keep.
    pub fn record_set(
        &mut self,
        key: &str,
        plan: &LayoutPlan,
        internal: Option<&str>,
        external: Option<&str>,
        desktop_scales: &BTreeMap<String, f64>,
    ) {
        let default_layout = plan.kind;
        let mut internals: Vec<String> = Vec::new();
        {
            let set = self.set_mut_or_default(key);
            set.last_seen = now_rfc3339();
            set.default_layout = default_layout;
            set.outputs.clear();
            for (name, po) in &plan.outputs {
                let desktop = if plan.kind == LayoutKind::Mirror && Some(name.as_str()) == external
                {
                    desktop_scales.get(name).copied().unwrap_or(po.scale)
                } else {
                    po.scale
                };
                set.outputs.insert(
                    name.clone(),
                    OutputState {
                        mode: po.mode.to_kanshi(),
                        custom: po.mode.custom,
                        scale: po.scale,
                        desktop_scale: Some(desktop),
                        transform: po.transform,
                        position: Some(po.position),
                        adaptive_sync: po.adaptive_sync,
                    },
                );
                if is_internal_name(name) {
                    internals.push(name.clone());
                }
            }
        }
        for name in internals {
            if !self.internal_outputs.contains(&name) {
                self.internal_outputs.push(name);
            }
        }
        // Keep the vector deterministic so state files do not churn.
        self.internal_outputs.sort();
        if let Some(internal) = internal {
            if !self.internal_outputs.iter().any(|n| n == internal) {
                self.internal_outputs.push(internal.to_string());
                self.internal_outputs.sort();
            }
        }
    }

    /// Seed a single-internal profile from the live panel the first time the
    /// app runs, so undocking works even if the laptop was never seen alone.
    pub fn seed_internal_single(&mut self, outputs: &[Output]) {
        let Some(internal) = outputs.iter().find(|o| o.is_internal()) else {
            return;
        };
        if !self.internal_outputs.iter().any(|n| n == &internal.name) {
            self.internal_outputs.push(internal.name.clone());
            self.internal_outputs.sort();
        }
        let key = internal.name.clone();
        if self.sets.contains_key(&key) {
            return;
        }
        let mut set_outputs = BTreeMap::new();
        set_outputs.insert(
            internal.name.clone(),
            OutputState {
                mode: internal.active_mode().to_kanshi(),
                custom: false,
                scale: internal.scale,
                desktop_scale: Some(internal.scale),
                transform: internal.transform,
                position: Some((0, 0)),
                adaptive_sync: internal.adaptive_sync,
            },
        );
        self.sets.insert(
            key,
            SetState {
                last_seen: now_rfc3339(),
                default_layout: LayoutKind::Single,
                outputs: set_outputs,
            },
        );
    }

    /// Desktop scales remembered for the current output set, keyed by name.
    pub fn remembered_desktop_scales(&self, key: &str) -> BTreeMap<String, f64> {
        self.sets
            .get(key)
            .map(|set| {
                set.outputs
                    .iter()
                    .map(|(name, state)| (name.clone(), state.desktop_scale()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::output::Mode;
    use tempfile::tempdir;

    fn output(name: &str, w: u32, h: u32, scale: f64) -> Output {
        Output {
            name: name.into(),
            description: String::new(),
            make: None,
            model: None,
            serial: None,
            enabled: true,
            modes: vec![Mode::new(w, h, 60_000)],
            position: (0, 0),
            scale,
            transform: Transform::Normal,
            adaptive_sync: false,
        }
    }

    #[test]
    fn state_roundtrip_and_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = AppState::default();
        state.record_set(
            "eDP-1",
            &LayoutPlan::from_outputs(
                &[output("eDP-1", 3120, 2080, 2.0)],
                LayoutKind::Single,
                Some("eDP-1"),
                None,
                &BTreeMap::new(),
            ),
            Some("eDP-1"),
            None,
            &BTreeMap::new(),
        );
        state.save(&path).unwrap();
        assert!(path.with_extension("json.bak").exists());
        let loaded = AppState::load(&path).unwrap();
        assert_eq!(loaded.sets["eDP-1"].default_layout, LayoutKind::Single);
        assert_eq!(loaded.sets["eDP-1"].outputs["eDP-1"].mode, "3120x2080@60");
    }

    #[test]
    fn seeds_internal_single() {
        let mut state = AppState::default();
        state.seed_internal_single(&[output("eDP-1", 3120, 2080, 2.0)]);
        assert_eq!(state.sets["eDP-1"].default_layout, LayoutKind::Single);
        assert!(state.internal_outputs.contains(&"eDP-1".to_string()));
    }
}
