//! Apply, keep and revert state machine.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use crate::backend;
use crate::backend::cosmic;
use crate::config::{AppState, Paths};
use crate::kanshi::config::{
    generate_managed_block, merge_managed_block, write_config, write_keep_backup, CurrentLayout,
};
use crate::kanshi::ctl::KanshiClient;
use crate::model::layout::{validate, LayoutPlan};
use crate::model::output::Output;
use crate::model::snapshot::{EngineKind, Snapshot};
use crate::util;
use crate::watchdog::{self, RestoreReport};

#[derive(Debug, Clone)]
pub struct ApplyParams {
    pub paths: Paths,
    pub plan: LayoutPlan,
    pub state: AppState,
    pub set_key: String,
    pub internal: Option<String>,
    pub external: Option<String>,
    pub desktop_scales: BTreeMap<String, f64>,
    pub timeout_secs: u32,
}

/// Everything the UI needs during the countdown. Stored on disk too.
#[derive(Debug, Clone)]
pub struct PreparedTx {
    pub dir: PathBuf,
    pub engine: EngineKind,
    pub plan: LayoutPlan,
    pub state_after: Vec<u8>,
    /// The managed block to merge into the config if the layout is kept.
    pub managed_block: String,
    pub config_path: PathBuf,
    pub state_path: PathBuf,
    pub deadline: std::time::Instant,
    pub kanshi_installed: bool,
    pub kanshi_running: bool,
    pub config_writable: bool,
}

impl PreparedTx {
    /// Seconds left before the watchdog restores the snapshot.
    pub fn remaining_secs(&self) -> u32 {
        let left = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        (left.as_secs_f64().ceil() as u32).max(1)
    }
}

/// Load the freshly written config into kanshi. `kanshictl reload` can answer
/// `ProfileNotMatched` while the compositor is still settling the output
/// changes the apply just made, so retry once before giving up.
fn reload_kanshi(kanshi: &KanshiClient) -> Result<()> {
    match kanshi.reload() {
        Ok(()) => Ok(()),
        Err(first) => {
            std::thread::sleep(std::time::Duration::from_millis(150));
            kanshi
                .reload()
                .with_context(|| format!("kanshi rejected the generated configuration ({first:#})"))
        }
    }
}

/// Explain, output by output, why a plan does not match the live state.
fn describe_mismatch(plan: &LayoutPlan, live: &[Output]) -> String {
    let on_off = |on: bool| if on { "on" } else { "off" };
    let mut parts = Vec::new();
    // Positions are compared relative to the first enabled output, because a
    // compositor may translate the whole layout (see `normalize_origin`).
    let anchor = plan
        .outputs
        .iter()
        .find(|(name, planned)| planned.enabled && live.iter().any(|output| &&output.name == name))
        .map(|(name, planned)| {
            let actual = live
                .iter()
                .find(|output| &output.name == name)
                .expect("checked by find above");
            (name.clone(), planned.position, actual.position)
        });
    for (name, planned) in &plan.outputs {
        let Some(actual) = live.iter().find(|output| &output.name == name) else {
            parts.push(format!("{name}: not connected"));
            continue;
        };
        if planned.enabled != actual.enabled {
            parts.push(format!(
                "{name}: wanted {}, got {}",
                on_off(planned.enabled),
                on_off(actual.enabled)
            ));
            continue;
        }
        if !planned.enabled {
            continue;
        }
        if !planned.mode.same_timing(&actual.active_mode()) {
            parts.push(format!(
                "{name}: wanted mode {}, got {}",
                planned.mode.to_kanshi(),
                actual.active_mode().to_kanshi()
            ));
        }
        if (planned.scale - actual.scale).abs() > 0.01 {
            parts.push(format!(
                "{name}: wanted scale {}, got {}",
                crate::model::layout::format_scale(planned.scale),
                crate::model::layout::format_scale(actual.scale)
            ));
        }
        if let Some((anchor_name, anchor_planned, anchor_live)) = &anchor {
            let wanted = (
                planned.position.0 - anchor_planned.0,
                planned.position.1 - anchor_planned.1,
            );
            let got = (
                actual.position.0 - anchor_live.0,
                actual.position.1 - anchor_live.1,
            );
            if (wanted.0 - got.0).abs() > 2 || (wanted.1 - got.1).abs() > 2 {
                parts.push(format!(
                    "{name}: wanted {},{} relative to {anchor_name}, got {},{}",
                    wanted.0, wanted.1, got.0, got.1
                ));
            }
        }
        if planned.transform != actual.transform {
            parts.push(format!(
                "{name}: wanted transform {}, got {}",
                planned.transform.as_str(),
                actual.transform.as_str()
            ));
        }
    }
    if parts.is_empty() {
        "no visible difference".to_string()
    } else {
        parts.join(", ")
    }
}

/// Create a native mirror when two heads sit at the same position and the
/// compositor can do it. Compositors that share one global space (labwc, sway,
/// niri) already mirror overlapping outputs, so nothing is run there; COSMIC
/// keeps one workspace set per output, so an overlap only shares the pointer
/// and the windows stay on the panel.
///
/// This runs for a manual arrangement too: the preview treats overlapping
/// heads as a mirror, so the compositor has to as well.
fn mirror_outputs(params: &ApplyParams) -> Result<()> {
    if !cosmic::native_mirror_supported() {
        return Ok(());
    }
    let (Some(internal), Some(external)) = (params.internal.as_deref(), params.external.as_deref())
    else {
        return Ok(());
    };
    let enabled_position = |name: &str| {
        params
            .plan
            .outputs
            .get(name)
            .filter(|output| output.enabled)
            .map(|output| output.position)
    };
    let (Some(internal_position), Some(external_position)) =
        (enabled_position(internal), enabled_position(external))
    else {
        return Ok(());
    };
    if internal_position != external_position {
        return Ok(());
    }
    cosmic::mirror(external, internal)
        .with_context(|| format!("mirroring {external} onto {internal}"))
}

/// Break the native COSMIC mirrors that `plan` does not want.
///
/// Without this, a head that stays where it is keeps mirroring the old source.
/// "External only" is the worst case: the panel it mirrors is turned off, so
/// the external shows a grey picture.
fn break_unwanted_mirrors(live: &BTreeMap<String, String>, plan: &LayoutPlan) -> Result<()> {
    for (output, from) in stale_mirrors(live, plan) {
        let Some(planned) = plan.outputs.get(&output) else {
            continue;
        };
        cosmic::break_mirror(&output, planned)
            .with_context(|| format!("ending the mirror of {output} on {from}"))?;
    }
    Ok(())
}

/// Mirrors that exist now but that `plan` does not want.
fn stale_mirrors(live: &BTreeMap<String, String>, plan: &LayoutPlan) -> Vec<(String, String)> {
    let wanted = cosmic::mirror_pair(plan);
    let mut stale = Vec::new();
    for (output, from) in live {
        let keeps = wanted
            .as_ref()
            .is_some_and(|(external, internal)| external == output && internal == from);
        // A head the plan turns off is off anyway: no need to wake it up just
        // to unmirror it.
        let planned_enabled = plan
            .outputs
            .get(output)
            .is_some_and(|output| output.enabled);
        if !keeps && planned_enabled {
            stale.push((output.clone(), from.clone()));
        }
    }
    stale
}

/// The native mirrors that exist now, recorded so a revert can put them back.
///
/// Read once per apply: `cosmic-randr list` is a subprocess, and the same map
/// decides both which stale mirrors to break and what a revert restores.
fn snapshot_mirrors() -> Option<BTreeMap<String, String>> {
    if !cosmic::native_mirror_supported() {
        return None;
    }
    // Failing to list mirrors must not block an apply: nothing about the
    // layout needs the map unless a mirror actually exists.
    cosmic::mirrors().ok()
}

pub fn prepare_and_apply(mut params: ApplyParams) -> Result<PreparedTx> {
    let backend = backend::create();
    let kanshi = KanshiClient::new();
    let root = params.paths.transaction_root();
    fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;

    // One transaction at a time: restore any older pending layout first, then
    // read the compositor state again.
    watchdog::recover_older_transactions(&root, &root.join("__none__"));

    let live = backend.list()?;
    let plan_names: BTreeSet<&String> = params.plan.outputs.keys().collect();
    let live_names: BTreeSet<&String> = live.iter().map(|o| &o.name).collect();
    if plan_names != live_names {
        bail!(
            "The connected displays changed (expected {:?}, found {:?}). Refresh and try again.",
            plan_names,
            live_names
        );
    }
    let validation = validate(&params.plan, &live);
    if !validation.is_valid() {
        bail!("{}", validation.errors.join(" "));
    }
    // Anchor the layout at (0, 0) before anything is sent or written: COSMIC
    // would otherwise shift a negative coordinate and the verification below
    // would reject a layout the compositor had just accepted.
    params.plan.normalize_origin();
    // Validate before any side effect: a rejected configuration must not
    // create a transaction, spawn a watchdog or touch the config.
    backend.dry_run(&params.plan)?;

    let info = kanshi.detect(&params.paths.config_path);
    let engine = if info.engine_usable {
        EngineKind::Kanshi
    } else {
        EngineKind::Wlr
    };

    let dir = watchdog::create_transaction_dir(&root)?;
    let config_path = params.paths.config_path.clone();
    let state_path = params.paths.state_path.clone();
    let config_before = util::read_optional(&config_path)?;
    let state_before = util::read_optional(&state_path)?;
    let config_existed = config_before.is_some();
    let state_existed = state_before.is_some();
    if let Some(bytes) = &config_before {
        util::atomic_write(&dir.join("config.before"), bytes)?;
    }
    if let Some(bytes) = &state_before {
        util::atomic_write(&dir.join("state.before"), bytes)?;
    }

    // Snapshot of the live layout before any change.
    let mut state = params.state.clone();
    state.seed_internal_single(&live);
    state.record_set(
        &params.set_key,
        &params.plan,
        &live,
        params.internal.as_deref(),
        params.external.as_deref(),
        &params.desktop_scales,
    );
    let state_after = serde_json::to_vec_pretty(&state)?;

    let current = CurrentLayout::new(
        params.set_key.clone(),
        params.plan.clone(),
        live.clone(),
        params.desktop_scales.clone(),
    );
    // COSMIC has no overlap-based mirroring: a "Mirror" plan needs its own
    // command, both now and inside the generated profile (see below).
    let native_mirror = cosmic::native_mirror_supported();
    let block = generate_managed_block(&state, Some(&current), native_mirror);
    let candidate_config = merge_managed_block(config_before.as_deref(), &block).into_bytes();
    let config_writable = util::writable_target(&config_path);
    if config_writable {
        // Useful for stale-transaction inspection and manual recovery.
        let _ = util::atomic_write(&dir.join("candidate.config"), &candidate_config);
    }
    let _ = util::atomic_write(&dir.join("state.after"), &state_after);

    let mut snapshot = Snapshot::new(
        engine,
        info.running,
        config_path.to_string_lossy().into_owned(),
        state_path.to_string_lossy().into_owned(),
        config_existed,
        state_existed,
        std::process::id() as i32,
        0,
        &live,
    );
    let mirrors_before = snapshot_mirrors();
    snapshot.mirrored = mirrors_before
        .clone()
        .map(|mirrors| mirrors.into_iter().collect())
        .unwrap_or_default();
    util::atomic_write(
        &dir.join("state.json"),
        &serde_json::to_vec_pretty(&snapshot)?,
    )?;

    // Watchdog is alive before the first display/config change.
    let watchdog_pid = match watchdog::spawn_watchdog(&dir, params.timeout_secs) {
        Ok(pid) => pid,
        Err(err) => {
            watchdog::cleanup_transaction(&dir);
            return Err(err);
        }
    };
    snapshot.record_watchdog(watchdog_pid);
    util::atomic_write(
        &dir.join("state.json"),
        &serde_json::to_vec_pretty(&snapshot)?,
    )?;
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(u64::from(params.timeout_secs));

    let apply_result = (|| -> Result<()> {
        // COSMIC keeps a native mirror until the mirrored head is
        // reconfigured, and a mirror of a disabled head renders grey: break
        // every mirror this plan no longer wants before touching the outputs.
        if let Some(live) = &mirrors_before {
            break_unwanted_mirrors(live, &params.plan)?;
        }
        match engine {
            EngineKind::Kanshi => (|| -> Result<()> {
                // Write the config first: kanshi may see the output changes
                // below and re-apply whatever the file says, which must
                // already be the layout the user picked.
                write_config(&config_path, &candidate_config, config_before.as_deref())?;
                // Drive the compositor directly instead of letting kanshi do
                // it. kanshi 1.9 on labwc drops the "enabled" bit for a head
                // that is currently off, so "Extend" would leave the laptop
                // panel dark.
                backend.apply(&params.plan)?;
                // Now kanshi only has to load the same layout into memory.
                reload_kanshi(&kanshi)?;
                Ok(())
            })(),
            EngineKind::Wlr => backend.apply(&params.plan),
        }
    })();

    if let Err(err) = apply_result {
        // Restore config/outputs; a failed apply must leave the system as it
        // was. For a dry-run failure this is a harmless no-op.
        let _ = watchdog::restore_transaction(&dir);
        watchdog::cancel_and_cleanup(&dir);
        return Err(err.context("applying the display layout failed"));
    }

    // COSMIC keeps a workspace set per output, so two heads at the same
    // position are not a mirror there: ask the compositor for a native one.
    if let Err(err) = mirror_outputs(&params) {
        let _ = watchdog::restore_transaction(&dir);
        watchdog::cancel_and_cleanup(&dir);
        return Err(err.context("applying the display layout failed"));
    }

    // Verify what the compositor actually accepted.
    match backend.list() {
        Ok(after) => {
            if !params.plan.matches_live_relaxed(&after) {
                let mismatch = describe_mismatch(&params.plan, &after);
                let restore = watchdog::restore_transaction(&dir);
                watchdog::cancel_and_cleanup(&dir);
                let detail = match restore {
                    Ok(report) => format!("restored: {}", report.summary()),
                    Err(err) => format!("restore failed: {err:#}"),
                };
                bail!("The displays did not accept the new layout ({mismatch}); {detail}.");
            }
        }
        Err(err) => {
            let _ = watchdog::restore_transaction(&dir);
            watchdog::cancel_and_cleanup(&dir);
            return Err(err.context("verifying the applied layout"));
        }
    }

    Ok(PreparedTx {
        dir,
        engine,
        plan: params.plan,
        state_after,
        managed_block: block,
        config_path,
        state_path,
        deadline,
        kanshi_installed: info.installed,
        kanshi_running: info.running,
        config_writable,
    })
}

/// Persist the layout. Marker is written last: a crash before it makes the
/// watchdog restore everything.
pub fn finalize_keep(tx: &PreparedTx) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    if !tx.dir.is_dir() {
        bail!("the transaction has already finished; the layout was not saved");
    }
    // Hold the lock for the whole keep. Otherwise the watchdog can time out
    // between the last write and the marker and restore the old layout after
    // the user already confirmed the new one.
    let _lock = util::DirLock::acquire(&tx.dir)?;
    let result = (|| -> Result<()> {
        // 1. App state, with a .bak of the previous bytes.
        util::write_backup(&tx.state_path, &tx.state_path.with_extension("json.bak"))?;
        util::atomic_write(&tx.state_path, &tx.state_after)?;

        // 2. Config.
        match tx.engine {
            EngineKind::Kanshi => {
                write_keep_backup(&tx.config_path)?;
            }
            EngineKind::Wlr => {
                if tx.config_writable {
                    // Re-read the config now: the user (or another tool) may
                    // have edited the legacy section during the countdown.
                    let current = util::read_optional(&tx.config_path)?;
                    let candidate =
                        merge_managed_block(current.as_deref(), &tx.managed_block).into_bytes();
                    // Keep the exact bytes in the transaction so a failed
                    // keep can restore the file we actually wrote.
                    util::atomic_write(&tx.dir.join("candidate.config"), &candidate)?;
                    write_config(&tx.config_path, &candidate, current.as_deref())?;
                    let info = KanshiClient::new().detect(&tx.config_path);
                    if info.engine_usable {
                        KanshiClient::new()
                            .reload()
                            .with_context(|| "kanshi rejected the saved configuration")?;
                    }
                } else {
                    warnings.push(
                        "The kanshi config is not writable, so the layout was not saved."
                            .to_string(),
                    );
                }
            }
        }
        Ok(())
    })();

    if let Err(err) = result {
        // Keep the state, config and compositor consistent. We already hold
        // the lock, so use the unlocked restore to avoid deadlocking.
        let _ = watchdog::restore_transaction_locked(&tx.dir);
        watchdog::cancel_and_cleanup(&tx.dir);
        return Err(err.context("could not save the layout; the previous layout was restored"));
    }

    // 3. Marker last.
    watchdog::write_marker(&tx.dir, watchdog::MARKER_KEEP)?;

    if !tx.kanshi_installed {
        warnings.push(
            "kanshi is not installed; the layout applies now but will not be restored at login."
                .to_string(),
        );
    } else if !tx.kanshi_running {
        warnings.push(
            "kanshi is not running; start it or add it to autostart to re-apply this layout at login."
                .to_string(),
        );
    }
    if let Ok(live) = backend::create().list() {
        if !tx.plan.matches_live_relaxed(&live) {
            warnings.push("The display state changed after saving.".to_string());
        }
    }

    let _ = write_keep_backup(&tx.config_path);
    watchdog::cleanup_transaction(&tx.dir);
    Ok(warnings)
}

pub fn revert(tx: &PreparedTx) -> Result<RestoreReport> {
    if !tx.dir.exists() {
        // The watchdog already restored and removed/cleaned the transaction.
        return Ok(RestoreReport::default());
    }
    watchdog::revert_and_finish(&tx.dir)
}

/// Human readable warning about persistence, used before the countdown.
pub fn persistence_hint(tx: &PreparedTx) -> Option<String> {
    if !tx.kanshi_installed {
        Some("kanshi is not installed; this layout will not re-apply at login.".into())
    } else if !tx.kanshi_running {
        Some("kanshi is not running; the layout will apply now but not at login.".into())
    } else if !tx.config_writable {
        Some("The kanshi config is not writable; the layout will not be saved.".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::{LayoutKind, OutputPlan};
    use crate::model::output::{Mode, Transform};

    fn plan_with(edp: (bool, (i32, i32)), hdmi: (bool, (i32, i32))) -> LayoutPlan {
        let output = |(enabled, position): (bool, (i32, i32))| OutputPlan {
            enabled,
            mode: Mode::new(1920, 1200, 59_950),
            position,
            scale: 1.0,
            transform: Transform::Normal,
            adaptive_sync: false,
        };
        let mut outputs = BTreeMap::new();
        outputs.insert("eDP-1".to_string(), output(edp));
        outputs.insert("HDMI-A-1".to_string(), output(hdmi));
        LayoutPlan {
            kind: LayoutKind::Custom,
            outputs,
        }
    }

    fn live_mirror() -> BTreeMap<String, String> {
        BTreeMap::from([("HDMI-A-1".to_string(), "eDP-1".to_string())])
    }

    #[test]
    fn a_wanted_mirror_is_left_alone() {
        let plan = plan_with((true, (0, 0)), (true, (0, 0)));
        assert!(stale_mirrors(&live_mirror(), &plan).is_empty());
    }

    #[test]
    fn external_only_breaks_the_mirror_of_the_disabled_panel() {
        // The bug behind the grey screen: HDMI keeps mirroring eDP while eDP
        // is turned off.
        let plan = plan_with((false, (0, 0)), (true, (0, 0)));
        assert_eq!(
            stale_mirrors(&live_mirror(), &plan),
            vec![("HDMI-A-1".to_string(), "eDP-1".to_string())]
        );
    }

    #[test]
    fn moving_a_head_breaks_its_mirror() {
        let plan = plan_with((true, (0, 0)), (true, (1920, 0)));
        assert_eq!(stale_mirrors(&live_mirror(), &plan).len(), 1);
    }

    #[test]
    fn a_head_that_gets_turned_off_needs_no_unmirror() {
        let plan = plan_with((true, (0, 0)), (false, (0, 0)));
        assert!(stale_mirrors(&live_mirror(), &plan).is_empty());
    }
}
