//! `wlr-randr` backend: JSON parsing and atomic configuration applies.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use super::{BackendInfo, OutputBackend};
use crate::model::layout::LayoutPlan;
use crate::model::output::{Mode, Output, Transform};
use crate::util::{self, CmdOutput};

pub const ENV_WLR_RANDR: &str = "KANSHI_GTK4_WLR_RANDR";

#[derive(Debug, Deserialize)]
struct RawOutput {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    make: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    serial: Option<String>,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    modes: Vec<RawMode>,
    #[serde(default)]
    position: RawPosition,
    #[serde(default = "one")]
    scale: f64,
    #[serde(default)]
    transform: Option<String>,
    #[serde(default)]
    adaptive_sync: bool,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Default, Deserialize)]
struct RawPosition {
    #[serde(default)]
    x: i32,
    #[serde(default)]
    y: i32,
}

#[derive(Debug, Deserialize)]
struct RawMode {
    width: u32,
    height: u32,
    refresh: f64,
    #[serde(default)]
    preferred: bool,
    #[serde(default)]
    current: bool,
}

#[derive(Debug, Clone)]
pub struct WlrRandrBackend {
    program: PathBuf,
}

impl Default for WlrRandrBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WlrRandrBackend {
    pub fn new() -> Self {
        Self {
            program: util::program_from_env(ENV_WLR_RANDR, "wlr-randr"),
        }
    }

    fn run(&self, args: Vec<OsString>, timeout: Duration) -> Result<CmdOutput> {
        util::run_command(&self.program, &args, timeout)
    }

    pub fn parse_json(bytes: &[u8]) -> Result<Vec<Output>> {
        let raw: Vec<RawOutput> =
            serde_json::from_slice(bytes).context("parsing wlr-randr --json output")?;
        let mut outputs = Vec::with_capacity(raw.len());
        for item in raw {
            if item.name.trim().is_empty() {
                bail!("wlr-randr reported an output without a name");
            }
            let modes = item
                .modes
                .into_iter()
                .map(|m| Mode {
                    width: m.width,
                    height: m.height,
                    refresh_mhz: (m.refresh.max(0.0) * 1000.0).round() as u32,
                    preferred: m.preferred,
                    current: m.current,
                    custom: false,
                })
                .collect();
            outputs.push(Output {
                name: item.name,
                description: item.description,
                make: item.make,
                model: item.model,
                serial: item.serial,
                enabled: item.enabled,
                modes,
                position: (item.position.x, item.position.y),
                scale: item.scale,
                transform: item
                    .transform
                    .as_deref()
                    .and_then(Transform::parse)
                    .unwrap_or_default(),
                adaptive_sync: item.adaptive_sync,
            });
        }
        Ok(outputs)
    }

    /// Build argv for a full plan. `live` is used to only emit adaptive-sync
    /// when the value actually changed (older compositors do not support it).
    pub fn build_args(plan: &LayoutPlan, live: Option<&[Output]>) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        for (name, output) in &plan.outputs {
            args.push("--output".into());
            args.push(name.into());
            if !output.enabled {
                args.push("--off".into());
                continue;
            }
            args.push("--on".into());
            // A synthesized resolution is not in the advertised list, so it
            // needs the compositor's custom-mode request. The live-list
            // fallback also covers snapshots taken while a custom mode was
            // active: it is only advertised while it is current.
            let custom = output.mode.custom
                || live
                    .and_then(|outputs| outputs.iter().find(|live| &live.name == name))
                    .is_some_and(|live| live.find_mode(&output.mode).is_none());
            args.push(if custom { "--custom-mode" } else { "--mode" }.into());
            args.push(output.mode.to_kanshi().into());
            // Plan positions are already in the compositor's global logical
            // coordinate space, which is exactly what wlr-randr --pos takes:
            // it forwards the value unchanged to
            // zwlr_output_configuration_head_v1_set_position. Do not multiply
            // by the output scale here, or the head lands `scale` times too
            // far right and the post-apply verification fails.
            args.push("--pos".into());
            args.push(format!("{},{}", output.position.0, output.position.1).into());
            args.push("--scale".into());
            args.push(crate::model::layout::format_scale(output.scale).into());
            args.push("--transform".into());
            args.push(output.transform.as_str().into());
            let changed = live
                .and_then(|outputs| outputs.iter().find(|o| &o.name == name))
                .map(|o| o.adaptive_sync != output.adaptive_sync)
                .unwrap_or(true);
            if changed {
                args.push("--adaptive-sync".into());
                args.push(
                    if output.adaptive_sync {
                        "enabled"
                    } else {
                        "disabled"
                    }
                    .into(),
                );
            }
        }
        args
    }
}

impl OutputBackend for WlrRandrBackend {
    fn detect(&self) -> Result<BackendInfo> {
        if !util::command_exists(&self.program) {
            return Ok(BackendInfo::unsupported(format!(
                "wlr-randr was not found ({}). Install it with: sudo pacman -S wlr-randr",
                self.program.display()
            )));
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("WAYLAND_SOCKET").is_none()
        {
            return Ok(BackendInfo::unsupported(
                "No Wayland session detected. This app supports labwc and niri, which \
                 expose zwlr_output_manager_v1.",
            ));
        }

        let mut version = None;
        let wayland_info = util::program_from_env("KANSHI_GTK4_WAYLAND_INFO", "wayland-info");
        if util::command_exists(&wayland_info) {
            if let Ok(out) = util::run_command(&wayland_info, &[], Duration::from_secs(5)) {
                if out.success() {
                    let mut found = false;
                    for line in out.stdout.lines() {
                        if line.contains("zwlr_output_manager_v1") {
                            found = true;
                            version = line
                                .split("version:")
                                .nth(1)
                                .and_then(|v| v.trim().parse().ok());
                        }
                    }
                    if !found {
                        return Ok(BackendInfo::unsupported(
                            "This compositor does not support zwlr_output_manager_v1. \
                             The app supports labwc and niri; other compositors may work \
                             but are untested. GNOME, KDE and Hyprland are not supported.",
                        ));
                    }
                    return Ok(BackendInfo {
                        supported: true,
                        reason: None,
                        protocol_version: version,
                    });
                }
            }
        }

        // wayland-info missing or failed: fall back to a real wlr-randr call.
        match self.list() {
            Ok(_) => Ok(BackendInfo {
                supported: true,
                reason: None,
                protocol_version: version,
            }),
            Err(err) => Ok(BackendInfo::unsupported(format!(
                "The compositor did not answer wlr-randr: {err:#}"
            ))),
        }
    }

    fn list(&self) -> Result<Vec<Output>> {
        let out = self.run(vec!["--json".into()], Duration::from_secs(8))?;
        if !out.success() {
            bail!("wlr-randr --json failed: {}", out.error_message());
        }
        Self::parse_json(out.stdout.as_bytes())
    }

    fn dry_run(&self, plan: &LayoutPlan) -> Result<()> {
        let live = self.list().ok();
        let mut args = vec!["--dryrun".into()];
        args.extend(Self::build_args(plan, live.as_deref()));
        let out = self.run(args, Duration::from_secs(10))?;
        if !out.success() {
            bail!("Dry run failed: {}", out.error_message());
        }
        Ok(())
    }

    fn apply(&self, plan: &LayoutPlan) -> Result<()> {
        let live = self.list().ok();
        let args = Self::build_args(plan, live.as_deref());
        let out = self.run(args, Duration::from_secs(15))?;
        if !out.success() {
            let message = out.error_message();
            if message.contains("cancelled") {
                bail!("Displays changed during apply, please try again ({message})");
            }
            bail!("wlr-randr failed: {message}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::{LayoutKind, OutputPlan};
    use std::collections::BTreeMap;

    const FIXTURE: &str = include_str!("../../tests/fixtures/wlr-randr.json");

    #[test]
    fn parses_fixture() {
        let outputs = WlrRandrBackend::parse_json(FIXTURE.as_bytes()).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].name, "eDP-1");
        assert_eq!(outputs[0].active_mode().refresh_mhz, 120_000);
        assert_eq!(
            outputs[0]
                .active_mode()
                .logical_size(outputs[0].scale, outputs[0].transform),
            (1560, 1040)
        );
        assert_eq!(outputs[1].name, "DP-1");
        assert_eq!(outputs[1].scale, 1.5);
        assert!(!outputs[1].enabled);
    }

    fn args_as_string(plan: &LayoutPlan, live: Option<&[Output]>) -> String {
        WlrRandrBackend::build_args(plan, live)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn builds_full_command() {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".into(),
            OutputPlan {
                enabled: true,
                mode: Mode::new(3120, 2080, 120_000),
                position: (0, 0),
                scale: 2.0,
                transform: Transform::Normal,
                adaptive_sync: false,
            },
        );
        outputs.insert(
            "DP-1".into(),
            OutputPlan {
                enabled: true,
                mode: Mode::new(1920, 1200, 60_000),
                position: (1560, 0),
                scale: 1.5,
                transform: Transform::Normal,
                adaptive_sync: true,
            },
        );
        let plan = LayoutPlan {
            kind: LayoutKind::Extend,
            outputs,
        };
        let cmd = args_as_string(&plan, None);
        assert_eq!(
            cmd,
            "--output DP-1 --on --mode 1920x1200@60 --pos 1560,0 --scale 1.5 --transform normal \
             --adaptive-sync enabled --output eDP-1 --on --mode 3120x2080@120 --pos 0,0 --scale 2 \
             --transform normal --adaptive-sync disabled"
        );
    }

    #[test]
    fn custom_mode_uses_custom_mode_option() {
        let mut mode = Mode::new(1920, 1280, 120_000);
        mode.custom = true;
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".into(),
            OutputPlan {
                enabled: true,
                mode,
                position: (0, 0),
                scale: 2.0,
                transform: Transform::Normal,
                adaptive_sync: false,
            },
        );
        let plan = LayoutPlan {
            kind: LayoutKind::Custom,
            outputs,
        };
        let cmd = args_as_string(&plan, None);
        assert!(cmd.contains("--custom-mode 1920x1280@120"), "{cmd}");
        assert!(!cmd.contains("--mode 1920x1280@120"), "{cmd}");
    }

    #[test]
    fn an_unadvertised_mode_falls_back_to_custom_mode() {
        let live = WlrRandrBackend::parse_json(FIXTURE.as_bytes()).unwrap();
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".into(),
            OutputPlan {
                enabled: true,
                mode: Mode::new(1920, 1280, 120_000),
                position: (0, 0),
                scale: 2.0,
                transform: Transform::Normal,
                adaptive_sync: false,
            },
        );
        let plan = LayoutPlan {
            kind: LayoutKind::Custom,
            outputs,
        };
        let cmd = args_as_string(&plan, Some(&live));
        assert!(cmd.contains("--custom-mode 1920x1280@120"), "{cmd}");
    }

    #[test]
    fn adaptive_sync_only_when_changed() {
        let outputs = WlrRandrBackend::parse_json(FIXTURE.as_bytes()).unwrap();
        let mut plan = LayoutPlan {
            kind: LayoutKind::Custom,
            outputs: BTreeMap::new(),
        };
        for o in &outputs {
            plan.outputs.insert(
                o.name.clone(),
                OutputPlan {
                    enabled: o.enabled,
                    mode: o.active_mode(),
                    position: o.position,
                    scale: o.scale,
                    transform: o.transform,
                    adaptive_sync: o.adaptive_sync,
                },
            );
        }
        let cmd = args_as_string(&plan, Some(&outputs));
        assert!(!cmd.contains("adaptive-sync"));
    }

    #[test]
    fn disabled_output_uses_off() {
        let outputs = WlrRandrBackend::parse_json(FIXTURE.as_bytes()).unwrap();
        let plan = LayoutPlan::from_outputs(
            &outputs,
            LayoutKind::Single,
            Some("eDP-1"),
            Some("DP-1"),
            &BTreeMap::new(),
        );
        let cmd = args_as_string(&plan, None);
        assert!(cmd.contains("--output DP-1 --off"));
    }
}
