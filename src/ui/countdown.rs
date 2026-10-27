//! Modal 10-second keep/revert countdown dialog.

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Handle to a presented countdown, used to dismiss it when the transaction
/// is aborted externally (for example a monitor is unplugged).
#[derive(Clone)]
pub struct CountdownHandle {
    dismiss: Rc<dyn Fn()>,
}

impl CountdownHandle {
    /// Close the dialog without running either callback.
    pub fn close(&self) {
        (self.dismiss)();
    }
}

/// Present the countdown. Closing the dialog, pressing Esc or letting the
/// timer run out calls `on_revert`; Enter or the Keep button calls `on_keep`.
pub fn present<F, G>(
    parent: &impl IsA<gtk::Window>,
    seconds: u32,
    note: Option<String>,
    on_keep: F,
    on_revert: G,
) -> CountdownHandle
where
    F: Fn() + 'static,
    G: Fn() + 'static,
{
    let seconds = seconds.max(1);
    let dialog = adw::Window::builder()
        .modal(true)
        .transient_for(parent)
        .resizable(false)
        .default_width(420)
        .default_height(300)
        .build();
    dialog.set_title(Some("Keep this display layout?"));
    dialog.add_css_class("countdown-dialog");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(28);
    content.set_margin_bottom(24);
    content.set_margin_start(28);
    content.set_margin_end(28);
    content.set_halign(gtk::Align::Center);

    let title = gtk::Label::new(Some("Keep this display layout?"));
    title.add_css_class("title-1");
    content.append(&title);

    let number = gtk::Label::new(Some(&seconds.to_string()));
    number.add_css_class("countdown-number");
    content.append(&number);

    let hint = gtk::Label::new(Some("Reverting automatically…"));
    hint.add_css_class("dim-label");
    content.append(&hint);

    if let Some(note) = note {
        let warning = gtk::Label::new(Some(&note));
        warning.add_css_class("caption");
        warning.add_css_class("dim-label");
        warning.set_wrap(true);
        warning.set_justify(gtk::Justification::Center);
        warning.set_max_width_chars(42);
        content.append(&warning);
    }

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    content.append(&spacer);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    buttons.set_halign(gtk::Align::Center);
    let revert_button = gtk::Button::with_label("Revert");
    revert_button.add_css_class("destructive-action");
    let keep_button = gtk::Button::with_label("Keep");
    keep_button.add_css_class("suggested-action");
    buttons.append(&revert_button);
    buttons.append(&keep_button);
    content.append(&buttons);

    dialog.set_content(Some(&content));

    let finished = Rc::new(Cell::new(false));
    let remaining = Rc::new(Cell::new(seconds));
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    let on_keep: Rc<dyn Fn()> = Rc::new(on_keep);
    let on_revert: Rc<dyn Fn()> = Rc::new(on_revert);

    let dismiss: Rc<dyn Fn() -> bool> = {
        let finished = finished.clone();
        let timer = timer.clone();
        let dialog = dialog.clone();
        Rc::new(move || {
            if finished.replace(true) {
                return false;
            }
            if let Some(id) = timer.borrow_mut().take() {
                id.remove();
            }
            dialog.close();
            true
        })
    };
    let make_finish = |callback: Rc<dyn Fn()>| {
        let dismiss = dismiss.clone();
        Rc::new(move || {
            if dismiss() {
                callback();
            }
        }) as Rc<dyn Fn()>
    };
    let finish_keep = make_finish(on_keep);
    let finish_revert = make_finish(on_revert);

    {
        let finish_keep = finish_keep.clone();
        keep_button.connect_clicked(move |_| finish_keep());
    }
    {
        let finish_revert = finish_revert.clone();
        revert_button.connect_clicked(move |_| finish_revert());
    }
    {
        let finish_revert = finish_revert.clone();
        dialog.connect_close_request(move |_| {
            finish_revert();
            glib::Propagation::Proceed
        });
    }
    {
        let finish_keep = finish_keep.clone();
        let finish_revert = finish_revert.clone();
        let controller = gtk::EventControllerKey::new();
        controller.connect_key_pressed(move |_, key, _, _| match key {
            gtk::gdk::Key::Escape => {
                finish_revert();
                glib::Propagation::Stop
            }
            gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                finish_keep();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        dialog.add_controller(controller);
    }

    number.set_text(&remaining.get().to_string());
    {
        let finish_revert = finish_revert.clone();
        let remaining = remaining.clone();
        let number = number.clone();
        let id = glib::timeout_add_seconds_local(1, move || {
            let value = remaining.get().saturating_sub(1);
            remaining.set(value);
            number.set_text(&value.to_string());
            if value == 0 {
                finish_revert();
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
        *timer.borrow_mut() = Some(id);
    }

    dialog.present();
    CountdownHandle {
        dismiss: Rc::new({
            let dismiss = dismiss.clone();
            move || {
                dismiss();
            }
        }),
    }
}
