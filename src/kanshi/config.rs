//! Managed-block kanshi config generation and atomic, backup-protected writes.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::backend::cosmic;
use crate::config::AppState;
use crate::model::layout::{format_scale, LayoutKind, LayoutPlan, OutputPlan};
use crate::model::output::{is_internal_name, Mode, Output, Transform};
use crate::util;

pub const BLOCK_START: &str = "# >>> kanshi-gtk4 managed block — regenerated on apply, do not edit";
pub const BLOCK_END: &str = "# <<< kanshi-gtk4 managed block";
pub const PRESERVED: &str = "# --- preserved from previous config ---";

/// One output used for generation.
#[derive(Debug, Clone)]
pub struct GenOutput {
    pub mode: Mode,
    pub scale: f64,
    pub desktop_scale: f64,
    pub transform: Transform,
    pub position: (i32, i32),
    pub adaptive_sync: bool,
}

impl GenOutput {
    fn to_plan_output(&self) -> OutputPlan {
        OutputPlan {
            enabled: true,
            mode: self.mode.clone(),
            position: self.position,
            scale: self.scale,
            transform: self.transform,
            adaptive_sync: self.adaptive_sync,
        }
    }
}

/// One output set known to the app.
#[derive(Debug, Clone)]
pub struct GenSet {
    pub key: String,
    pub outputs: BTreeMap<String, GenOutput>,
    pub default_layout: LayoutKind,
    pub primary: Option<String>,
    pub secondary: Option<String>,
    /// The live plan, used only for the `auto-custom` profile of the current
    /// set.
    pub custom_plan: Option<LayoutPlan>,
}

impl GenSet {
    fn from_plan(
        key: String,
        plan: &LayoutPlan,
        default_layout: LayoutKind,
        desktop_scales: &BTreeMap<String, f64>,
    ) -> Self {
        let outputs: BTreeMap<String, GenOutput> = plan
            .outputs
            .iter()
            .map(|(name, po)| {
                (
                    name.clone(),
                    GenOutput {
                        mode: po.mode.clone(),
                        scale: po.scale,
                        desktop_scale: desktop_scales.get(name).copied().unwrap_or(po.scale),
                        transform: po.transform,
                        position: po.position,
                        adaptive_sync: po.adaptive_sync,
                    },
                )
            })
            .collect();
        let (primary, secondary) = pick_outputs(outputs.keys().cloned().collect());
        Self {
            key,
            outputs,
            default_layout,
            primary,
            secondary,
            custom_plan: (default_layout == LayoutKind::Custom).then(|| plan.clone()),
        }
    }

    fn from_state(key: &str, set: &crate::config::SetState) -> Option<Self> {
        let mut outputs = BTreeMap::new();
        for (name, state) in &set.outputs {
            let Some(mut mode) = Mode::parse_kanshi(&state.mode) else {
                continue;
            };
            mode.custom = state.custom;
            outputs.insert(
                name.clone(),
                GenOutput {
                    mode,
                    scale: state.scale,
                    desktop_scale: state.desktop_scale(),
                    transform: state.transform,
                    position: state.position.unwrap_or((0, 0)),
                    adaptive_sync: state.adaptive_sync,
                },
            );
        }
        if outputs.is_empty() {
            return None;
        }
        let (primary, secondary) = pick_outputs(outputs.keys().cloned().collect());
        Some(Self {
            key: key.to_string(),
            outputs,
            default_layout: set.default_layout,
            primary,
            secondary,
            custom_plan: None,
        })
    }

    fn has_second(&self) -> bool {
        self.secondary.is_some()
    }

    fn internal(&self) -> bool {
        self.primary.as_deref().is_some_and(is_internal_name)
    }

    fn desktop_scales(&self) -> BTreeMap<String, f64> {
        self.outputs
            .iter()
            .map(|(name, out)| (name.clone(), out.desktop_scale))
            .collect()
    }

    fn plan_for(&self, kind: LayoutKind) -> LayoutPlan {
        if kind == LayoutKind::Custom {
            if let Some(plan) = &self.custom_plan {
                return plan.clone();
            }
        }
        let mut plan = LayoutPlan {
            kind,
            outputs: self
                .outputs
                .iter()
                .map(|(name, out)| (name.clone(), out.to_plan_output()))
                .collect(),
        };
        plan.rebuild(
            self.primary.as_deref(),
            self.secondary.as_deref(),
            &self.desktop_scales(),
        );
        plan
    }

    fn applicable_layouts(&self) -> Vec<LayoutKind> {
        if self.has_second() {
            LayoutKind::PRESETS.to_vec()
        } else if self.internal() {
            vec![LayoutKind::Single]
        } else {
            vec![LayoutKind::ExternalOnly]
        }
    }
}

/// Internal panel first, then other outputs sorted.
fn pick_outputs(names: Vec<String>) -> (Option<String>, Option<String>) {
    let mut names = names;
    names.sort_by(|a, b| {
        let ai = is_internal_name(a);
        let bi = is_internal_name(b);
        bi.cmp(&ai).then_with(|| a.cmp(b))
    });
    let primary = names.first().cloned();
    let secondary = names
        .iter()
        .skip(1)
        .find(|n| Some(*n) != primary.as_ref())
        .cloned();
    (primary, secondary)
}

/// The live layout to emit first for the current output set.
#[derive(Debug, Clone)]
pub struct CurrentLayout {
    pub key: String,
    pub plan: LayoutPlan,
    pub connected: Vec<Output>,
    /// Scales to use when generating non-mirror profiles. A mirror apply
    /// changes the live external scale, but the desktop scale must not change.
    pub desktop_scales: BTreeMap<String, f64>,
}

impl CurrentLayout {
    pub fn new(
        key: String,
        plan: LayoutPlan,
        connected: Vec<Output>,
        desktop_scales: BTreeMap<String, f64>,
    ) -> Self {
        Self {
            key,
            plan,
            connected,
            desktop_scales,
        }
    }
}

/// Generate the complete managed block for all known sets, current one first.
///
/// `native_mirror` adds an `exec cosmic-randr mirror …` line to mirror
/// profiles, for compositors (COSMIC) where two outputs at the same position
/// do not show the same content.
pub fn generate_managed_block(
    state: &AppState,
    current: Option<&CurrentLayout>,
    native_mirror: bool,
) -> String {
    let mut sets: Vec<GenSet> = Vec::new();
    if let Some(current) = current {
        sets.push(GenSet::from_plan(
            current.key.clone(),
            &current.plan,
            current.plan.kind,
            &current.desktop_scales,
        ));
    }
    for (key, set) in &state.sets {
        if current.is_some_and(|c| &c.key == key) {
            continue;
        }
        if let Some(gen) = GenSet::from_state(key, set) {
            sets.push(gen);
        }
    }

    let mut used_names = BTreeSet::new();
    let mut profiles = Vec::new();
    for set in &sets {
        profiles.extend(render_set(set, &mut used_names, native_mirror));
    }

    let mut text = String::new();
    text.push_str(BLOCK_START);
    text.push('\n');
    if let Some(current) = current {
        let mut described = BTreeSet::new();
        for (name, output) in &current.plan.outputs {
            if !described.insert(name.clone()) {
                continue;
            }
            let Some(live) = current.connected.iter().find(|o| &o.name == name) else {
                continue;
            };
            let description = clean_description(live);
            let (w, h) = output.logical_size();
            let mut details = String::new();
            if !description.is_empty() {
                details.push_str(description.trim_end_matches([',', ' ']));
                details.push_str(", ");
            }
            details.push_str(&format!(
                "{}x{} @ {} Hz, logical {w}x{h}",
                output.mode.width,
                output.mode.height,
                output.mode.refresh_string()
            ));
            text.push_str(&format!("# {name}: {details}\n"));
        }
        text.push('\n');
    }
    if profiles.is_empty() {
        text.push_str("# No profiles yet — connect a display and open kanshi-gtk4.\n");
    } else {
        text.push_str(&profiles.join("\n"));
    }
    text.push_str(BLOCK_END);
    text.push('\n');
    text
}

fn clean_description(output: &Output) -> String {
    let description = output.description.trim();
    let suffix = format!(" ({})", output.name);
    description
        .strip_suffix(&suffix)
        .unwrap_or(description)
        .to_string()
}

fn render_set(set: &GenSet, used_names: &mut BTreeSet<String>, native_mirror: bool) -> Vec<String> {
    let applicable = set.applicable_layouts();
    let mut layouts: Vec<LayoutKind> = Vec::new();
    if set.default_layout == LayoutKind::Custom && set.custom_plan.is_some() {
        layouts.push(LayoutKind::Custom);
    }
    if applicable.contains(&set.default_layout) {
        layouts.push(set.default_layout);
    }
    for kind in &applicable {
        if !layouts.contains(kind) {
            layouts.push(*kind);
        }
    }

    let mut rendered = Vec::new();
    let when_connected = connection_phrase(&set.key);
    for kind in layouts {
        let plan = set.plan_for(kind);
        let name = unique_profile_name(kind, &set.key, used_names);
        let comment = if kind == set.default_layout {
            format!("# Default {when_connected}")
        } else if kind == LayoutKind::Custom {
            format!("# Manual arrangement {when_connected}")
        } else {
            format!("# {} {when_connected}", kind.label())
        };
        rendered.push(render_profile(&name, &comment, &plan, native_mirror));
    }
    rendered
}

/// Human-readable subject for profile comments: `when only eDP-1 is
/// connected`, `when eDP-1 + DP-1 are connected`.
fn connection_phrase(key: &str) -> String {
    let names: Vec<&str> = key.split('+').filter(|part| !part.is_empty()).collect();
    match names.len() {
        0 => "when this output set is connected".to_string(),
        1 => format!("when only {} is connected", names[0]),
        _ => format!("when {} are connected", names.join(" + ")),
    }
}

fn unique_profile_name(kind: LayoutKind, set_key: &str, used: &mut BTreeSet<String>) -> String {
    let base = format!("auto-{}", kind.as_str());
    if used.insert(base.clone()) {
        return base;
    }
    let suffix = sanitise_suffix(set_key);
    let candidate = format!("{base}--{suffix}");
    if used.insert(candidate.clone()) {
        return candidate;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}--{suffix}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn sanitise_suffix(key: &str) -> String {
    key.split('+')
        .map(|part| {
            let lower = part.to_ascii_lowercase();
            let mut out = String::new();
            let mut last_dash = false;
            for ch in lower.chars() {
                if ch.is_ascii_alphanumeric() {
                    out.push(ch);
                    last_dash = false;
                } else if !last_dash {
                    out.push('-');
                    last_dash = true;
                }
            }
            out.trim_matches('-').to_string()
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn render_profile(name: &str, comment: &str, plan: &LayoutPlan, native_mirror: bool) -> String {
    let mut text = String::new();
    text.push_str(comment);
    text.push('\n');
    text.push_str(&format!("profile {name} {{\n"));
    for (output_name, output) in &plan.outputs {
        if !output.enabled {
            text.push_str(&format!("\toutput {output_name} disable\n"));
            continue;
        }
        text.push_str(&format!("\toutput {output_name} {{\n"));
        if output.mode.custom {
            text.push_str(&format!("\t\tmode --custom {}\n", output.mode.to_kanshi()));
        } else {
            text.push_str(&format!("\t\tmode {}\n", output.mode.to_kanshi()));
        }
        text.push_str(&format!(
            "\t\tposition {},{}\n",
            output.position.0, output.position.1
        ));
        text.push_str(&format!("\t\tscale {}\n", format_scale(output.scale)));
        text.push_str(&format!("\t\ttransform {}\n", output.transform.as_str()));
        if output.adaptive_sync {
            text.push_str("\t\tadaptive_sync on\n");
        }
        text.push_str("\t}\n");
    }
    // Overlap alone is not a mirror on COSMIC: the profile has to ask the
    // compositor for one, or only the pointer is shared between the heads.
    // Any plan whose heads sit at the same position is a mirror, including a
    // manual arrangement that was never labelled as the Mirror preset: that is
    // what makes a hotplug restore the same picture on both screens.
    if native_mirror {
        if let Some((external, internal)) = cosmic::mirror_pair(plan) {
            text.push_str(&format!(
                "\texec {}\n",
                cosmic::mirror_command(&external, &internal)
            ));
        }
    }
    text.push_str("}\n");
    text
}

/// Replace the managed block, or insert it at the top on first run. Text
/// outside the markers is preserved.
///
/// Markers are matched on a whole line, not as a substring: a user comment
/// that merely mentions the marker text must not be mistaken for the block.
/// A file with duplicated or unbalanced markers is repaired rather than
/// extended: every old block body and marker is dropped, the user's text is
/// kept and a single fresh block goes to the top. The body of a truncated
/// block is kept as preserved text, because it may contain the profiles the
/// user was editing when the write was interrupted.
pub fn merge_managed_block(existing: Option<&[u8]>, block: &str) -> String {
    let block = block.trim_end().to_string();
    let Some(existing) = existing else {
        return format!("{block}\n");
    };
    let text = String::from_utf8_lossy(existing);
    let lines: Vec<&str> = text.lines().collect();

    // Pair every start marker with the next end marker. Blocks that never
    // close, markers that close nothing and a second block all make the file
    // "malformed": its bodies cannot be trusted, so they are dropped.
    let mut regions: Vec<(usize, usize)> = Vec::new();
    // Marker lines that belong to no complete block (an orphaned end, or a
    // start that was taken over by another start).
    let mut strays: Vec<usize> = Vec::new();
    let mut open: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        if is_marker(line, BLOCK_START) {
            if let Some(previous) = open.replace(index) {
                strays.push(previous);
            }
        } else if is_marker(line, BLOCK_END) {
            match open.take() {
                Some(start) => regions.push((start, index)),
                None => strays.push(index),
            }
        }
    }
    // A start marker with no end: drop the marker but keep what follows, as
    // it is user text that never became a managed block.
    if let Some(start) = open {
        strays.push(start);
    }

    let mut drop = vec![false; lines.len()];
    for index in &strays {
        drop[*index] = true;
    }
    for (start, end) in &regions {
        for flag in &mut drop[*start..=*end] {
            *flag = true;
        }
    }

    let rest: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !drop[*index])
        .map(|(_, line)| *line)
        .collect();
    let rest = rest.join("\n");
    let rest = rest.trim_matches('\n');

    if regions.len() == 1 && strays.is_empty() {
        // The normal case: keep the block where the user had it.
        if rest.is_empty() {
            format!("{block}\n")
        } else {
            format!("{block}\n\n{rest}\n")
        }
    } else if rest.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{block}\n\n{PRESERVED}\n{rest}\n")
    }
}

fn is_marker(line: &str, marker: &str) -> bool {
    line.trim() == marker
}

/// Write the candidate config atomically, creating a one-time pre-change
/// backup and never touching user files such as `config.bak`.
pub fn write_config(path: &Path, contents: &[u8], previous: Option<&[u8]>) -> Result<()> {
    if let Some(previous) = previous {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let backup = path.with_file_name(format!(
            "{}.pre-kanshi-gtk4.{stamp}.bak",
            path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default()
        ));
        if !backup.exists() {
            util::atomic_write(&backup, previous)
                .with_context(|| format!("writing backup {}", backup.display()))?;
        }
    }
    util::atomic_write(path, contents)
}

/// Keep a copy of the last known-good config after Keep.
pub fn write_keep_backup(path: &Path) -> Result<()> {
    let backup = path.with_file_name(format!(
        "{}.kanshi-gtk4.bak",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    if let Some(bytes) = util::read_optional(path)? {
        util::atomic_write(&backup, &bytes)?;
    }
    Ok(())
}

/// Restore exact bytes, removing the file when it did not exist before.
pub fn restore_config(path: &Path, previous: Option<&[u8]>, existed: bool) -> Result<()> {
    match (previous, existed) {
        (Some(bytes), _) => util::atomic_write(path, bytes),
        (None, false) => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("removing {}", path.display())),
        },
        (None, true) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, OutputState, SetState};
    use crate::model::output::Transform;
    use std::collections::BTreeMap;

    fn live_output(name: &str, w: u32, h: u32, refresh: u32, scale: f64) -> Output {
        let mut mode = Mode::new(w, h, refresh);
        mode.current = true;
        mode.preferred = true;
        Output {
            name: name.into(),
            description: format!(
                "{} Display ({name})",
                name.split('-').next().unwrap_or(name)
            ),
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

    fn two_live() -> Vec<Output> {
        vec![
            live_output("eDP-1", 3120, 2080, 120_000, 2.0),
            live_output("DP-1", 1920, 1200, 60_000, 1.5),
        ]
    }

    fn state_with_two() -> AppState {
        let mut state = AppState::default();
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".to_string(),
            OutputState {
                mode: "3120x2080@120".into(),
                identity: None,
                custom: false,
                scale: 2.0,
                desktop_scale: Some(2.0),
                transform: Transform::Normal,
                position: Some((0, 0)),
                adaptive_sync: false,
            },
        );
        outputs.insert(
            "DP-1".to_string(),
            OutputState {
                mode: "1920x1200@60".into(),
                identity: None,
                custom: false,
                scale: 1.5,
                desktop_scale: Some(1.5),
                transform: Transform::Normal,
                position: Some((1560, 0)),
                adaptive_sync: false,
            },
        );
        state.sets.insert(
            "eDP-1+DP-1".into(),
            SetState {
                last_seen: "2026-10-26T16:40:00+01:00".into(),
                default_layout: LayoutKind::Extend,
                outputs,
            },
        );
        state.internal_outputs = vec!["eDP-1".into()];
        state
    }

    #[test]
    fn generates_expected_profiles_in_order() {
        let live = two_live();
        let mut scales = BTreeMap::new();
        scales.insert("eDP-1".to_string(), 2.0);
        scales.insert("DP-1".to_string(), 1.5);
        let plan = LayoutPlan::from_outputs(
            &live,
            LayoutKind::Extend,
            Some("eDP-1"),
            Some("DP-1"),
            &scales,
        );
        let current = CurrentLayout::new("eDP-1+DP-1".into(), plan, live, scales);
        let block = generate_managed_block(&AppState::default(), Some(&current), false);
        assert!(block.starts_with(BLOCK_START));
        assert!(block.contains("profile auto-extend {"));
        assert!(block.contains("profile auto-mirror {"));
        assert!(block.contains("profile auto-single {"));
        assert!(block.contains("position 1560,0"));
        assert!(block.contains("scale 1.231"));
        assert!(block.contains("output eDP-1 disable"));
        let first_profile = block.find("profile auto-extend").unwrap();
        let second_profile = block.find("profile auto-single").unwrap();
        assert!(first_profile < second_profile);
    }

    #[test]
    fn custom_modes_are_written_with_the_custom_flag() {
        let live = vec![live_output("eDP-1", 3120, 2080, 120_000, 2.0)];
        let mut scales = BTreeMap::new();
        scales.insert("eDP-1".to_string(), 2.0);
        let mut plan =
            LayoutPlan::from_outputs(&live, LayoutKind::Single, Some("eDP-1"), None, &scales);
        let mut mode = Mode::new(1920, 1280, 120_000);
        mode.custom = true;
        plan.outputs.get_mut("eDP-1").unwrap().mode = mode;

        let current = CurrentLayout::new("eDP-1".into(), plan, live, scales);
        let block = generate_managed_block(&AppState::default(), Some(&current), false);
        assert!(block.contains("mode --custom 1920x1280@120"), "{block}");
        assert!(!block.contains("mode 1920x1280@120"), "{block}");
    }

    #[test]
    fn remembered_custom_modes_keep_the_custom_flag() {
        let mut state = state_with_two();
        let output = state
            .sets
            .get_mut("eDP-1+DP-1")
            .unwrap()
            .outputs
            .get_mut("eDP-1")
            .unwrap();
        output.mode = "1920x1280@120".into();
        output.custom = true;

        let block = generate_managed_block(&state, None, false);
        assert!(block.contains("mode --custom 1920x1280@120"), "{block}");
    }

    #[test]
    fn mirror_apply_keeps_desktop_scale_in_other_profiles() {
        let live = two_live();
        let mut desktop_scales = BTreeMap::new();
        desktop_scales.insert("eDP-1".to_string(), 2.0);
        desktop_scales.insert("DP-1".to_string(), 1.5);
        let plan = LayoutPlan::from_outputs(
            &live,
            LayoutKind::Mirror,
            Some("eDP-1"),
            Some("DP-1"),
            &desktop_scales,
        );
        assert!((plan.outputs["DP-1"].scale - 1.231).abs() < 0.001);

        let current = CurrentLayout::new("eDP-1+DP-1".into(), plan, live, desktop_scales);
        let block = generate_managed_block(&AppState::default(), Some(&current), false);
        let start = block.find("profile auto-extend").unwrap();
        let extend = block[start..].split("\nprofile ").next().unwrap();
        assert!(extend.contains("scale 1.5"), "{extend}");
        assert!(!extend.contains("1.231"), "{extend}");
    }

    #[test]
    fn manual_overlap_profiles_carry_the_native_mirror_command() {
        let live = two_live();
        let mut scales = BTreeMap::new();
        scales.insert("eDP-1".to_string(), 2.0);
        scales.insert("DP-1".to_string(), 1.5);
        let mut plan = LayoutPlan::from_outputs(
            &live,
            LayoutKind::Extend,
            Some("eDP-1"),
            Some("DP-1"),
            &scales,
        );
        // The external was dragged onto the panel, or the compositor restored
        // that arrangement after a hotplug: a manual overlap is still a mirror.
        plan.outputs.get_mut("DP-1").unwrap().position = (0, 0);
        plan.kind = LayoutKind::Custom;
        let current = CurrentLayout::new("eDP-1+DP-1".into(), plan, live, scales);

        let block = generate_managed_block(&AppState::default(), Some(&current), true);
        let start = block.find("profile auto-custom").expect("custom profile");
        let custom = block[start..].split("\nprofile ").next().unwrap();
        assert!(
            custom.contains("\n\texec ") && custom.contains("mirror DP-1 eDP-1\n"),
            "{custom}"
        );
    }

    #[test]
    fn mirror_profiles_carry_the_native_mirror_command() {
        let live = two_live();
        let mut scales = BTreeMap::new();
        scales.insert("eDP-1".to_string(), 2.0);
        scales.insert("DP-1".to_string(), 1.5);
        let plan = LayoutPlan::from_outputs(
            &live,
            LayoutKind::Mirror,
            Some("eDP-1"),
            Some("DP-1"),
            &scales,
        );
        let current = CurrentLayout::new("eDP-1+DP-1".into(), plan, live, scales);

        let block = generate_managed_block(&AppState::default(), Some(&current), true);
        let start = block.find("profile auto-mirror").unwrap();
        let mirror = block[start..].split("\nprofile ").next().unwrap();
        assert!(
            mirror.contains("\n\texec ") && mirror.contains("mirror DP-1 eDP-1\n"),
            "{mirror}"
        );

        // A non-mirror profile carries no exec line: kanshi reconfigures the
        // head through zwlr_output_manager_v1, and the app breaks stale
        // mirrors before it applies a layout.
        let start = block.find("profile auto-extend").unwrap();
        let extend = block[start..].split("\nprofile ").next().unwrap();
        assert!(!extend.contains("\texec "), "{extend}");

        // Without native mirroring (sway, labwc, niri) the config is unchanged.
        let block = generate_managed_block(&AppState::default(), Some(&current), false);
        assert!(!block.contains("exec "), "{block}");
    }

    #[test]
    fn header_and_comments_have_clean_grammar() {
        let mut live = vec![live_output("eDP-1", 3120, 2080, 120_000, 2.0)];
        live[0].description = "EDO EDO14.55, (eDP-1)".into();
        let mut scales = BTreeMap::new();
        scales.insert("eDP-1".to_string(), 2.0);
        let plan =
            LayoutPlan::from_outputs(&live, LayoutKind::Single, Some("eDP-1"), None, &scales);
        let current = CurrentLayout::new("eDP-1".into(), plan, live, scales);
        let block = generate_managed_block(&AppState::default(), Some(&current), false);
        assert!(!block.contains(", ,"), "{block}");
        assert!(!block.contains("EDO EDO14.55, ,"), "{block}");
        assert!(
            block.contains("# Default when only eDP-1 is connected"),
            "{block}"
        );
        assert!(!block.contains("when eDP-1 are connected"), "{block}");
    }

    #[test]
    fn names_collide_across_sets() {
        let mut state = state_with_two();
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".to_string(),
            OutputState {
                mode: "3120x2080@120".into(),
                identity: None,
                custom: false,
                scale: 2.0,
                desktop_scale: Some(2.0),
                transform: Transform::Normal,
                position: Some((0, 0)),
                adaptive_sync: false,
            },
        );
        outputs.insert(
            "HDMI-A-1".to_string(),
            OutputState {
                mode: "1920x1080@60".into(),
                identity: None,
                custom: false,
                scale: 1.0,
                desktop_scale: Some(1.0),
                transform: Transform::Normal,
                position: Some((1560, 0)),
                adaptive_sync: false,
            },
        );
        state.sets.insert(
            "eDP-1+HDMI-A-1".into(),
            SetState {
                last_seen: String::new(),
                default_layout: LayoutKind::Extend,
                outputs,
            },
        );
        let block = generate_managed_block(&state, None, false);
        assert_eq!(block.matches("profile auto-extend--").count(), 1);
        assert!(block.contains("auto-extend--edp-1+hdmi-a-1"));
        assert!(!block.contains("auto-extend--edp-1+dp-1"));
        // All profile names must be unique.
        let names: Vec<&str> = block
            .lines()
            .filter_map(|l| {
                l.strip_prefix("profile ")
                    .and_then(|l| l.strip_suffix(" {"))
            })
            .collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len());
    }

    #[test]
    fn legacy_text_is_preserved_and_block_moves_to_top() {
        let existing = b"# header\n\nprofile single {\n\toutput eDP-1 mode 3120x2080@120\n}\n";
        let block = format!("{BLOCK_START}\nblock\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing), &block);
        assert!(merged.starts_with(BLOCK_START));
        assert!(merged.contains("profile single"));
        assert!(merged.contains("# header"));
        assert!(merged.contains(PRESERVED));
    }

    #[test]
    fn existing_block_is_replaced_not_duplicated() {
        let existing =
            format!("# keep me\n{BLOCK_START}\nold\n{BLOCK_END}\n# footer\nprofile single {{}}\n");
        let block = format!("{BLOCK_START}\nnew\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing.as_bytes()), &block);
        assert_eq!(merged.matches(BLOCK_START).count(), 1);
        assert_eq!(merged.matches(BLOCK_END).count(), 1);
        assert!(merged.contains("new"));
        assert!(!merged.contains("old"));
        assert!(merged.contains("# keep me"));
        assert!(merged.contains("# footer"));
        assert!(!merged.contains(PRESERVED));
        assert!(merged.starts_with(BLOCK_START));
    }

    #[test]
    fn merge_missing_markers_puts_block_first() {
        let merged = merge_managed_block(Some(b"profile old {}\n"), "BLOCK\n");
        assert!(merged.starts_with("BLOCK"));
        assert!(merged.contains(PRESERVED));
        assert!(merged.contains("profile old {}"));
    }

    #[test]
    fn a_truncated_block_is_repaired_without_losing_content() {
        // The end marker is gone (interrupted write, hand edit): the old block
        // body must not be nested inside the new one.
        let existing = format!("# header\n{BLOCK_START}\nprofile half {{}}\n");
        let block = format!("{BLOCK_START}\nnew\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing.as_bytes()), &block);
        assert_eq!(merged.matches(BLOCK_START).count(), 1);
        assert_eq!(merged.matches(BLOCK_END).count(), 1);
        assert!(merged.starts_with(BLOCK_START));
        assert!(merged.contains("profile half {}"));
        assert!(merged.contains("# header"));
        assert!(!merged.contains("new\nprofile half"), "{merged}");
    }

    #[test]
    fn duplicate_markers_are_collapsed() {
        let existing = format!(
            "{BLOCK_START}\nold\n{BLOCK_END}\n# between\n{BLOCK_START}\nolder\n{BLOCK_END}\n"
        );
        let block = format!("{BLOCK_START}\nnew\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing.as_bytes()), &block);
        assert_eq!(merged.matches(BLOCK_START).count(), 1);
        assert_eq!(merged.matches(BLOCK_END).count(), 1);
        assert!(merged.contains("new"));
        assert!(!merged.contains("old"));
        assert!(merged.contains("# between"));
    }

    #[test]
    fn an_orphaned_end_marker_is_dropped() {
        // An end marker with no start (an interrupted append) must not be
        // mistaken for a block boundary.
        let existing = format!("profile mine {{}}\n{BLOCK_END}\n# tail\n");
        let block = format!("{BLOCK_START}\nnew\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing.as_bytes()), &block);
        assert_eq!(merged.matches(BLOCK_END).count(), 1);
        assert_eq!(merged.matches(BLOCK_START).count(), 1);
        assert!(merged.starts_with(BLOCK_START));
        assert!(merged.contains("profile mine {}"));
        assert!(merged.contains("# tail"));
    }

    #[test]
    fn marker_text_inside_a_comment_is_not_a_marker() {
        // A line that merely mentions the marker (e.g. quoted in a comment)
        // must not be treated as the block boundary.
        let existing = format!("## {BLOCK_START}\nprofile mine {{}}\n");
        let block = format!("{BLOCK_START}\nnew\n{BLOCK_END}");
        let merged = merge_managed_block(Some(existing.as_bytes()), &block);
        assert_eq!(merged.matches(BLOCK_START).count(), 2);
        assert!(merged.contains("## "), "{merged}");
        assert!(merged.contains("profile mine {}"));
        assert!(merged.starts_with(BLOCK_START));
    }

    #[test]
    fn writes_backup_once_and_restores_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, b"old\n").unwrap();
        write_config(&path, b"new\n", Some(b"old\n")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("pre-kanshi-gtk4"))
            .collect();
        assert_eq!(backups.len(), 1);
        write_config(&path, b"newer\n", Some(b"new\n")).unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("pre-kanshi-gtk4"))
            .collect();
        assert_eq!(backups.len(), 1);
        restore_config(&path, Some(b"old\n"), true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    }
}
