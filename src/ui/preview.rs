//! Scale preview of the planned layout, drawn with cairo.
//!
//! The same [`Geometry`] mapping is used for painting and for drag-to-move
//! hit testing, so what you see is exactly what you can grab.

use gtk4 as gtk;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

use crate::model::layout::{format_scale, LayoutPlan};

#[derive(Clone)]
pub struct Preview {
    pub area: gtk::DrawingArea,
    state: Rc<RefCell<Option<LayoutPlan>>>,
}

#[allow(clippy::new_without_default)]
impl Preview {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(150)
            .hexpand(true)
            .vexpand(false)
            .build();
        area.add_css_class("preview-canvas");
        let state = Rc::new(RefCell::new(None));
        let draw_state = state.clone();
        area.set_draw_func(move |area, cr, width, height| {
            draw(area, cr, width, height, draw_state.borrow().as_ref());
        });
        Self { area, state }
    }

    pub fn set_plan(&self, plan: &LayoutPlan) {
        *self.state.borrow_mut() = Some(plan.clone());
        self.area.queue_draw();
    }
}

pub struct Rect {
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub index: usize,
}

/// Maps logical layout coordinates into drawing-area pixels.
pub struct Geometry {
    pub rects: Vec<Rect>,
    pub scale: f64,
    off_x: f64,
    off_y: f64,
    min_x: f64,
    min_y: f64,
}

impl Geometry {
    pub fn new(width: f64, height: f64, plan: &LayoutPlan) -> Option<Self> {
        let mut rects: Vec<Rect> = Vec::new();
        for (index, (name, output)) in plan
            .outputs
            .iter()
            .filter(|(_, output)| output.enabled)
            .enumerate()
        {
            let (w, h) = output.logical_size();
            if w <= 0 || h <= 0 {
                continue;
            }
            rects.push(Rect {
                name: name.clone(),
                x: f64::from(output.position.0),
                y: f64::from(output.position.1),
                w: f64::from(w),
                h: f64::from(h),
                index,
            });
        }
        if rects.is_empty() {
            return None;
        }

        let min_x = rects.iter().map(|r| r.x).fold(f64::INFINITY, f64::min);
        let min_y = rects.iter().map(|r| r.y).fold(f64::INFINITY, f64::min);
        let max_x = rects
            .iter()
            .map(|r| r.x + r.w)
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = rects
            .iter()
            .map(|r| r.y + r.h)
            .fold(f64::NEG_INFINITY, f64::max);
        let span_x = (max_x - min_x).max(1.0);
        let span_y = (max_y - min_y).max(1.0);

        let padding = 24.0;
        let scale = ((width - padding * 2.0) / span_x)
            .min((height - padding * 2.0) / span_y)
            .max(0.05);
        let used_w = span_x * scale;
        let used_h = span_y * scale;
        let off_x = (width - used_w) / 2.0;
        let off_y = (height - used_h) / 2.0;

        Some(Self {
            rects,
            scale,
            off_x,
            off_y,
            min_x,
            min_y,
        })
    }

    pub fn screen_rect(&self, rect: &Rect) -> (f64, f64, f64, f64) {
        (
            self.off_x + (rect.x - self.min_x) * self.scale,
            self.off_y + (rect.y - self.min_y) * self.scale,
            rect.w * self.scale,
            rect.h * self.scale,
        )
    }

    /// Topmost output under a pointer position, if any.
    pub fn hit(&self, x: f64, y: f64) -> Option<&Rect> {
        self.rects.iter().rev().find(|rect| {
            let (rx, ry, rw, rh) = self.screen_rect(rect);
            x >= rx && x <= rx + rw && y >= ry && y <= ry + rh
        })
    }

    /// Convert a pointer delta in pixels into logical layout units.
    pub fn logical_delta(&self, dx: f64, dy: f64) -> (i32, i32) {
        (
            (dx / self.scale).round() as i32,
            (dy / self.scale).round() as i32,
        )
    }
}

/// Position of a `size` rectangle resting flush against the closest side of
/// `anchor`, aligned to the nearest of its start, centre and end on the other
/// axis.
///
/// `anchor` is `(x, y, width, height)` in logical layout units. The side is
/// chosen from the relative position of the two centres, so dragging the
/// external display past a corner switches it to the neighbouring side.
pub fn nearest_side_position(
    position: (i32, i32),
    size: (i32, i32),
    anchor: (i32, i32, i32, i32),
) -> (i32, i32) {
    let (px, py) = position;
    let (w, h) = size;
    let (ax, ay, aw, ah) = anchor;

    let aligned_x = nearest_alignment(px, w, ax, aw);
    let aligned_y = nearest_alignment(py, h, ay, ah);
    let dx = (px + w / 2) - (ax + aw / 2);
    let dy = (py + h / 2) - (ay + ah / 2);

    if dx.abs() >= dy.abs() {
        let x = if dx >= 0 { ax + aw } else { ax - w };
        (x, aligned_y)
    } else {
        let y = if dy >= 0 { ay + ah } else { ay - h };
        (aligned_x, y)
    }
}

/// `nearest_side_position` applied only when the pointer is within `threshold`
/// logical units of the flush position, which makes the preview feel magnetic
/// without taking the display away from the pointer.
pub fn magnetic_position(
    position: (i32, i32),
    size: (i32, i32),
    anchor: (i32, i32, i32, i32),
    threshold: i32,
) -> (i32, i32) {
    let snapped = nearest_side_position(position, size, anchor);
    let close =
        (position.0 - snapped.0).abs() <= threshold && (position.1 - snapped.1).abs() <= threshold;
    if close {
        snapped
    } else {
        position
    }
}

fn nearest_alignment(start: i32, length: i32, anchor_start: i32, anchor_length: i32) -> i32 {
    [
        anchor_start,
        anchor_start + (anchor_length - length) / 2,
        anchor_start + anchor_length - length,
    ]
    .into_iter()
    .min_by_key(|candidate| (start - candidate).abs())
    .unwrap_or(anchor_start)
}

fn draw(
    area: &gtk::DrawingArea,
    cr: &gtk::cairo::Context,
    width: i32,
    height: i32,
    plan: Option<&LayoutPlan>,
) {
    let (width, height) = (f64::from(width), f64::from(height));
    let fg = area.color();
    cr.set_source_rgb(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
    );

    let Some(plan) = plan else {
        draw_centered(cr, width, height, "No layout to preview", 14.0);
        return;
    };
    let Some(geometry) = Geometry::new(width, height, plan) else {
        draw_centered(cr, width, height, "All displays are turned off", 14.0);
        return;
    };

    let palette = [
        (0.208, 0.518, 0.894), // accent blue
        (0.937, 0.494, 0.161), // orange
        (0.184, 0.686, 0.431), // green
    ];

    for rect in &geometry.rects {
        let (x, y, w, h) = geometry.screen_rect(rect);
        let (r, g, b) = palette[rect.index % palette.len()];
        rounded_rect(cr, x, y, w, h, 7.0);
        cr.set_source_rgba(r, g, b, 0.20);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(r, g, b, 0.9);
        cr.set_line_width(1.6);
        let _ = cr.stroke();

        // Labels.
        let font = (w / 9.0).min(h / 4.0).clamp(10.0, 15.0);
        cr.set_source_rgba(r, g, b, 1.0);
        cr.select_font_face(
            "Sans",
            gtk::cairo::FontSlant::Normal,
            gtk::cairo::FontWeight::Bold,
        );
        cr.set_font_size(font);
        let name_ext = cr
            .text_extents(&rect.name)
            .map(|e| e.width())
            .unwrap_or(0.0);
        if name_ext < w - 8.0 {
            if let Ok(ext) = cr.text_extents(&rect.name) {
                cr.move_to(x + (w - ext.width()) / 2.0, y + h / 2.0 - 2.0);
                let _ = cr.show_text(&rect.name);
            }
        } else {
            // Long name: place at the top-left corner.
            cr.move_to(x + 5.0, y + font + 2.0);
            let _ = cr.show_text(&rect.name);
        }

        // Mode / logical size under the name when there is room.
        if h > 48.0 {
            let plan = plan.outputs.get(&rect.name);
            if let Some(output) = plan {
                let text = format!(
                    "{}×{} · {}",
                    output.mode.width,
                    output.mode.height,
                    format_scale(output.scale)
                );
                cr.select_font_face(
                    "Sans",
                    gtk::cairo::FontSlant::Normal,
                    gtk::cairo::FontWeight::Normal,
                );
                cr.set_font_size((font - 2.0).max(9.0));
                cr.set_source_rgba(r, g, b, 0.85);
                if let Ok(ext) = cr.text_extents(&text) {
                    if ext.width() < w - 8.0 {
                        cr.move_to(x + (w - ext.width()) / 2.0, y + h / 2.0 + font);
                        let _ = cr.show_text(&text);
                    }
                }
            }
        }
    }

    // Scale hint in the corner.
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    cr.set_font_size(10.0);
    cr.set_source_rgba(
        f64::from(fg.red()),
        f64::from(fg.green()),
        f64::from(fg.blue()),
        0.55,
    );
    cr.move_to(10.0, height - 10.0);
    let _ = cr.show_text("drag to move · logical coordinates");
}

fn draw_centered(cr: &gtk::cairo::Context, width: f64, height: f64, text: &str, size: f64) {
    cr.select_font_face(
        "Sans",
        gtk::cairo::FontSlant::Normal,
        gtk::cairo::FontWeight::Normal,
    );
    cr.set_font_size(size);
    if let Ok(ext) = cr.text_extents(text) {
        cr.move_to((width - ext.width()) / 2.0, height / 2.0);
        let _ = cr.show_text(text);
    }
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::{LayoutKind, OutputPlan};
    use crate::model::output::{Mode, Transform};
    use std::collections::BTreeMap;

    fn plan() -> LayoutPlan {
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "eDP-1".to_string(),
            OutputPlan {
                enabled: true,
                mode: Mode::new(100, 100, 60_000),
                position: (0, 0),
                scale: 1.0,
                transform: Transform::Normal,
                adaptive_sync: false,
            },
        );
        outputs.insert(
            "DP-1".to_string(),
            OutputPlan {
                enabled: true,
                mode: Mode::new(100, 100, 60_000),
                position: (100, 0),
                scale: 1.0,
                transform: Transform::Normal,
                adaptive_sync: false,
            },
        );
        LayoutPlan {
            kind: LayoutKind::Extend,
            outputs,
        }
    }

    #[test]
    fn geometry_hit_tests_both_displays() {
        let geometry = Geometry::new(400.0, 200.0, &plan()).expect("geometry");
        assert!(geometry.scale > 0.0);
        assert_eq!(
            geometry.hit(60.0, 100.0).map(|r| r.name.as_str()),
            Some("eDP-1")
        );
        assert_eq!(
            geometry.hit(250.0, 100.0).map(|r| r.name.as_str()),
            Some("DP-1")
        );
        assert!(geometry.hit(5.0, 5.0).is_none());
    }

    #[test]
    fn logical_delta_uses_the_preview_scale() {
        let geometry = Geometry::new(400.0, 200.0, &plan()).expect("geometry");
        // The two 100-wide outputs plus padding map to 352 px.
        assert!((geometry.scale - 1.52).abs() < 0.001);
        assert_eq!(geometry.logical_delta(15.2, -30.4), (10, -20));
    }

    #[test]
    fn disabled_outputs_are_not_drawn_or_hit() {
        let mut plan = plan();
        plan.outputs.get_mut("DP-1").unwrap().enabled = false;
        let geometry = Geometry::new(400.0, 200.0, &plan).expect("geometry");
        assert_eq!(geometry.rects.len(), 1);
        assert!(geometry.rects.iter().all(|rect| rect.name != "DP-1"));
        assert_eq!(
            geometry.hit(200.0, 100.0).map(|rect| rect.name.as_str()),
            Some("eDP-1")
        );
    }

    /// The laptop panel is 100×100 at the origin.
    const ANCHOR: (i32, i32, i32, i32) = (0, 0, 100, 100);
    const SIZE: (i32, i32) = (80, 60);

    #[test]
    fn drag_snaps_to_the_nearest_side() {
        // Past the right edge, vertically aligned with the panel's top.
        assert_eq!(nearest_side_position((120, 10), SIZE, ANCHOR), (100, 0));
        // Past the left edge.
        assert_eq!(nearest_side_position((-40, 10), SIZE, ANCHOR), (-80, 0));
        // Above.
        assert_eq!(nearest_side_position((10, -40), SIZE, ANCHOR), (10, -60));
        // Below.
        assert_eq!(nearest_side_position((10, 130), SIZE, ANCHOR), (10, 100));
    }

    #[test]
    fn snap_aligns_to_the_nearest_edge_or_centre() {
        // Close to the bottom of the panel: bottom-aligned.
        assert_eq!(
            nearest_side_position((120, 45), SIZE, ANCHOR),
            (100, 100 - 60)
        );
        // Roughly centred: centre-aligned.
        assert_eq!(
            nearest_side_position((120, 20), SIZE, ANCHOR),
            (100, (100 - 60) / 2)
        );
        // Dragging below past the corner switches to the vertical side.
        assert_eq!(nearest_side_position((120, 200), SIZE, ANCHOR), (20, 100));
    }

    #[test]
    fn magnet_only_moves_a_nearby_display() {
        // 6 units away from the flush position: pulled in.
        assert_eq!(magnetic_position((106, 4), SIZE, ANCHOR, 12), (100, 0));
        // Far away: left where the pointer put it.
        assert_eq!(magnetic_position((400, 300), SIZE, ANCHOR, 12), (400, 300));
    }
}
