//! Preset card mini diagrams.

use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;

use crate::model::layout::LayoutKind;

/// Build the four layout toggle cards. Returns them in PRESETS order.
pub fn build(
    on_select: impl Fn(LayoutKind) + 'static,
) -> (gtk::Box, Vec<(LayoutKind, gtk::ToggleButton)>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_homogeneous(true);
    row.set_halign(gtk::Align::Fill);

    let on_select = std::rc::Rc::new(on_select);
    let mut cards = Vec::new();
    let mut group: Option<gtk::ToggleButton> = None;

    for kind in LayoutKind::PRESETS {
        let area = gtk::DrawingArea::builder()
            .content_width(76)
            .content_height(44)
            .build();
        area.add_css_class("preset-diagram");
        area.set_draw_func(move |_, cr, width, height| {
            draw_diagram(cr, f64::from(width), f64::from(height), kind);
        });

        let label = gtk::Label::new(Some(kind.label()));
        label.add_css_class("caption");
        label.set_wrap(true);
        label.set_justify(gtk::Justification::Center);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 5);
        content.set_halign(gtk::Align::Center);
        content.append(&area);
        content.append(&label);

        let button = gtk::ToggleButton::new();
        button.set_child(Some(&content));
        button.set_hexpand(true);
        button.add_css_class("preset-card");
        button.set_tooltip_text(Some(kind.description()));
        if let Some(group) = &group {
            button.set_group(Some(group));
        } else {
            group = Some(button.clone());
        }

        let on_select = on_select.clone();
        button.connect_toggled(move |button| {
            if button.is_active() {
                on_select(kind);
            }
        });

        row.append(&button);
        cards.push((kind, button));
    }

    (row, cards)
}

fn draw_diagram(cr: &gtk::cairo::Context, width: f64, height: f64, kind: LayoutKind) {
    let padding = 8.0;
    let available_w = width - padding * 2.0;
    let available_h = height - padding * 2.0;
    let accent = (0.208, 0.518, 0.894);
    let secondary = (0.937, 0.494, 0.161);

    let rect = |x: f64, y: f64, w: f64, h: f64, color: (f64, f64, f64), fill: bool| {
        rounded(cr, x, y, w, h, 4.0);
        if fill {
            cr.set_source_rgba(color.0, color.1, color.2, 0.28);
            let _ = cr.fill_preserve();
        }
        cr.set_source_rgba(color.0, color.1, color.2, 0.95);
        cr.set_line_width(1.5);
        let _ = cr.stroke();
    };

    match kind {
        LayoutKind::Single => {
            let w = available_w * 0.56;
            let h = available_h * 0.78;
            rect(
                padding + (available_w - w) / 2.0,
                padding + (available_h - h) / 2.0,
                w,
                h,
                accent,
                true,
            );
        }
        LayoutKind::Extend => {
            let w1 = available_w * 0.58;
            let w2 = available_w * 0.34;
            let h = available_h * 0.78;
            let y = padding + (available_h - h) / 2.0;
            rect(padding, y, w1, h, accent, true);
            rect(padding + w1 + 3.0, y, w2, h, secondary, true);
        }
        LayoutKind::Mirror => {
            let w = available_w * 0.56;
            let h = available_h * 0.82;
            let x = padding + (available_w - w) / 2.0;
            let y = padding + (available_h - h) / 2.0;
            rect(x, y, w, h, accent, true);
            rect(x + 5.0, y + 4.0, w, h, secondary, false);
        }
        LayoutKind::ExternalOnly => {
            let w = available_w * 0.52;
            let h = available_h * 0.78;
            rect(
                padding + (available_w - w) / 2.0,
                padding + (available_h - h) / 2.0,
                w,
                h,
                secondary,
                true,
            );
        }
        LayoutKind::Custom => {}
    }
}

fn rounded(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let radius = radius.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(
        x + w - radius,
        y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        0.0,
    );
    cr.arc(
        x + w - radius,
        y + h - radius,
        radius,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x + radius,
        y + h - radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    cr.close_path();
}

/// Status page for unsupported compositors or missing dependencies.
pub fn status_page(title: &str, description: &str) -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .title(title)
        .description(description)
        .icon_name("video-display-symbolic")
        .build();
    page.set_hexpand(true);
    page.set_vexpand(true);
    page
}
