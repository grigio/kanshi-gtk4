//! kanshi-gtk4 — arrange displays and persist the layout as a kanshi profile.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

use kanshi_gtk4::application;
use kanshi_gtk4::config::Paths;
use kanshi_gtk4::kanshi::config::{generate_managed_block, merge_managed_block, CurrentLayout};
use kanshi_gtk4::session::Session;
use kanshi_gtk4::util;
use kanshi_gtk4::watchdog;

#[derive(Debug, Parser)]
#[command(
    name = "kanshi-gtk4",
    version,
    about = "Arrange displays and save the layout as a kanshi profile"
)]
struct Args {
    /// Print the generated kanshi config for the current outputs and exit.
    #[arg(long)]
    print_config: bool,

    /// Verbose diagnostics on stderr.
    #[arg(long)]
    verbose: bool,

    /// Override the kanshi config path (also KANSHI_GTK4_KANSHI_CONFIG).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Override the application state directory.
    #[arg(long, value_name = "PATH")]
    state_dir: Option<PathBuf>,

    /// Internal: rollback watchdog for a transaction directory.
    #[arg(long, value_name = "DIR", hide = true)]
    watchdog: Option<PathBuf>,

    /// Internal: watchdog timeout in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = watchdog::DEFAULT_TIMEOUT, hide = true)]
    timeout: u32,

    /// Internal: restore the snapshot in a transaction directory.
    #[arg(long, value_name = "DIR", hide = true)]
    revert: Option<PathBuf>,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();

    if let Some(dir) = &args.watchdog {
        let code = watchdog::run_watchdog(dir, args.timeout);
        return std::process::ExitCode::from(code.clamp(0, 255) as u8);
    }
    if let Some(dir) = &args.revert {
        return match watchdog::revert_cli(dir) {
            Ok(report) => {
                println!("Restored: {}", report.summary());
                std::process::ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("Could not restore {}: {err:#}", dir.display());
                std::process::ExitCode::FAILURE
            }
        };
    }

    let paths = Paths::discover(args.config, args.state_dir);

    if args.print_config {
        return match print_config(&paths, args.verbose) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("{err:#}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    let code = application::run(paths, args.verbose);
    std::process::ExitCode::from(u8::from(code))
}

fn print_config(paths: &Paths, verbose: bool) -> Result<()> {
    let session = Session::load(paths.clone())?;
    if !session.backend_info.supported {
        anyhow::bail!(
            "{}",
            session
                .backend_info
                .reason
                .unwrap_or_else(|| "output management is unavailable".into())
        );
    }
    let existing = util::read_optional(&paths.config_path)
        .with_context(|| format!("reading {}", paths.config_path.display()))?;
    let current = CurrentLayout::new(
        session.set_key.clone(),
        session.plan.clone(),
        session.outputs.clone(),
        session.desktop_scales.clone(),
    );
    let block = generate_managed_block(&session.state, Some(&current));
    let merged = merge_managed_block(existing.as_deref(), &block);
    if verbose {
        eprintln!("config path: {}", paths.config_path.display());
        eprintln!("output set: {}", session.set_key);
    }
    print!("{merged}");
    Ok(())
}
