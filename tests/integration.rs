//! End-to-end tests for the apply/keep/revert state machine.
//!
//! No compositor is needed: `wlr-randr` and `kanshictl` are replaced by the
//! mocks in `tests/`, and the watchdog is the real binary spawned by
//! `apply::prepare_and_apply`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::time::{Duration, Instant};

use common::{MockSession, ALT_FIXTURE, DESKTOP_FIXTURE, LEGACY_CONFIG};
use kanshi_gtk4::apply;
use kanshi_gtk4::config::AppState;
use kanshi_gtk4::kanshi::config::{BLOCK_START, PRESERVED};
use kanshi_gtk4::model::output::Mode;
use kanshi_gtk4::model::snapshot::EngineKind;
use kanshi_gtk4::watchdog;

#[test]
fn wlr_engine_apply_and_keep_persists_everything() {
    let session = MockSession::new();
    session.use_wlr_engine();
    let before = session.config();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30)).expect("apply");
    assert_eq!(tx.engine, EngineKind::Wlr);

    // The compositor state changed...
    let dp = session.output("DP-1");
    assert!(dp.enabled);
    assert_eq!(dp.position, (1560, 0));
    // ...but the config is only written on Keep.
    assert_eq!(session.config(), before);

    let warnings = apply::finalize_keep(&tx).expect("keep");
    assert!(
        warnings.iter().any(|w| w.contains("not installed")),
        "{warnings:?}"
    );

    let config = session.config_text();
    assert!(config.starts_with(BLOCK_START));
    assert!(config.contains("profile auto-extend {"));
    assert!(config.contains("position 1560,0"));
    assert!(config.contains("profile single"), "legacy profile lost");
    assert!(config.contains("# hand-written"), "legacy comment lost");

    let state = AppState::load(&session.state_path).expect("state");
    let set = &state.sets["eDP-1+DP-1"];
    assert_eq!(set.outputs["DP-1"].mode, "1920x1200@60");

    assert!(!tx.dir.exists(), "transaction directory must be removed");
    assert!(session.pending_dirs().is_empty());
    assert!(session
        .config_path
        .with_extension("kanshi-gtk4.bak")
        .exists());
}

#[test]
fn custom_mode_applies_and_is_persisted_as_mode_custom() {
    let session = MockSession::new();
    session.use_wlr_engine();

    let mut plan = session.extend_plan();
    let mut mode = Mode::new(2340, 1560, 120_000);
    mode.custom = true;
    plan.outputs.get_mut("eDP-1").unwrap().mode = mode;

    let tx = apply::prepare_and_apply(session.apply_params(plan, 30)).expect("apply");
    assert_eq!(tx.engine, EngineKind::Wlr);

    let active = session.output("eDP-1").active_mode();
    assert_eq!((active.width, active.height), (2340, 1560));
    assert_eq!(active.refresh_mhz, 120_000);

    apply::finalize_keep(&tx).expect("keep");
    let config = session.config_text();
    assert!(
        config.contains("mode --custom 2340x1560@120"),
        "missing custom mode in config:\n{config}"
    );

    let state = AppState::load(&session.state_path).expect("state");
    let output = &state.sets["eDP-1+DP-1"].outputs["eDP-1"];
    assert_eq!(output.mode, "2340x1560@120");
    assert!(output.custom);
}

#[test]
fn cosmic_mirror_apply_asks_for_a_native_mirror() {
    let session = MockSession::new();
    session.use_wlr_engine();
    session.enable_native_mirror();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.mirror_plan(), 30)).expect("apply");
    assert_eq!(tx.engine, EngineKind::Wlr);

    // COSMIC needs the extra command: overlapping heads do not mirror.
    assert_eq!(session.cosmic_calls(), vec!["mirror DP-1 eDP-1"]);

    apply::finalize_keep(&tx).expect("keep");
    let config = session.config_text();
    let start = config.find("profile auto-mirror").expect("mirror profile");
    let mirror = config[start..].split("\nprofile ").next().unwrap();
    assert!(
        mirror.contains("exec ") && mirror.contains("mirror DP-1 eDP-1"),
        "missing exec line in:\n{mirror}"
    );
}

#[test]
fn extend_apply_does_not_touch_the_compositor_mirror() {
    let session = MockSession::new();
    session.use_wlr_engine();
    session.enable_native_mirror();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30)).expect("apply");
    assert!(
        session.cosmic_calls().is_empty(),
        "{:?}",
        session.cosmic_calls()
    );
    apply::finalize_keep(&tx).expect("keep");
    let config = session.config_text();
    let start = config.find("profile auto-extend").unwrap();
    let extend = config[start..].split("\nprofile ").next().unwrap();
    assert!(!extend.contains("exec "), "{extend}");
    // The mirror profile is still generated for this output set, so it keeps
    // its own exec line: switching to it later must still mirror.
    assert!(config.contains("exec ") && config.contains("mirror DP-1 eDP-1"));
}

#[test]
fn revert_restores_outputs_and_config_bytes() {
    let session = MockSession::new();
    session.use_wlr_engine();
    let before = session.config();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30)).expect("apply");
    assert!(session.output("DP-1").enabled);

    let report = apply::revert(&tx).expect("revert");
    assert!(report.fully_restored());

    let dp = session.output("DP-1");
    assert!(!dp.enabled);
    assert_eq!(session.config(), before, "config bytes changed");
    assert!(!tx.dir.exists());
    assert!(session.pending_dirs().is_empty());
}

#[test]
fn watchdog_restores_when_the_app_never_confirms() {
    let session = MockSession::new();
    session.use_wlr_engine();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.extend_plan(), 1)).expect("apply");
    assert!(session.output("DP-1").enabled);

    // Simulate a crash: no Keep, no Revert, no marker.
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if !session.output("DP-1").enabled {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let dp = session.output("DP-1");
    assert!(!dp.enabled, "watchdog did not restore the layout");

    let log = fs::read_to_string(tx.dir.join("watchdog.log")).unwrap_or_default();
    assert!(log.contains("restored"), "{log}");
    assert_eq!(watchdog::read_marker(&tx.dir).as_deref(), Some("cancel"));

    // Idempotent: cleaning up again is harmless.
    apply::revert(&tx).expect("second revert");
    assert!(!tx.dir.exists());
}

#[test]
fn kanshi_engine_applies_the_generated_profile_and_keeps_it() {
    let session = MockSession::new();
    let _before = session.config();

    let tx =
        apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30)).expect("apply");
    assert_eq!(tx.engine, EngineKind::Kanshi);

    // The provisional config was written before the reload and is live now.
    let provisional = session.config_text();
    assert!(provisional.starts_with(BLOCK_START));
    assert!(provisional.contains("profile auto-extend {"));
    assert!(provisional.contains("profile single"));
    assert_eq!(
        provisional.matches(LEGACY_CONFIG.trim()).count(),
        1,
        "legacy section must be preserved exactly once"
    );
    let dp = session.output("DP-1");
    assert!(dp.enabled);
    assert_eq!(dp.position, (1560, 0));

    apply::finalize_keep(&tx).expect("keep");
    assert_eq!(session.config(), provisional.as_bytes());
    assert!(session
        .config_path
        .with_extension("kanshi-gtk4.bak")
        .exists());
    assert!(!tx.dir.exists());
}

#[test]
fn kanshi_engine_turns_a_dark_head_on_before_reloading() {
    // DP-1 starts disabled, like the laptop panel after External only.
    let session = MockSession::new();
    // kanshi 1.9 on labwc drops the enabled bit: only the pre-enable saves us.
    std::env::set_var("MOCK_KANSHI_IGNORE_ENABLE", "1");

    let tx = apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30))
        .expect("apply must succeed");
    assert_eq!(tx.engine, EngineKind::Kanshi);

    let dp = session.output("DP-1");
    assert!(dp.enabled, "kanshi reload left the head dark");
    assert_eq!(dp.position, (1560, 0));

    let log = fs::read_to_string(&session.wlr_log).unwrap_or_default();
    assert!(log.contains("--output DP-1 --on"), "{log}");

    apply::finalize_keep(&tx).expect("keep");
    assert!(session.output("DP-1").enabled);
}

#[test]
fn other_output_names_and_fractional_scales() {
    // eDP-2 2880x1800 @ 90 Hz, scale 1.75; DP-2 3840x2160 @ 143.998 Hz,
    // scale 1.5, adaptive sync on.
    let session = MockSession::with_fixture(ALT_FIXTURE, Some("eDP-2"), Some("DP-2"));
    session.use_wlr_engine();

    let plan = session.extend_plan();
    assert_eq!(plan.outputs["eDP-2"].position, (0, 0));
    assert_eq!(plan.outputs["DP-2"].position, (1646, 0));
    assert_eq!(plan.outputs["eDP-2"].mode.to_kanshi(), "2880x1800@90");
    assert_eq!(plan.outputs["DP-2"].mode.to_kanshi(), "3840x2160@143.998");

    let tx = apply::prepare_and_apply(session.apply_params(plan, 30)).expect("apply");
    assert_eq!(session.output("DP-2").position, (1646, 0));
    assert_eq!(session.output("DP-2").scale, 1.5);
    assert!(session.output("DP-2").adaptive_sync);

    apply::finalize_keep(&tx).expect("keep");
    let config = session.config_text();
    assert!(config.contains("profile auto-extend {"), "{config}");
    assert!(config.contains("mode 2880x1800@90"), "{config}");
    assert!(config.contains("scale 1.75"), "{config}");
    assert!(config.contains("mode 3840x2160@143.998"), "{config}");
    assert!(config.contains("scale 1.5"), "{config}");
    assert!(config.contains("adaptive_sync on"), "{config}");
}

#[test]
fn each_display_keeps_its_own_resolution() {
    let session = MockSession::with_fixture(ALT_FIXTURE, Some("eDP-2"), Some("DP-2"));
    session.use_wlr_engine();

    let outputs = session.outputs();
    let mode = |name: &str, width: u32, height: u32, refresh_mhz: u32| {
        outputs
            .iter()
            .find(|o| o.name == name)
            .and_then(|o| {
                o.modes
                    .iter()
                    .find(|m| (m.width, m.height, m.refresh_mhz) == (width, height, refresh_mhz))
                    .cloned()
            })
            .unwrap_or_else(|| panic!("{name} does not advertise {width}x{height}@{refresh_mhz}"))
    };

    let mut plan = session.extend_plan();
    // The user picks a different resolution and refresh per display.
    plan.outputs.get_mut("eDP-2").unwrap().mode = mode("eDP-2", 1920, 1080, 60_000);
    plan.outputs.get_mut("DP-2").unwrap().mode = mode("DP-2", 2560, 1440, 120_000);
    let scales: BTreeMap<String, f64> = outputs
        .iter()
        .map(|output| (output.name.clone(), output.scale))
        .collect();
    plan.rebuild(Some("eDP-2"), Some("DP-2"), &scales);
    // 1920 / scale 1.75 = 1097.14 -> 1097.
    assert_eq!(plan.outputs["DP-2"].position, (1097, 0));

    let tx = apply::prepare_and_apply(session.apply_params(plan, 30)).expect("apply");
    assert_eq!(
        session.output("eDP-2").active_mode().to_kanshi(),
        "1920x1080@60"
    );
    assert_eq!(
        session.output("DP-2").active_mode().to_kanshi(),
        "2560x1440@120"
    );
    assert_eq!(session.output("DP-2").position, (1097, 0));
    assert_eq!(session.output("eDP-2").scale, 1.75);

    apply::finalize_keep(&tx).expect("keep");
    let config = session.config_text();
    assert!(config.contains("mode 1920x1080@60"), "{config}");
    assert!(config.contains("mode 2560x1440@120"), "{config}");
    assert!(config.contains("scale 1.75"), "{config}");

    let state = AppState::load(&session.state_path).expect("state");
    let set = &state.sets["eDP-2+DP-2"];
    assert_eq!(set.outputs["eDP-2"].mode, "1920x1080@60");
    assert_eq!(set.outputs["DP-2"].mode, "2560x1440@120");
    assert_eq!(set.outputs["DP-2"].position, Some((1097, 0)));

    // The other generated profiles for this output set keep the per-display
    // choices instead of falling back to the live modes.
    let mirror = config
        .split("profile auto-mirror {")
        .nth(1)
        .expect("mirror profile");
    assert!(mirror.contains("mode 1920x1080@60"), "{config}");
    assert!(mirror.contains("mode 2560x1440@120"), "{config}");
}

#[test]
fn desktop_without_internal_panel_extends_the_second_output() {
    // No eDP-* head: DP-1 is the anchor, DP-2 the preset partner.
    let session = MockSession::with_fixture(DESKTOP_FIXTURE, None, Some("DP-2"));
    session.use_wlr_engine();
    assert_eq!(session.set_key, "DP-1+DP-2");

    let plan = session.extend_plan();
    assert_eq!(plan.outputs["DP-1"].position, (0, 0));
    assert_eq!(plan.outputs["DP-1"].scale, 1.25);
    assert_eq!(plan.outputs["DP-2"].position, (2048, 0));

    let tx = apply::prepare_and_apply(session.apply_params(plan, 30)).expect("apply");
    assert_eq!(session.output("DP-2").position, (2048, 0));
    apply::finalize_keep(&tx).expect("keep");

    let config = session.config_text();
    assert!(config.contains("profile auto-extend {"), "{config}");
    assert!(config.contains("profile auto-mirror {"), "{config}");
    assert!(config.contains("mode 2560x1440@164.999"), "{config}");
    assert!(config.contains("scale 1.25"), "{config}");
}

#[test]
fn kanshi_reload_failure_restores_the_previous_config() {
    let session = MockSession::new();
    std::env::set_var("MOCK_KANSHI_RELOAD_FAIL", "1");
    let before = session.config();

    let err = apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30))
        .expect_err("reload must fail");
    let message = format!("{err:#}");
    assert!(message.contains("kanshi rejected"), "{message}");

    assert_eq!(session.config(), before, "config bytes changed");
    assert!(!session.output("DP-1").enabled, "display changed");
    assert!(session.pending_dirs().is_empty(), "transaction left behind");
}

#[test]
fn dry_run_failure_changes_nothing() {
    let session = MockSession::new();
    session.use_wlr_engine();
    std::env::set_var("MOCK_WLR_DRYRUN_FAIL", "1");
    let before = session.config();

    let err = apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30))
        .expect_err("dry run must fail");
    assert!(format!("{err:#}").contains("Dry run failed"));

    assert_eq!(session.config(), before);
    assert!(!session.output("DP-1").enabled);
    assert!(session.pending_dirs().is_empty());
}

#[test]
fn cancelled_apply_reports_the_hotplug_race() {
    let session = MockSession::new();
    session.use_wlr_engine();
    std::env::set_var("MOCK_WLR_CANCEL", "1");

    let err = apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30))
        .expect_err("apply must be cancelled");
    assert!(format!("{err:#}").contains("Displays changed during apply"));
    assert!(session.pending_dirs().is_empty());
}

#[test]
fn verification_mismatch_reverts_and_reports() {
    let session = MockSession::new();
    session.use_wlr_engine();
    std::env::set_var("MOCK_WLR_IGNORE_APPLY", "1");
    let before = session.config();

    let err = apply::prepare_and_apply(session.apply_params(session.extend_plan(), 30))
        .expect_err("verification must fail");
    let message = format!("{err:#}");
    assert!(message.contains("did not accept"), "{message}");
    assert_eq!(session.config(), before);
    assert!(session.pending_dirs().is_empty());
}

/// COSMIC stores output positions as unsigned values and shifts a
/// configuration with a negative coordinate into the positive quadrant. The
/// layout is accepted (the external display really is on the left), so the
/// apply must not be reported as a failure and reverted.
#[test]
fn left_of_panel_layout_is_kept_when_the_compositor_normalizes_it() {
    let session = MockSession::new();
    session.use_wlr_engine();
    session.normalize_origin_like_cosmic();
    let before = session.config();

    let tx = apply::prepare_and_apply(session.apply_params(session.extend_left_plan(), 30))
        .expect("apply must be accepted");

    // The compositor moved the whole layout to the origin...
    assert_eq!(session.output("DP-1").position, (0, 0));
    assert_eq!(session.output("eDP-1").position, (1560, 0));
    // ...and the transaction is still pending, not reverted.
    assert_eq!(session.config(), before);
    assert_eq!(session.pending_dirs().len(), 1);

    apply::finalize_keep(&tx).expect("keep");
    // The saved profile keeps the relative arrangement, at (0, 0).
    let config = session.config_text();
    assert!(config.contains("output DP-1 {"), "{config}");
    assert!(config.contains("position 0,0"), "{config}");
    assert!(config.contains("position 1560,0"), "{config}");
    assert!(session.pending_dirs().is_empty());
}

#[test]
fn print_config_cli_writes_a_managed_block() {
    let session = MockSession::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanshi-gtk4"))
        .arg("--print-config")
        .arg("--config")
        .arg(&session.config_path)
        .arg("--state-dir")
        .arg(session.state_path.parent().unwrap())
        .output()
        .expect("running kanshi-gtk4");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with(BLOCK_START), "{stdout}");
    assert!(stdout.contains("profile auto-single {"), "{stdout}");
    assert!(stdout.contains(PRESERVED), "legacy text lost: {stdout}");
    // Read-only: the real config is untouched.
    assert_eq!(session.config_text(), LEGACY_CONFIG);
}
