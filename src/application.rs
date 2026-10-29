//! AdwApplication setup, CSS and entry point for the GUI.

use gtk4 as gtk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

use crate::config::{Paths, APP_ID};
use crate::session::Session;
use crate::ui::window;

pub fn run(paths: Paths, verbose: bool) -> glib::ExitCode {
    let application = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::empty())
        .build();

    application.connect_startup(|application| {
        application.set_accels_for_action("app.quit", &["<primary>q"]);
        load_css();
    });

    // The built window owns the `Rc<Widgets>` behind every weak callback.
    // Keeping it in the application (instead of a local in `activate`) is what
    // makes the periodic refresh and the buttons keep working.
    let built_window: Rc<RefCell<Option<window::AppWindow>>> = Rc::new(RefCell::new(None));
    application.connect_activate(move |application| {
        if let Some(existing) = built_window.borrow().as_ref() {
            existing.window.present();
            return;
        }
        if let Some(app_window) = activate(application, &paths, verbose) {
            app_window.window.present();
            *built_window.borrow_mut() = Some(app_window);
        }
    });

    // GTK parses the process argv again during `run()`. Hand it a clean
    // command line so app options such as `--config` are not rejected by
    // GOption.
    application.run_with_args(&["kanshi-gtk4"])
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../data/style.css"));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn activate(
    application: &adw::Application,
    paths: &Paths,
    verbose: bool,
) -> Option<window::AppWindow> {
    if let Some(window) = application.active_window() {
        window.present();
        return None;
    }
    install_quit_action(application);

    let session = match Session::load(paths.clone()) {
        Ok(session) => session,
        Err(err) => {
            let window = window::build_unavailable(
                application,
                "Could not read the display configuration",
                &format!(
                    "{err:#}\n\nThis app speaks zwlr_output_manager_v1 directly. Check that \
                     the compositor supports it (labwc, sway, niri and Hyprland do; GNOME \
                     and KDE do not)."
                ),
            );
            window.present();
            return None;
        }
    };

    if !session.backend_info.supported {
        let reason = session
            .backend_info
            .reason
            .clone()
            .unwrap_or_else(|| "Output management is not available.".to_string());
        let window = window::build_unavailable(
            application,
            "Output management is not available",
            &format!(
                "{reason}\n\nThis app needs a Wayland compositor that implements \
                 zwlr_output_manager_v1, the output-management protocol from wlroots \
                 (labwc, sway, niri and Hyprland provide it). GNOME and KDE do not, \
                 so outputs cannot be changed there."
            ),
        );
        window.present();
        return None;
    }

    if verbose {
        eprintln!(
            "config: {} · state: {} · backend: {}",
            paths.config_path.display(),
            paths.state_path.display(),
            crate::backend::selected_name()
        );
    }
    Some(window::build(application, session))
}

fn install_quit_action(application: &adw::Application) {
    if application.lookup_action("quit").is_some() {
        return;
    }
    let quit = gio::SimpleAction::new("quit", None);
    let app = application.clone();
    quit.connect_activate(move |_, _| app.quit());
    application.add_action(&quit);
}
