//! Native mirroring on COSMIC.
//!
//! COSMIC keeps one workspace set per output, so two heads placed at the same
//! position do not show the same thing: the pointer is shared because it lives
//! in the global space, but each head renders its own workspaces, so windows
//! stay on the head that owns them. A real mirror is a compositor feature
//! (`zcosmic_output_configuration_v1`), reachable from the command line with
//! `cosmic-randr mirror <output> <from>`.
//!
//! `wlr-randr` cannot express a mirror, and applying a layout through it does
//! not drop one either: COSMIC keeps the relationship until the mirrored head
//! is reconfigured, even when the source is disabled. A mirror of a disabled
//! head renders grey, so a plan that does not want a mirror has to break it
//! first with `cosmic-randr mode` plus a position.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use crate::model::layout::{format_scale, LayoutPlan, OutputPlan};
use crate::model::output::{is_internal_name, Transform};
use crate::util::{self, program_from_env};

pub const ENV_COSMIC_RANDR: &str = "KANSHI_GTK4_COSMIC_RANDR";
/// Force native mirroring on (`1`) or off (`0`), for tests and for sessions
/// that do not export the desktop environment variables.
pub const ENV_NATIVE_MIRROR: &str = "KANSHI_GTK4_NATIVE_MIRROR";

const TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this is a COSMIC session, judged from the desktop environment
/// variables so that no subprocess is needed on every redraw.
pub fn is_cosmic() -> bool {
    [
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "DESKTOP_SESSION",
    ]
    .iter()
    .filter_map(|key| std::env::var(key).ok())
    .any(|value| value.to_ascii_lowercase().contains("cosmic"))
}

/// Native mirroring is possible when this is a COSMIC session and
/// `cosmic-randr` is installed.
pub fn native_mirror_supported() -> bool {
    match std::env::var(ENV_NATIVE_MIRROR).as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => is_cosmic() && program_exists(&program_from_env(ENV_COSMIC_RANDR, "cosmic-randr")),
    }
}

/// Make `output` show the same content as `from`.
pub fn mirror(output: &str, from: &str) -> Result<()> {
    let program = program_from_env(ENV_COSMIC_RANDR, "cosmic-randr");
    let args: Vec<OsString> = ["mirror".to_string(), output.to_string(), from.to_string()]
        .into_iter()
        .map(OsString::from)
        .collect();
    let result = util::run_command(&program, &args, TIMEOUT)
        .with_context(|| format!("running {} mirror", program.display()))?;
    if !result.success() {
        bail!(
            "{} mirror {output} {from} failed: {}",
            program.display(),
            result.error_message()
        );
    }
    Ok(())
}

/// The `exec` line that re-creates a mirror from inside a kanshi profile, so a
/// later profile change keeps showing the same content on both heads.
pub fn mirror_command(output: &str, from: &str) -> String {
    let program = program_from_env(ENV_COSMIC_RANDR, "cosmic-randr");
    let args: Vec<OsString> = ["mirror".to_string(), output.to_string(), from.to_string()]
        .into_iter()
        .map(OsString::from)
        .collect();
    exec_command(&program, &args)
}

/// Look the program up in `PATH` when it is a bare name.
fn program_exists(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Live native mirrors, as `mirrored head -> source`.
///
/// COSMIC keeps a mirror until the mirrored head is reconfigured, and it does
/// not drop it when the source is disabled: a stale mirror renders a grey
/// picture. `cosmic-randr list` prints `(mirroring "eDP-1")` next to the head.
pub fn mirrors() -> Result<BTreeMap<String, String>> {
    let program = program_from_env(ENV_COSMIC_RANDR, "cosmic-randr");
    let out = util::run_command(&program, &["list".into()], TIMEOUT)
        .with_context(|| format!("running {} list", program.display()))?;
    if !out.success() {
        bail!("{} list failed: {}", program.display(), out.error_message());
    }
    Ok(parse_mirrors(&out.stdout))
}

fn parse_mirrors(text: &str) -> BTreeMap<String, String> {
    let mut mirrors = BTreeMap::new();
    for line in text.lines() {
        let line = strip_ansi(line);
        let Some((head, rest)) = line.split_once("(mirroring \"") else {
            continue;
        };
        let Some(from) = rest.strip_suffix("\")") else {
            continue;
        };
        let head = head.trim();
        if !head.is_empty() && !from.is_empty() {
            mirrors.insert(head.to_string(), from.to_string());
        }
    }
    mirrors
}

/// Drop ANSI colour codes: `cosmic-randr list` colours head names and states.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // CSI sequence: ESC [ … final byte. Colour codes end with 'm'.
        for next in chars.by_ref() {
            if next.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// Arguments that make one head independent at its planned settings.
///
/// `cosmic-randr mode` keeps an existing mirror when no position is given
/// (`mirror_head`). Passing `--pos-x/--pos-y` switches it to `enable_head`,
/// which drops the mirror and enables the head with this mode. That is the
/// only way to break a mirror without turning the screen off first.
pub fn break_args(name: &str, output: &OutputPlan) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::with_capacity(16);
    for value in [
        "mode".to_string(),
        name.to_string(),
        output.mode.width.to_string(),
        output.mode.height.to_string(),
    ] {
        args.push(value.into());
    }
    if output.mode.refresh_mhz > 0 {
        args.push("--refresh".into());
        args.push(format!("{:.3}", f64::from(output.mode.refresh_mhz) / 1000.0).into());
    }
    args.push("--pos-x".into());
    args.push(output.position.0.to_string().into());
    args.push("--pos-y".into());
    args.push(output.position.1.to_string().into());
    args.push("--scale".into());
    args.push(format_scale(output.scale).into());
    args.push("--transform".into());
    args.push(cosmic_transform(output.transform).into());
    args
}

fn cosmic_transform(transform: Transform) -> &'static str {
    match transform {
        Transform::Normal => "normal",
        Transform::Rotate90 => "rotate90",
        Transform::Rotate180 => "rotate180",
        Transform::Rotate270 => "rotate270",
        Transform::Flipped => "flipped",
        Transform::Flipped90 => "flipped90",
        Transform::Flipped180 => "flipped180",
        Transform::Flipped270 => "flipped270",
    }
}

/// Break one native mirror, keeping the head on when possible.
///
/// A synthesized (custom) mode is not advertised by the compositor, so
/// `cosmic-randr mode` cannot select it; turning the head off still drops the
/// mirror, and the caller's `wlr-randr` apply turns it back on with the custom
/// mode.
pub fn break_mirror(name: &str, output: &OutputPlan) -> Result<()> {
    let program = program_from_env(ENV_COSMIC_RANDR, "cosmic-randr");
    let result = util::run_command(&program, &break_args(name, output), TIMEOUT)
        .with_context(|| format!("running {} mode", program.display()))?;
    if result.success() {
        return Ok(());
    }
    let args: Vec<OsString> = ["disable".to_string(), name.to_string()]
        .into_iter()
        .map(OsString::from)
        .collect();
    let disabled = util::run_command(&program, &args, TIMEOUT)
        .with_context(|| format!("running {} disable", program.display()))?;
    if disabled.success() {
        Ok(())
    } else {
        bail!(
            "{} could not unmirror {name}: {}",
            program.display(),
            disabled.error_message()
        )
    }
}

/// (external, internal) when both heads are enabled at the same position.
///
/// That is what a mirror means in the layout model: COSMIC then needs an
/// explicit native mirror, or the two heads only share the pointer.
pub fn mirror_pair(plan: &LayoutPlan) -> Option<(String, String)> {
    let mut internal = None;
    let mut external = None;
    for (name, output) in &plan.outputs {
        if !output.enabled {
            continue;
        }
        if is_internal_name(name) {
            internal.get_or_insert_with(|| (name.clone(), output.position));
        } else {
            external.get_or_insert_with(|| (name.clone(), output.position));
        }
    }
    let (internal, internal_position) = internal?;
    let (external, external_position) = external?;
    (internal_position == external_position).then_some((external, internal))
}

/// Re-create the mirrors recorded in a transaction snapshot, for heads the
/// restored plan keeps enabled.
pub fn restore_mirrors(mirrored: &[(String, String)], plan: &LayoutPlan) {
    if mirrored.is_empty() || !native_mirror_supported() {
        return;
    }
    for (output, from) in mirrored {
        let enabled = |name: &str| plan.outputs.get(name).is_some_and(|output| output.enabled);
        if enabled(output) && enabled(from) {
            let _ = mirror(output, from);
        }
    }
}

fn exec_command(program: &Path, args: &[OsString]) -> String {
    let program = program.display().to_string();
    let program = if program.contains(char::is_whitespace) {
        format!("'{program}'")
    } else {
        program
    };
    let mut parts = vec![program];
    parts.extend(args.iter().map(|arg| shell_quote(&arg.to_string_lossy())));
    parts.join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.contains(|ch: char| ch.is_whitespace() || ch == '\'' || ch == '"') {
        format!("'{}'", value.replace('\'', "'\\''"))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_setting_wins_over_the_session() {
        std::env::set_var(ENV_NATIVE_MIRROR, "1");
        assert!(native_mirror_supported());
        std::env::set_var(ENV_NATIVE_MIRROR, "0");
        assert!(!native_mirror_supported());
        std::env::remove_var(ENV_NATIVE_MIRROR);
    }

    #[test]
    fn program_lookup_handles_paths_and_names() {
        assert!(program_exists(Path::new("/bin/sh")));
        assert!(!program_exists(Path::new("/nonexistent/cosmic-randr")));
        assert!(program_exists(Path::new("sh")));
    }

    #[test]
    fn mirror_map_ignores_colour_codes() {
        // Verbatim shape of `cosmic-randr list`, which colours the head names
        // and the "(mirroring …)" state.
        let list = "\u{1b}[1meDP-1\u{1b}[0m \u{1b}[1;32m(enabled)\u{1b}[0m\n\
                    \u{1b}[1mHDMI-A-1\u{1b}[0m \u{1b}[1;34m(mirroring \"eDP-1\")\u{1b}[0m\n\
                    DP-2 \u{1b}[1;34m(mirroring \"HDMI-A-1\")\u{1b}[0m\n\
                    DP-3 (disabled)\n";
        let mirrors = parse_mirrors(list);
        assert_eq!(mirrors["HDMI-A-1"], "eDP-1");
        assert_eq!(mirrors["DP-2"], "HDMI-A-1");
        assert!(!mirrors.contains_key("eDP-1"));
        assert!(!mirrors.contains_key("DP-3"));
        assert!(parse_mirrors("").is_empty());
    }

    #[test]
    fn break_args_ask_for_an_independent_head() {
        let plan = OutputPlan {
            enabled: true,
            mode: crate::model::output::Mode::new(1920, 1200, 59_950),
            position: (0, 0),
            scale: 2.25,
            transform: Transform::Rotate90,
            adaptive_sync: false,
        };
        let args: Vec<String> = break_args("HDMI-A-1", &plan)
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "mode",
                "HDMI-A-1",
                "1920",
                "1200",
                "--refresh",
                "59.950",
                "--pos-x",
                "0",
                "--pos-y",
                "0",
                "--scale",
                "2.25",
                "--transform",
                "rotate90",
            ]
        );
    }

    #[test]
    fn mirror_pair_needs_two_enabled_heads_at_one_position() {
        let mut outputs = BTreeMap::new();
        let output = |position: (i32, i32), enabled: bool| OutputPlan {
            enabled,
            mode: crate::model::output::Mode::new(1920, 1200, 60_000),
            position,
            scale: 1.0,
            transform: Transform::Normal,
            adaptive_sync: false,
        };
        outputs.insert("eDP-1".to_string(), output((0, 0), true));
        outputs.insert("HDMI-A-1".to_string(), output((0, 0), true));
        let plan = LayoutPlan {
            kind: crate::model::layout::LayoutKind::Mirror,
            outputs,
        };
        assert_eq!(
            mirror_pair(&plan),
            Some(("HDMI-A-1".to_string(), "eDP-1".to_string()))
        );

        let mut overlapping = plan.clone();
        overlapping.outputs.get_mut("HDMI-A-1").unwrap().position = (1920, 0);
        assert_eq!(mirror_pair(&overlapping), None);

        let mut disabled = plan;
        disabled.outputs.get_mut("eDP-1").unwrap().enabled = false;
        assert_eq!(mirror_pair(&disabled), None);
    }
}
