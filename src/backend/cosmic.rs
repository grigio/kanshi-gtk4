//! Native mirroring on COSMIC.
//!
//! COSMIC keeps one workspace set per output, so two heads placed at the same
//! position do not show the same thing: the pointer is shared because it lives
//! in the global space, but each head renders its own workspaces, so windows
//! stay on the head that owns them. A real mirror is a compositor feature
//! (`zcosmic_output_configuration_v1`), reachable from the command line with
//! `cosmic-randr mirror <output> <from>`.
//!
//! `wlr-randr` cannot express it, and any later position change drops the
//! mirror again, so the mirror has to be re-created after every apply.

use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

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
    let program = program.display().to_string();
    // kanshi runs `exec` through a shell, so a path with spaces needs quoting.
    if program.contains(char::is_whitespace) {
        format!("'{program}' mirror {output} {from}")
    } else {
        format!("{program} mirror {output} {from}")
    }
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
}
