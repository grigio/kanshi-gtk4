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
/// Schema version of `state.json`. Bump it when a field changes meaning, and
/// add a migration in [`AppState::load`].
pub const STATE_VERSION: u32 = 1;

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
    /// EDID identity of the physical head (see [`Output::identity`]). Together
    /// with the other outputs it lets a renamed connector adopt the settings
    /// remembered under its old name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
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
    #[serde(default = "current_state_version")]
    pub version: u32,
    #[serde(default)]
    pub internal_outputs: Vec<String>,
    #[serde(default)]
    pub sets: BTreeMap<String, SetState>,
}

fn current_state_version() -> u32 {
    STATE_VERSION
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            internal_outputs: Vec::new(),
            sets: BTreeMap::new(),
        }
    }
}

impl AppState {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut state: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing app state {}", path.display()))?;
                if state.version > STATE_VERSION {
                    anyhow::bail!(
                        "app state {} uses schema version {} but this build understands up to {}. \
                         Refusing to downgrade and lose remembered layouts; upgrade kanshi-gtk4 \
                         or move the file away.",
                        path.display(),
                        state.version,
                        STATE_VERSION
                    );
                }
                // Version 1 is the first schema. Future versions migrate here
                // instead of silently dropping unknown fields.
                state.version = STATE_VERSION;
                Ok(state)
            }
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
        connected: &[Output],
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
                        identity: connected
                            .iter()
                            .find(|output| &output.name == name)
                            .and_then(|output| output.identity()),
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

    /// Adopt a remembered set whose connector names changed but whose
    /// physical heads did not (dock or reboot renaming `DP-1` to `DP-3`).
    ///
    /// Matching uses EDID identities, so it only happens when every output on
    /// both sides reports one and the identities are unique. The set is
    /// re-keyed and its output entries are renamed to the current connectors;
    /// remembered modes, scales and custom-mode flags carry over.
    ///
    /// Returns true when a set was adopted.
    pub fn adopt_renamed_set(&mut self, key: &str, outputs: &[Output]) -> bool {
        if self.sets.contains_key(key) {
            return false;
        }
        let Some(wanted) = output_identity_signature(outputs) else {
            return false;
        };
        let Some(old_key) = self
            .sets
            .iter()
            .find(|(name, set)| {
                *name != key && set_identity_signature(set).as_deref() == Some(wanted.as_str())
            })
            .map(|(name, _)| name.clone())
        else {
            return false;
        };

        // Map identity -> current connector name. `output_identity_signature`
        // already rejected duplicates, so this is unambiguous.
        let by_identity: BTreeMap<String, String> = outputs
            .iter()
            .filter_map(|output| output.identity().map(|id| (id, output.name.clone())))
            .collect();
        let Some(mut set) = self.sets.remove(&old_key) else {
            return false;
        };
        let mut remapped = BTreeMap::new();
        for (old_name, state) in set.outputs {
            let new_name = state
                .identity
                .as_ref()
                .and_then(|id| by_identity.get(id))
                .cloned()
                .unwrap_or(old_name);
            remapped.insert(new_name, state);
        }
        set.outputs = remapped;
        self.sets.insert(key.to_string(), set);
        true
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
                identity: internal.identity(),
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

/// Signature of the live outputs, or `None` when identities are missing or
/// duplicated (two identical monitors without serial numbers are ambiguous).
fn output_identity_signature(outputs: &[Output]) -> Option<String> {
    if outputs.is_empty() {
        return None;
    }
    let mut identities = Vec::with_capacity(outputs.len());
    for output in outputs {
        identities.push(output.identity()?);
    }
    identities.sort();
    let unique = {
        let mut deduped = identities.clone();
        deduped.dedup();
        deduped.len() == identities.len()
    };
    unique.then(|| identities.join("|"))
}

fn set_identity_signature(set: &SetState) -> Option<String> {
    if set.outputs.is_empty() || set.outputs.values().any(|state| state.identity.is_none()) {
        return None;
    }
    let mut identities: Vec<&str> = set
        .outputs
        .values()
        .filter_map(|state| state.identity.as_deref())
        .collect();
    identities.sort_unstable();
    Some(identities.join("|"))
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
        let outputs = [output("eDP-1", 3120, 2080, 2.0)];
        state.record_set(
            "eDP-1",
            &LayoutPlan::from_outputs(
                &outputs,
                LayoutKind::Single,
                Some("eDP-1"),
                None,
                &BTreeMap::new(),
            ),
            &outputs,
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

    fn edid(name: &str, make: &str, model: &str) -> Output {
        let mut output = output(name, 1920, 1080, 1.0);
        output.make = Some(make.into());
        output.model = Some(model.into());
        output
    }

    #[test]
    fn renamed_connectors_adopt_the_remembered_set() {
        let old = vec![
            edid("eDP-1", "EDO", "Panel"),
            edid("DP-1", "Dell", "U2720Q"),
        ];
        let scales = BTreeMap::from([("eDP-1".to_string(), 2.0), ("DP-1".to_string(), 1.5)]);
        let mut state = AppState::default();
        state.record_set(
            "eDP-1+DP-1",
            &LayoutPlan::from_outputs(
                &old,
                LayoutKind::Extend,
                Some("eDP-1"),
                Some("DP-1"),
                &scales,
            ),
            &old,
            Some("eDP-1"),
            Some("DP-1"),
            &scales,
        );
        state
            .sets
            .get_mut("eDP-1+DP-1")
            .unwrap()
            .outputs
            .get_mut("DP-1")
            .unwrap()
            .custom = true;

        // The dock comes back with the monitor on DP-3.
        let new = vec![
            edid("eDP-1", "EDO", "Panel"),
            edid("DP-3", "Dell", "U2720Q"),
        ];
        assert!(state.adopt_renamed_set("eDP-1+DP-3", &new));
        assert!(!state.sets.contains_key("eDP-1+DP-1"));
        let set = &state.sets["eDP-1+DP-3"];
        assert_eq!(set.outputs["DP-3"].mode, "1920x1080@60");
        assert!(set.outputs["DP-3"].custom);
        assert_eq!(set.outputs["DP-3"].desktop_scale(), 1.5);
        assert_eq!(state.remembered_desktop_scales("eDP-1+DP-3")["DP-3"], 1.5);

        // Already adopted: nothing to do.
        assert!(!state.adopt_renamed_set("eDP-1+DP-3", &new));
    }

    #[test]
    fn renamed_laptop_panel_keeps_its_single_set() {
        let old = vec![edid("eDP-1", "EDO", "Panel")];
        let mut state = AppState::default();
        state.record_set(
            "eDP-1",
            &LayoutPlan::from_outputs(
                &old,
                LayoutKind::Single,
                Some("eDP-1"),
                None,
                &BTreeMap::new(),
            ),
            &old,
            Some("eDP-1"),
            None,
            &BTreeMap::new(),
        );
        let new = vec![edid("eDP-2", "EDO", "Panel")];
        assert!(state.adopt_renamed_set("eDP-2", &new));
        assert!(!state.sets.contains_key("eDP-1"));
        // Seeding the single profile must not create a second set now that
        // the renamed one has been adopted.
        state.seed_internal_single(&new);
        assert_eq!(state.sets.len(), 1);
        assert!(state.sets.contains_key("eDP-2"));
    }

    #[test]
    fn ambiguous_identities_do_not_adopt_a_set() {
        let old = vec![edid("DP-1", "Dell", "Same"), edid("DP-2", "Dell", "Same")];
        let mut state = AppState::default();
        state.record_set(
            "DP-1+DP-2",
            &LayoutPlan::from_outputs(
                &old,
                LayoutKind::Extend,
                None,
                Some("DP-2"),
                &BTreeMap::new(),
            ),
            &old,
            None,
            Some("DP-2"),
            &BTreeMap::new(),
        );
        let new = vec![edid("DP-3", "Dell", "Same"), edid("DP-4", "Dell", "Same")];
        assert!(!state.adopt_renamed_set("DP-3+DP-4", &new));
        assert!(state.sets.contains_key("DP-1+DP-2"));
    }

    #[test]
    fn newer_state_schema_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, br#"{"version": 999}"#).unwrap();
        let err = AppState::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("schema version"), "{err:#}");

        // A file written before the field existed counts as the current
        // schema instead of failing to parse.
        fs::write(&path, br#"{"internal_outputs": []}"#).unwrap();
        let state = AppState::load(&path).unwrap();
        assert_eq!(state.version, STATE_VERSION);
    }

    #[test]
    fn seeds_internal_single() {
        let mut state = AppState::default();
        state.seed_internal_single(&[output("eDP-1", 3120, 2080, 2.0)]);
        assert_eq!(state.sets["eDP-1"].default_layout, LayoutKind::Single);
        assert!(state.internal_outputs.contains(&"eDP-1".to_string()));
    }
}
