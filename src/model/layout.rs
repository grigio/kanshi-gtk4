//! Preset geometry, layout plans and validation.
//!
//! All positions are in the compositor's global logical coordinate space.
//! A [`LayoutPlan`] always contains every connected output, including
//! disabled ones, because `zwlr_output_manager_v1` requires a complete
//! configuration.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::output::{Mode, Output, Transform};

/// The preset currently being edited. `Custom` keeps live positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutKind {
    Single,
    /// External display next to the laptop panel. The user can drag it to any
    /// position, which switches the plan to `Custom`.
    #[serde(alias = "extend-right", alias = "extend-left")]
    Extend,
    Mirror,
    ExternalOnly,
    Custom,
}

impl LayoutKind {
    pub const PRESETS: [LayoutKind; 4] = [
        LayoutKind::Single,
        LayoutKind::Extend,
        LayoutKind::Mirror,
        LayoutKind::ExternalOnly,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LayoutKind::Single => "single",
            LayoutKind::Extend => "extend",
            LayoutKind::Mirror => "mirror",
            LayoutKind::ExternalOnly => "external-only",
            LayoutKind::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LayoutKind::Single => "Laptop only",
            LayoutKind::Extend => "Extend",
            LayoutKind::Mirror => "Mirror",
            LayoutKind::ExternalOnly => "External only",
            LayoutKind::Custom => "Custom",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            LayoutKind::Single => "Use only the laptop panel",
            LayoutKind::Extend => "Put the external display next to the laptop",
            LayoutKind::Mirror => "Show the same picture on both displays",
            LayoutKind::ExternalOnly => "Turn the laptop panel off",
            LayoutKind::Custom => "Keep the current positions",
        }
    }
}

/// One output inside a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputPlan {
    pub enabled: bool,
    pub mode: Mode,
    pub position: (i32, i32),
    pub scale: f64,
    pub transform: Transform,
    pub adaptive_sync: bool,
}

impl OutputPlan {
    pub fn from_output(output: &Output) -> Self {
        Self {
            enabled: output.enabled,
            mode: output.active_mode(),
            position: output.position,
            scale: output.scale,
            transform: output.transform,
            adaptive_sync: output.adaptive_sync,
        }
    }

    pub fn logical_size(&self) -> (i32, i32) {
        self.mode.logical_size(self.scale, self.transform)
    }
}

/// A complete, applyable configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutPlan {
    pub kind: LayoutKind,
    pub outputs: BTreeMap<String, OutputPlan>,
}

impl LayoutPlan {
    /// Build a plan from live outputs. Preset geometry is recomputed unless
    /// the kind is `Custom`.
    pub fn from_outputs(
        outputs: &[Output],
        kind: LayoutKind,
        internal: Option<&str>,
        external: Option<&str>,
        desktop_scales: &BTreeMap<String, f64>,
    ) -> Self {
        let mut plan = Self {
            kind,
            outputs: outputs
                .iter()
                .map(|o| (o.name.clone(), OutputPlan::from_output(o)))
                .collect(),
        };
        plan.rebuild(internal, external, desktop_scales);
        plan
    }

    /// Recompute derived positions/scales for the current preset.
    pub fn rebuild(
        &mut self,
        internal: Option<&str>,
        external: Option<&str>,
        desktop_scales: &BTreeMap<String, f64>,
    ) {
        if self.kind == LayoutKind::Custom {
            return;
        }
        // The anchor is the built-in panel when there is one. Machines without
        // a recognised built-in panel (desktops, tablets) anchor on the first
        // output instead, and the preset partner becomes the second one, so
        // Extend and Mirror work there too.
        let primary = internal
            .filter(|name| self.outputs.contains_key(*name))
            .map(str::to_string)
            .or_else(|| {
                self.outputs
                    .keys()
                    .find(|name| Some(name.as_str()) != external)
                    .cloned()
            });
        let second = external
            .filter(|name| Some(*name) != primary.as_deref() && self.outputs.contains_key(*name));

        // Restore remembered desktop scales when leaving mirror mode.
        if self.kind != LayoutKind::Mirror {
            for name in [primary.as_deref(), second].into_iter().flatten() {
                if let (Some(plan), Some(desktop)) =
                    (self.outputs.get_mut(name), desktop_scales.get(name))
                {
                    plan.scale = *desktop;
                }
            }
        }

        let Some(primary) = primary else {
            return;
        };
        let primary = primary.as_str();

        match self.kind {
            LayoutKind::Single => {
                if let Some(p) = self.outputs.get_mut(primary) {
                    p.enabled = true;
                    p.position = (0, 0);
                }
                if let Some(second) = second {
                    if let Some(p) = self.outputs.get_mut(second) {
                        p.enabled = false;
                    }
                }
            }
            LayoutKind::ExternalOnly => {
                if let Some(second) = second {
                    if let Some(p) = self.outputs.get_mut(primary) {
                        p.enabled = false;
                    }
                    if let Some(p) = self.outputs.get_mut(second) {
                        p.enabled = true;
                        p.position = (0, 0);
                    }
                } else if let Some(p) = self.outputs.get_mut(primary) {
                    p.enabled = true;
                    p.position = (0, 0);
                }
            }
            LayoutKind::Extend => {
                let Some(second) = second else { return };
                let primary_w = {
                    let Some(p) = self.outputs.get(primary) else {
                        return;
                    };
                    p.mode.logical_size(p.scale, p.transform).0
                };
                if let Some(p) = self.outputs.get_mut(primary) {
                    p.enabled = true;
                    p.position = (0, 0);
                }
                if let Some(p) = self.outputs.get_mut(second) {
                    p.enabled = true;
                    if let Ok(x) = i32::try_from(i64::from(primary_w)) {
                        p.position = (x, 0);
                    }
                }
            }
            LayoutKind::Mirror => {
                let Some(second) = second else { return };
                let fit = {
                    let Some(p) = self.outputs.get(primary) else {
                        return;
                    };
                    let Some(s) = self.outputs.get(second) else {
                        return;
                    };
                    mirror_scale(s, p)
                };
                if let Some(p) = self.outputs.get_mut(primary) {
                    p.enabled = true;
                    p.position = (0, 0);
                }
                if let Some(p) = self.outputs.get_mut(second) {
                    p.enabled = true;
                    p.position = (0, 0);
                    p.scale = fit;
                }
            }
            LayoutKind::Custom => {}
        }
    }

    pub fn enabled_count(&self) -> usize {
        self.outputs.values().filter(|o| o.enabled).count()
    }

    /// Shift the enabled outputs so the layout starts at `(0, 0)`.
    ///
    /// An output layout is only defined up to a global translation: nothing
    /// visually changes when every display moves by the same offset. Some
    /// compositors normalise the layout themselves — COSMIC stores positions
    /// as unsigned values and offsets any configuration with a negative
    /// coordinate (`HDMI at -1560,0` comes back as `HDMI 0,0, eDP 1560,0`),
    /// while labwc forwards the values unchanged.
    ///
    /// Normalising before applying keeps the requested plan, the written
    /// kanshi profile and the compositor's state identical on both kinds of
    /// compositor, so the post-apply verification cannot fail on a layout that
    /// was in fact accepted.
    pub fn normalize_origin(&mut self) {
        let origin = self.outputs.values().filter(|output| output.enabled).fold(
            None,
            |origin: Option<(i32, i32)>, output| {
                Some(match origin {
                    Some((x, y)) => (x.min(output.position.0), y.min(output.position.1)),
                    None => output.position,
                })
            },
        );

        let Some((origin_x, origin_y)) = origin else {
            return;
        };
        if origin_x == 0 && origin_y == 0 {
            return;
        }

        for output in self.outputs.values_mut().filter(|output| output.enabled) {
            output.position = (output.position.0 - origin_x, output.position.1 - origin_y);
        }
    }

    /// Compare a plan with the live compositor state.
    pub fn matches_live(&self, outputs: &[Output]) -> bool {
        self.matches_live_with(outputs, true)
    }

    /// Like [`Self::matches_live`] but ignores adaptive sync, which some
    /// compositors silently refuse to change.
    pub fn matches_live_relaxed(&self, outputs: &[Output]) -> bool {
        self.matches_live_with(outputs, false)
    }

    fn matches_live_with(&self, outputs: &[Output], check_adaptive_sync: bool) -> bool {
        if self.outputs.len() != outputs.len() {
            return false;
        }
        // Positions are only comparable up to a global translation: a
        // compositor may move the whole layout into the positive quadrant, so
        // the first enabled output fixes the offset the rest must share.
        let mut anchor: Option<(i32, i32)> = None;
        outputs.iter().all(|output| {
            let Some(plan) = self.outputs.get(&output.name) else {
                return false;
            };
            if plan.enabled != output.enabled {
                return false;
            }
            // A disabled head has no meaningful mode, scale or position:
            // compositors keep the last values or reset them arbitrarily.
            if !plan.enabled {
                return true;
            }
            if !plan.mode.same_timing(&output.active_mode())
                || (plan.scale - output.scale).abs() > 0.01
                || plan.transform != output.transform
                || (check_adaptive_sync && plan.adaptive_sync != output.adaptive_sync)
            {
                return false;
            }
            let offset = (
                plan.position.0 - output.position.0,
                plan.position.1 - output.position.1,
            );
            match anchor {
                None => {
                    anchor = Some(offset);
                    true
                }
                Some(expected) => {
                    (offset.0 - expected.0).abs() <= 2 && (offset.1 - expected.1).abs() <= 2
                }
            }
        })
    }
}

/// Format a scale without trailing zeros: `2`, `1.5`, `1.231`.
pub fn format_scale(scale: f64) -> String {
    let rounded = (scale * 1000.0).round() / 1000.0;
    if (rounded - rounded.round()).abs() < 0.0005 {
        format!("{}", rounded.round() as i64)
    } else {
        let mut s = format!("{rounded:.3}");
        while s.ends_with('0') {
            s.pop();
        }
        s
    }
}

/// Compute the external scale for an approximate mirror: make the external
/// logical area fit inside the primary logical area.
pub fn mirror_scale(external: &OutputPlan, primary: &OutputPlan) -> f64 {
    let (pw, ph) = primary.mode.logical_size(primary.scale, primary.transform);
    if pw <= 0 || ph <= 0 {
        return external.scale;
    }
    let (ew, eh) = if external.transform.rotates() {
        (external.mode.height, external.mode.width)
    } else {
        (external.mode.width, external.mode.height)
    };
    let scale = f64::from(ew) / f64::from(pw);
    let scale_y = f64::from(eh) / f64::from(ph);
    let scale = scale.max(scale_y).clamp(0.5, 3.0);
    (scale * 1000.0).round() / 1000.0
}

/// Detect which preset, if any, describes the live arrangement.
pub fn detect_layout(
    outputs: &[Output],
    internal: Option<&str>,
    external: Option<&str>,
) -> LayoutKind {
    if outputs.is_empty() {
        return LayoutKind::Custom;
    }
    let live_scales: BTreeMap<String, f64> =
        outputs.iter().map(|o| (o.name.clone(), o.scale)).collect();

    let has_second = external.is_some_and(|e| Some(e) != internal);
    let mut candidates: Vec<LayoutKind> = Vec::new();
    if !has_second {
        if internal.is_some() && outputs.len() == 1 {
            candidates.push(LayoutKind::Single);
        }
        candidates.push(LayoutKind::ExternalOnly);
    } else {
        candidates.extend(LayoutKind::PRESETS);
    }

    for kind in candidates {
        let plan = LayoutPlan::from_outputs(outputs, kind, internal, external, &live_scales);
        if plan.matches_live(outputs) {
            return kind;
        }
    }
    LayoutKind::Custom
}

/// Result of local hard validation plus informational warnings.
#[derive(Debug, Default, Clone)]
pub struct Validation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl Validation {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn validate(plan: &LayoutPlan, connected: &[Output]) -> Validation {
    let mut v = Validation::default();
    if plan.enabled_count() == 0 {
        v.errors.push("At least one output must be enabled.".into());
    }
    let names: Vec<&String> = plan.outputs.keys().collect();
    for output in connected {
        if !plan.outputs.contains_key(&output.name) {
            v.errors.push(format!(
                "Output {} is connected but missing from the plan.",
                output.name
            ));
        }
    }
    if plan.outputs.len() != names.len() {
        v.errors.push("Duplicate output names in the plan.".into());
    }
    let mut rects: Vec<(String, i64, i64, i64, i64)> = Vec::new();
    for (name, output_plan) in &plan.outputs {
        let Some(output) = connected.iter().find(|o| &o.name == name) else {
            v.errors.push(format!("{name} is no longer connected."));
            continue;
        };
        if !(0.5..=3.0).contains(&output_plan.scale) || !output_plan.scale.is_finite() {
            v.errors.push(format!(
                "{name}: scale {:.3} is outside 0.5–3.0.",
                output_plan.scale
            ));
        }
        if output_plan.enabled {
            let mode = &output_plan.mode;
            if mode.width == 0 || mode.height == 0 {
                v.errors.push(format!("{name}: no mode selected."));
            } else if mode.custom {
                // Custom modes are expected not to be advertised. The
                // compositor's dry run is the real compatibility check.
                if mode.width > 16_384 || mode.height > 16_384 {
                    v.errors
                        .push(format!("{name}: custom mode {mode} is too large."));
                }
            } else if mode.refresh_mhz > 0 && output.find_mode(mode).is_none() {
                v.errors.push(format!(
                    "{name}: mode {} is not advertised by the output.",
                    mode
                ));
            }
        }
        if output_plan.enabled {
            let (w, h) = output_plan.logical_size();
            if w <= 0 || h <= 0 {
                v.errors.push(format!("{name}: invalid logical size."));
            }
            rects.push((
                name.clone(),
                i64::from(output_plan.position.0),
                i64::from(output_plan.position.1),
                i64::from(w),
                i64::from(h),
            ));
        }
    }

    // Overlap warnings (exact mirroring is expected to overlap).
    for i in 0..rects.len() {
        for j in (i + 1)..rects.len() {
            let a = &rects[i];
            let b = &rects[j];
            let overlap = a.1 < b.1 + b.3 && b.1 < a.1 + a.3 && a.2 < b.2 + b.4 && b.2 < a.2 + a.4;
            if overlap && plan.kind != LayoutKind::Mirror {
                v.warnings.push(format!("{} and {} overlap.", a.0, b.0));
            }
        }
    }
    v
}

/// Human readable mirror caveat, when the external does not exactly fill the
/// primary logical area.
pub fn mirror_note(
    plan: &LayoutPlan,
    internal: Option<&str>,
    external: Option<&str>,
) -> Option<String> {
    if plan.kind != LayoutKind::Mirror {
        return None;
    }
    let (internal, external) = (internal?, external?);
    let primary = plan.outputs.get(internal)?;
    let second = plan.outputs.get(external)?;
    let (pw, ph) = primary.logical_size();
    let (ew, eh) = second.logical_size();
    if (pw, ph) == (ew, eh) {
        return None;
    }
    Some(format!(
        "Mirror is approximate: {} shows a {}×{} area of the {}×{} desktop.",
        external, ew, eh, pw, ph
    ))
}

/// Whether a preset makes sense for the connected displays.
///
/// "Laptop only" needs a built-in panel, because a desktop has no laptop
/// screen to keep. "External only" needs a display that is not the built-in
/// panel: on a laptop with no monitor attached there is nothing external to
/// switch to. Desktops and tablets still get it, because all their outputs
/// count as external.
pub fn preset_available(
    kind: LayoutKind,
    output_count: usize,
    has_internal: bool,
    has_external: bool,
) -> bool {
    match kind {
        LayoutKind::Single => has_internal,
        LayoutKind::ExternalOnly => has_external,
        LayoutKind::Extend | LayoutKind::Mirror => output_count >= 2 && has_external,
        LayoutKind::Custom => true,
    }
}

/// Build a deterministic key for a set of outputs, internal panel first.
pub fn set_key(names: &[String], internal: Option<&str>) -> String {
    let mut names = names.to_vec();
    names.sort_by(|a, b| {
        let ai = Some(a.as_str()) == internal;
        let bi = Some(b.as_str()) == internal;
        bi.cmp(&ai).then_with(|| a.cmp(b))
    });
    names.dedup();
    names.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::output::is_internal_name;

    fn output(name: &str, w: u32, h: u32, scale: f64) -> Output {
        let mut mode = Mode::new(w, h, 60_000);
        mode.current = true;
        Output {
            name: name.into(),
            description: format!("{name} display"),
            make: None,
            model: None,
            serial: None,
            enabled: true,
            modes: vec![mode],
            position: (0, 0),
            scale,
            transform: Transform::Normal,
            adaptive_sync: false,
        }
    }

    fn two() -> Vec<Output> {
        let mut edp = output("eDP-1", 3120, 2080, 2.0);
        edp.modes[0].refresh_mhz = 120_000;
        let dp = output("DP-1", 1920, 1200, 1.5);
        vec![edp, dp]
    }

    fn scales(outputs: &[Output]) -> BTreeMap<String, f64> {
        outputs.iter().map(|o| (o.name.clone(), o.scale)).collect()
    }

    fn plan(kind: LayoutKind) -> LayoutPlan {
        let outs = two();
        LayoutPlan::from_outputs(&outs, kind, Some("eDP-1"), Some("DP-1"), &scales(&outs))
    }

    #[test]
    fn extend_position() {
        let p = plan(LayoutKind::Extend);
        assert_eq!(p.outputs["eDP-1"].position, (0, 0));
        assert_eq!(p.outputs["DP-1"].position, (1560, 0));
        assert!(p.outputs["DP-1"].enabled);
    }

    #[test]
    fn extend_position_uses_rotated_primary_width() {
        let outs = two();
        let mut p = LayoutPlan::from_outputs(
            &outs,
            LayoutKind::Extend,
            Some("eDP-1"),
            Some("DP-1"),
            &scales(&outs),
        );
        // Rotating the laptop panel changes the width the external starts at.
        p.outputs.get_mut("eDP-1").unwrap().transform = Transform::Rotate90;
        p.rebuild(Some("eDP-1"), Some("DP-1"), &scales(&outs));
        // 3120x2080 at scale 2 rotated 90 -> 1040 wide.
        assert_eq!(p.outputs["DP-1"].position, (1040, 0));
    }

    #[test]
    fn mirror_fit_scale() {
        let p = plan(LayoutKind::Mirror);
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
        assert!((p.outputs["DP-1"].scale - 1.231).abs() < 0.001);
        assert_eq!(p.outputs["DP-1"].logical_size(), (1560, 975));
    }

    #[test]
    fn normalize_origin_keeps_the_relative_layout() {
        let mut p = plan(LayoutKind::Extend);
        // The external display was dragged to the left of the laptop panel.
        p.outputs.get_mut("DP-1").unwrap().position = (-1560, 0);
        p.normalize_origin();
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
        assert_eq!(p.outputs["eDP-1"].position, (1560, 0));
    }

    #[test]
    fn normalize_origin_handles_both_axes_and_is_idempotent() {
        let mut p = plan(LayoutKind::Extend);
        p.outputs.get_mut("DP-1").unwrap().position = (-1560, -40);
        p.normalize_origin();
        let once = p.clone();
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
        assert_eq!(p.outputs["eDP-1"].position, (1560, 40));
        p.normalize_origin();
        assert_eq!(p, once);
    }

    #[test]
    fn normalize_origin_ignores_disabled_outputs() {
        let mut p = plan(LayoutKind::Single);
        p.outputs.get_mut("DP-1").unwrap().position = (-500, -500);
        p.normalize_origin();
        // The disabled head keeps its stale position; it is not part of the
        // layout the compositor sees.
        assert_eq!(p.outputs["DP-1"].position, (-500, -500));
        assert_eq!(p.outputs["eDP-1"].position, (0, 0));
    }

    #[test]
    fn matches_live_accepts_a_translated_layout() {
        // COSMIC answers with the whole layout moved into the positive
        // quadrant: eDP 0,0 -> 1560,0 and DP -1560,0 -> 0,0. The arrangement
        // is the same one the user asked for.
        let mut live = two();
        live[0].position = (1560, 0);
        live[1].position = (0, 0);
        let mut p = plan(LayoutKind::Extend);
        p.outputs.get_mut("DP-1").unwrap().position = (-1560, 0);
        assert!(p.matches_live(&live));
    }

    #[test]
    fn matches_live_rejects_a_different_arrangement() {
        // The plan keeps the external display on the left, the compositor put
        // it on the right: that is a real mismatch, not a translation.
        let mut live = two();
        live[0].position = (0, 0);
        live[1].position = (1560, 0);
        let mut p = plan(LayoutKind::Extend);
        p.outputs.get_mut("eDP-1").unwrap().position = (1560, 0);
        p.outputs.get_mut("DP-1").unwrap().position = (0, 0);
        assert!(!p.matches_live(&live));
    }

    #[test]
    fn mirror_restores_desktop_scale() {
        let outs = two();
        let mut remembered = scales(&outs);
        let mut p = LayoutPlan::from_outputs(
            &outs,
            LayoutKind::Mirror,
            Some("eDP-1"),
            Some("DP-1"),
            &remembered,
        );
        assert!((p.outputs["DP-1"].scale - 1.231).abs() < 0.001);
        p.kind = LayoutKind::Extend;
        remembered.insert("DP-1".into(), 1.5);
        p.rebuild(Some("eDP-1"), Some("DP-1"), &remembered);
        assert!((p.outputs["DP-1"].scale - 1.5).abs() < 0.001);
    }

    #[test]
    fn single_disables_external() {
        let p = plan(LayoutKind::Single);
        assert!(p.outputs["eDP-1"].enabled);
        assert!(!p.outputs["DP-1"].enabled);
        assert_eq!(p.enabled_count(), 1);
    }

    #[test]
    fn external_only_disables_panel() {
        let p = plan(LayoutKind::ExternalOnly);
        assert!(!p.outputs["eDP-1"].enabled);
        assert!(p.outputs["DP-1"].enabled);
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
    }

    /// Two external monitors and no built-in panel (a desktop, a tablet with
    /// a dock, a laptop whose panel has an unusual name).
    fn desktop() -> Vec<Output> {
        vec![
            output("DP-1", 2560, 1440, 1.0),
            output("DP-2", 1920, 1080, 1.0),
        ]
    }

    #[test]
    fn extend_works_without_a_built_in_panel() {
        let outs = desktop();
        let p = LayoutPlan::from_outputs(
            &outs,
            LayoutKind::Extend,
            None,
            Some("DP-2"),
            &scales(&outs),
        );
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
        assert_eq!(p.outputs["DP-2"].position, (2560, 0));
        assert!(p.outputs["DP-1"].enabled && p.outputs["DP-2"].enabled);
    }

    #[test]
    fn mirror_works_without_a_built_in_panel() {
        let outs = desktop();
        let p = LayoutPlan::from_outputs(
            &outs,
            LayoutKind::Mirror,
            None,
            Some("DP-2"),
            &scales(&outs),
        );
        assert_eq!(p.outputs["DP-1"].position, (0, 0));
        assert_eq!(p.outputs["DP-2"].position, (0, 0));
        assert!((p.outputs["DP-2"].scale - 0.75).abs() < 0.001);
    }

    #[test]
    fn presets_only_need_the_right_number_of_displays() {
        assert!(preset_available(LayoutKind::Single, 2, true, true));
        assert!(!preset_available(LayoutKind::Single, 2, false, true));
        assert!(preset_available(LayoutKind::Extend, 2, false, true));
        assert!(preset_available(LayoutKind::Mirror, 2, false, true));
        assert!(!preset_available(LayoutKind::Extend, 1, true, false));
        assert!(!preset_available(LayoutKind::Mirror, 1, true, false));
        // A desktop with a single monitor has an external output but no panel.
        assert!(preset_available(LayoutKind::ExternalOnly, 1, false, true));
        // A laptop on its own has nothing external to switch to.
        assert!(!preset_available(LayoutKind::ExternalOnly, 1, true, false));
        assert!(preset_available(LayoutKind::ExternalOnly, 2, true, true));
        assert!(preset_available(LayoutKind::Custom, 0, false, false));
    }

    #[test]
    fn detects_presets() {
        for kind in [
            LayoutKind::Single,
            LayoutKind::Extend,
            LayoutKind::Mirror,
            LayoutKind::ExternalOnly,
        ] {
            let mut outs = two();
            let p =
                LayoutPlan::from_outputs(&outs, kind, Some("eDP-1"), Some("DP-1"), &scales(&outs));
            for out in &mut outs {
                let po = &p.outputs[&out.name];
                out.enabled = po.enabled;
                out.position = po.position;
                out.scale = po.scale;
            }
            assert_eq!(
                detect_layout(&outs, Some("eDP-1"), Some("DP-1")),
                kind,
                "kind {kind:?}"
            );
        }
    }

    #[test]
    fn legacy_extend_kinds_load_as_extend() {
        let right: LayoutKind = serde_json::from_str("\"extend-right\"").unwrap();
        let left: LayoutKind = serde_json::from_str("\"extend-left\"").unwrap();
        assert_eq!(right, LayoutKind::Extend);
        assert_eq!(left, LayoutKind::Extend);
        assert_eq!(
            serde_json::to_string(&LayoutKind::Extend).unwrap(),
            "\"extend\""
        );
    }

    #[test]
    fn custom_when_offset() {
        let mut outs = two();
        outs[0].position = (10, 10);
        outs[1].position = (2000, 42);
        assert_eq!(
            detect_layout(&outs, Some("eDP-1"), Some("DP-1")),
            LayoutKind::Custom
        );
    }

    #[test]
    fn validation_catches_bad_scale_and_unknown_mode() {
        let outs = two();
        let mut p = plan(LayoutKind::Extend);
        p.outputs.get_mut("DP-1").unwrap().scale = 4.0;
        p.outputs.get_mut("eDP-1").unwrap().mode = Mode::new(123, 456, 60_000);
        let v = validate(&p, &outs);
        assert!(!v.is_valid());
        assert!(v.errors.iter().any(|e| e.contains("outside")));
        assert!(v.errors.iter().any(|e| e.contains("not advertised")));
    }

    #[test]
    fn validation_accepts_compositor_custom_modes() {
        let outs = two();
        let mut p = plan(LayoutKind::Extend);
        let mut mode = Mode::new(1920, 1280, 120_000);
        mode.custom = true;
        p.outputs.get_mut("eDP-1").unwrap().mode = mode;
        assert!(validate(&p, &outs).is_valid());
    }

    #[test]
    fn validation_rejects_a_zero_sized_custom_mode() {
        let outs = two();
        let mut p = plan(LayoutKind::Extend);
        let mut mode = Mode::new(0, 0, 120_000);
        mode.custom = true;
        p.outputs.get_mut("eDP-1").unwrap().mode = mode;
        assert!(validate(&p, &outs)
            .errors
            .iter()
            .any(|e| e.contains("no mode selected")));
    }

    #[test]
    fn validation_rejects_zero_enabled() {
        let outs = two();
        let mut p = plan(LayoutKind::Single);
        for po in p.outputs.values_mut() {
            po.enabled = false;
        }
        assert!(validate(&p, &outs)
            .errors
            .iter()
            .any(|e| e.contains("At least one")));
    }

    #[test]
    fn preview_has_internal_first_key() {
        let names = ["DP-1".to_string(), "eDP-1".to_string()];
        assert_eq!(set_key(&names, Some("eDP-1")), "eDP-1+DP-1");
        assert!(is_internal_name("eDP-1"));
    }

    #[test]
    fn disabled_output_position_is_ignored() {
        let mut outs = two();
        let p = plan(LayoutKind::Single);
        for output in &mut outs {
            let po = &p.outputs[&output.name];
            output.enabled = po.enabled;
        }
        // Disabling an output leaves its last position behind.
        outs[1].position = (1560, 0);
        assert!(p.matches_live(&outs));
        outs[1].enabled = true;
        assert!(!p.matches_live(&outs));
    }

    #[test]
    fn plan_matches_live_tolerances() {
        let mut outs = two();
        let p = plan(LayoutKind::Extend);
        for output in &mut outs {
            let po = &p.outputs[&output.name];
            output.enabled = po.enabled;
            output.position = po.position;
            output.scale = po.scale;
        }
        assert!(p.matches_live(&outs));
        outs[1].position.0 += 2;
        assert!(p.matches_live(&outs));
        outs[1].position.0 += 3;
        assert!(!p.matches_live(&outs));
    }
}
