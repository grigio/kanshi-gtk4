//! Discover the live session once, then hand it to the UI or `--print-config`.

use anyhow::Result;
use std::collections::BTreeMap;

use crate::backend::{self, BackendInfo};
use crate::config::{AppState, Paths};
use crate::kanshi::ctl::{KanshiClient, KanshiInfo};
use crate::model::layout::{detect_layout, set_key, LayoutKind, LayoutPlan};
use crate::model::output::{is_internal_name, Output};

pub struct Session {
    pub paths: Paths,
    pub backend_info: BackendInfo,
    pub kanshi_info: KanshiInfo,
    pub outputs: Vec<Output>,
    pub state: AppState,
    pub internal: Option<String>,
    pub external: Option<String>,
    pub set_key: String,
    pub desktop_scales: BTreeMap<String, f64>,
    pub kind: LayoutKind,
    pub plan: LayoutPlan,
}

impl Session {
    pub fn load(paths: Paths) -> Result<Self> {
        let backend = backend::create();
        let backend_info = backend.detect()?;
        let outputs = if backend_info.supported {
            backend.list()?
        } else {
            Vec::new()
        };

        let mut state = AppState::load(&paths.state_path)?;

        let internal = outputs
            .iter()
            .find(|o| o.is_internal())
            .map(|o| o.name.clone())
            .or_else(|| {
                state
                    .internal_outputs
                    .iter()
                    .find(|name| outputs.iter().any(|o| &o.name == *name))
                    .cloned()
            });
        // Keep only names that are actually connected.
        let internal = internal.filter(|name| outputs.iter().any(|o| &o.name == name));
        let names: Vec<String> = outputs.iter().map(|o| o.name.clone()).collect();
        // The preset partner of the anchor display. With a built-in panel that
        // is the first external output; on a machine without one (desktop,
        // tablet) the anchor is the first output in name order and the partner
        // is the second, so two external monitors can still Extend and Mirror.
        let mut ordered = names.clone();
        ordered.sort();
        let external = match &internal {
            Some(internal) => ordered
                .iter()
                .find(|name| *name != internal && !is_internal_name(name))
                .cloned(),
            None => ordered.get(1).cloned(),
        };
        let key = if names.is_empty() {
            String::new()
        } else {
            set_key(&names, internal.as_deref())
        };

        // Connector names are not stable across docks and reboots. When the
        // same physical heads come back under other names, adopt the set
        // remembered for them so per-output settings survive the rename.
        if !outputs.is_empty() {
            state.adopt_renamed_set(&key, &outputs);
        }
        // Seed after adoption: if the panel is the whole live set and its
        // connector was renamed, the adopted set already owns that key.
        state.seed_internal_single(&outputs);

        let mut desktop_scales = state.remembered_desktop_scales(&key);
        for output in &outputs {
            desktop_scales
                .entry(output.name.clone())
                .or_insert(output.scale);
        }

        let kind = detect_layout(&outputs, internal.as_deref(), external.as_deref());
        let live_scales: BTreeMap<String, f64> =
            outputs.iter().map(|o| (o.name.clone(), o.scale)).collect();
        let mut plan = LayoutPlan::from_outputs(
            &outputs,
            kind,
            internal.as_deref(),
            external.as_deref(),
            &live_scales,
        );
        // The compositor lists a custom mode like any other mode while it is
        // active, so recover the "custom" flag from the saved state. Without
        // this, regenerating the kanshi config after a reboot would drop
        // `mode --custom` and fail on the next login.
        if let Some(set) = state.sets.get(&key) {
            for (name, output_plan) in &mut plan.outputs {
                if let Some(saved) = set.outputs.get(name) {
                    if saved.custom && saved.mode == output_plan.mode.to_kanshi() {
                        output_plan.mode.custom = true;
                    }
                }
            }
        }
        if !outputs.is_empty() {
            state.record_set(
                &key,
                &plan,
                &outputs,
                internal.as_deref(),
                external.as_deref(),
                &desktop_scales,
            );
        }

        let kanshi_info = KanshiClient::new().detect(&paths.config_path);

        Ok(Self {
            paths,
            backend_info,
            kanshi_info,
            outputs,
            state,
            internal,
            external,
            set_key: key,
            desktop_scales,
            kind,
            plan,
        })
    }
}
