//! Main window: presets, preview, per-output controls and apply flow.

use gtk4 as gtk;
use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;

use crate::apply::{self, ApplyParams, PreparedTx};
use crate::backend::wlr_randr::WlrRandrBackend;
use crate::backend::{BackendInfo, OutputBackend};
use crate::config::{self, AppState, Paths, APP_ID};
use crate::kanshi::ctl::{KanshiClient, KanshiInfo};
use crate::model::layout::{
    mirror_note, preset_available, set_key, validate, LayoutKind, LayoutPlan, OutputPlan,
};
use crate::model::output::Output;
use crate::session::Session;
use crate::ui::countdown::{self, CountdownHandle};
use crate::ui::layout_cards;
use crate::ui::output_row::{EditHandler, OutputEdit, OutputRow};
use crate::ui::preview::{self, Preview};
use crate::ui::spawn_background;
use crate::watchdog;

pub struct AppWindow {
    pub window: adw::ApplicationWindow,
    /// Keeps the widget handles (and therefore every weak callback) alive for
    /// as long as the window. Without this the periodic refresh and all
    /// buttons stop working once `build()` returns.
    _widgets: Rc<Widgets>,
}

pub struct AppModel {
    pub paths: Paths,
    pub backend_info: BackendInfo,
    pub kanshi_info: KanshiInfo,
    pub outputs: Vec<Output>,
    pub state: AppState,
    pub internal: Option<String>,
    pub external: Option<String>,
    pub set_key: String,
    pub desktop_scales: BTreeMap<String, f64>,
    pub kind: LayoutKind,
    pub plan: LayoutPlan,
    pub busy: bool,
    pub refreshing: bool,
    pub tx: Option<PreparedTx>,
    pub countdown: Option<CountdownHandle>,
    pub expanded: HashSet<String>,
    pub last_refresh_error: Option<String>,
}

impl AppModel {
    fn from_session(session: Session) -> Self {
        Self {
            paths: session.paths,
            backend_info: session.backend_info,
            kanshi_info: session.kanshi_info,
            outputs: session.outputs,
            state: session.state,
            internal: session.internal,
            external: session.external,
            set_key: session.set_key,
            desktop_scales: session.desktop_scales,
            kind: session.kind,
            plan: session.plan,
            busy: false,
            refreshing: false,
            tx: None,
            countdown: None,
            expanded: HashSet::new(),
            last_refresh_error: None,
        }
    }

    fn rebuild(&mut self) {
        self.plan.rebuild(
            self.internal.as_deref(),
            self.external.as_deref(),
            &self.desktop_scales,
        );
        self.kind = self.plan.kind;
    }

    /// Apply one edit coming from a display row.
    ///
    /// Returns whether anything changed. Widgets report their state back while
    /// they are being synced (a `GtkDropDown` emits `notify::selected` when its
    /// list is replaced), so a no-op edit must not rebuild the whole window:
    /// doing that re-splices the very list that raised the notification and the
    /// two ping-pong forever.
    fn apply_edit(&mut self, edit: OutputEdit) -> bool {
        let changed = match edit {
            OutputEdit::Expanded { name, expanded } => {
                if expanded {
                    self.expanded.insert(name)
                } else {
                    self.expanded.remove(&name)
                }
            }
            OutputEdit::Enabled { name, enabled } => match self.plan.outputs.get_mut(&name) {
                Some(output) if output.enabled != enabled => {
                    output.enabled = enabled;
                    self.plan.kind = LayoutKind::Custom;
                    true
                }
                _ => false,
            },
            OutputEdit::Mode { name, mode } => {
                let changed = match self.plan.outputs.get_mut(&name) {
                    Some(output) if output.mode != mode => {
                        output.mode = mode;
                        true
                    }
                    _ => false,
                };
                if changed {
                    self.rebuild();
                }
                changed
            }
            OutputEdit::Scale { name, scale } => {
                let mut changed = false;
                if let Some(output) = self.plan.outputs.get_mut(&name) {
                    if (output.scale - scale).abs() > f64::EPSILON {
                        output.scale = scale;
                        changed = true;
                    }
                }
                if self.desktop_scales.insert(name.clone(), scale) != Some(scale) {
                    changed = true;
                }
                if changed {
                    if self.plan.kind == LayoutKind::Mirror
                        && Some(name.as_str()) == self.external.as_deref()
                    {
                        // A manual external scale turns the computed mirror into
                        // a custom layout.
                        self.plan.kind = LayoutKind::Custom;
                    } else {
                        self.rebuild();
                    }
                }
                changed
            }
            OutputEdit::Transform { name, transform } => {
                let changed = match self.plan.outputs.get_mut(&name) {
                    Some(output) if output.transform != transform => {
                        output.transform = transform;
                        true
                    }
                    _ => false,
                };
                if changed {
                    self.rebuild();
                }
                changed
            }
            OutputEdit::AdaptiveSync { name, enabled } => match self.plan.outputs.get_mut(&name) {
                Some(output) if output.adaptive_sync != enabled => {
                    output.adaptive_sync = enabled;
                    true
                }
                _ => false,
            },
        };
        if self.kind != self.plan.kind {
            self.kind = self.plan.kind;
            return true;
        }
        changed
    }

    fn select_preset(&mut self, kind: LayoutKind) {
        let has_external = self
            .outputs
            .iter()
            .any(|o| Some(&o.name) != self.internal.as_ref());
        if !preset_available(
            kind,
            self.outputs.len(),
            self.internal.is_some(),
            has_external,
        ) {
            return;
        }
        self.plan.kind = kind;
        self.kind = kind;
        self.rebuild();
    }

    /// Move one output to a new logical position. The laptop panel is kept at
    /// `0,0` by the caller; any manual move turns the preset into `Custom`.
    /// Returns whether anything changed.
    fn move_output(&mut self, name: &str, position: (i32, i32)) -> bool {
        let Some(output) = self.plan.outputs.get_mut(name) else {
            return false;
        };
        if output.position == position {
            return false;
        }
        output.position = position;
        self.plan.kind = LayoutKind::Custom;
        self.kind = LayoutKind::Custom;
        true
    }

    fn reconcile_outputs(&mut self, new_outputs: Vec<Output>) {
        let old_key = self.set_key.clone();
        let mut plan_outputs = BTreeMap::new();
        for output in &new_outputs {
            let mut po = self
                .plan
                .outputs
                .get(&output.name)
                .cloned()
                .unwrap_or_else(|| OutputPlan::from_output(output));
            // Custom modes are not in the advertised list until they are
            // active, so only re-sync modes the compositor is supposed to
            // know about.
            if !po.mode.custom && output.find_mode(&po.mode).is_none() {
                po.mode = output.active_mode();
            }
            if !(0.5..=3.0).contains(&po.scale) || !po.scale.is_finite() {
                po.scale = output.scale;
            }
            if output.modes.is_empty() {
                po.enabled = false;
            }
            plan_outputs.insert(output.name.clone(), po);
        }
        self.plan.outputs = plan_outputs;
        self.outputs = new_outputs;

        self.internal = self
            .outputs
            .iter()
            .find(|o| o.is_internal())
            .map(|o| o.name.clone());
        self.external = self
            .outputs
            .iter()
            .find(|o| Some(&o.name) != self.internal.as_ref() && !o.is_internal())
            .map(|o| o.name.clone());
        let names: Vec<String> = self.outputs.iter().map(|o| o.name.clone()).collect();
        self.set_key = if names.is_empty() {
            String::new()
        } else {
            set_key(&names, self.internal.as_deref())
        };
        if self.set_key != old_key {
            for (name, scale) in self.state.remembered_desktop_scales(&self.set_key) {
                self.desktop_scales.insert(name, scale);
            }
        }
        self.desktop_scales
            .retain(|name, _| self.outputs.iter().any(|o| &o.name == name));
        for output in &self.outputs {
            self.desktop_scales
                .entry(output.name.clone())
                .or_insert(output.scale);
        }
        if self.plan.kind != LayoutKind::Custom {
            self.rebuild();
        } else {
            self.kind = LayoutKind::Custom;
        }
    }
}

pub struct Widgets {
    pub window: adw::ApplicationWindow,
    pub toast: adw::ToastOverlay,
    pub apply_button: gtk::Button,
    spinner: gtk::Spinner,
    presets_box: gtk::Box,
    presets: RefCell<Vec<(LayoutKind, gtk::ToggleButton)>>,
    presets_guard: Cell<bool>,
    preview: Preview,
    outputs_group: adw::PreferencesGroup,
    rows: RefCell<BTreeMap<String, OutputRow>>,
    row_names: RefCell<Vec<String>>,
    syncing: Cell<bool>,
    status_dot: gtk::Label,
    status_label: gtk::Label,
    engine_label: gtk::Label,
    mirror_label: gtk::Label,
    preview_hint: gtk::Label,
    validation_label: gtk::Label,
}

impl Widgets {
    fn sync(&self, model: &AppModel) {
        self.presets_guard.set(true);
        let has_internal = model.internal.is_some();
        let has_external = model
            .outputs
            .iter()
            .any(|o| Some(&o.name) != model.internal.as_ref());
        let output_count = model.outputs.len();
        for (kind, button) in self.presets.borrow().iter() {
            let available = preset_available(*kind, output_count, has_internal, has_external);
            button.set_sensitive(available);
            button.set_tooltip_text(Some(if available {
                kind.description()
            } else {
                match kind {
                    LayoutKind::Single => "No built-in display detected",
                    LayoutKind::ExternalOnly => "No external display connected",
                    _ => "Connect a second display",
                }
            }));
            button.set_active(available && model.kind == *kind);
        }
        self.presets_guard.set(false);

        self.preview.set_plan(&model.plan);
        self.preview_hint.set_visible(model.external.is_some());
        let rows = self.rows.borrow();
        for (name, output_plan) in &model.plan.outputs {
            let Some(row) = rows.get(name) else {
                continue;
            };
            let Some(output) = model.outputs.iter().find(|o| &o.name == name) else {
                continue;
            };
            let desktop_scale = model
                .desktop_scales
                .get(name)
                .copied()
                .unwrap_or(output_plan.scale);
            row.sync(
                output,
                Some(output_plan),
                model.expanded.contains(name),
                desktop_scale,
            );
        }

        let validation = validate(&model.plan, &model.outputs);
        let mut lines: Vec<String> = validation.errors.iter().map(|e| format!("✗ {e}")).collect();
        lines.extend(validation.warnings.iter().map(|w| format!("! {w}")));
        if model.outputs.is_empty() {
            lines.clear();
        }
        self.validation_label.remove_css_class("validation-error");
        self.validation_label.remove_css_class("validation-warning");
        if lines.is_empty() {
            self.validation_label.set_visible(false);
        } else {
            self.validation_label.set_text(&lines.join("\n"));
            self.validation_label.set_visible(true);
            if !validation.errors.is_empty() {
                self.validation_label.add_css_class("validation-error");
            } else {
                self.validation_label.add_css_class("validation-warning");
            }
        }

        if let Some(note) = mirror_note(
            &model.plan,
            model.internal.as_deref(),
            model.external.as_deref(),
        ) {
            self.mirror_label.set_text(&note);
            self.mirror_label.set_visible(true);
        } else {
            self.mirror_label.set_visible(false);
        }

        let changed = !model.plan.matches_live(&model.outputs) && !model.outputs.is_empty();
        self.apply_button
            .set_sensitive(!model.busy && model.tx.is_none() && changed && validation.is_valid());
        self.apply_button
            .set_tooltip_text(Some(if !validation.is_valid() {
                "Fix the highlighted problems first"
            } else if !changed {
                "No changes to apply"
            } else {
                "Apply this layout (Ctrl+Enter)"
            }));
        self.spinner.set_visible(model.busy);
        self.spinner.set_spinning(model.busy);

        for class in ["status-ok", "status-warn", "status-error"] {
            self.status_dot.remove_css_class(class);
        }
        let dot_class = if !model.backend_info.supported {
            "status-error"
        } else if model.outputs.is_empty() || !model.kanshi_info.running {
            "status-warn"
        } else {
            "status-ok"
        };
        self.status_dot.add_css_class(dot_class);

        let mut status = if model.outputs.is_empty() {
            "No displays detected. Connect a monitor and refresh.".to_string()
        } else {
            model.kanshi_info.status_text()
        };
        if model.tx.is_some() {
            status.push_str(" · waiting for confirmation");
        }
        self.status_label.set_text(&status);
        let protocol = model
            .backend_info
            .protocol_version
            .map(|version| format!(" · output protocol v{version}"))
            .unwrap_or_default();
        self.engine_label.set_text(&format!(
            "Applying through {}{protocol}",
            model.kanshi_info.engine_label()
        ));
    }
}

pub fn build(application: &adw::Application, session: Session) -> AppWindow {
    let model = Rc::new(RefCell::new(AppModel::from_session(session)));

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Displays")
        .default_width(680)
        .default_height(700)
        .build();

    let header = adw::HeaderBar::new();
    let window_title = adw::WindowTitle::new("Displays", "Arrange and save the layout");
    header.set_title_widget(Some(&window_title));

    let spinner = gtk::Spinner::new();
    spinner.set_visible(false);
    header.pack_start(&spinner);

    let about_button = gtk::Button::from_icon_name("help-about-symbolic");
    about_button.set_tooltip_text(Some("About Displays"));
    header.pack_end(&about_button);

    let apply_button = gtk::Button::with_label("Apply");
    apply_button.add_css_class("suggested-action");
    apply_button.set_tooltip_text(Some("Apply this layout (Ctrl+Enter)"));
    header.pack_end(&apply_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(14);
    content.set_margin_bottom(18);
    content.set_margin_start(14);
    content.set_margin_end(14);

    let layout_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    layout_section.append(&section_label("Layout"));
    let presets_box = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    presets_box.set_homogeneous(true);
    layout_section.append(&presets_box);
    content.append(&layout_section);

    let preview_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    preview_section.append(&section_label("Preview"));
    let preview = Preview::new();
    let preview_frame = gtk::Frame::new(None);
    preview_frame.add_css_class("card");
    preview_frame.set_child(Some(&preview.area));
    preview_section.append(&preview_frame);
    let mirror_label = gtk::Label::new(None);
    mirror_label.set_xalign(0.0);
    mirror_label.set_wrap(true);
    mirror_label.add_css_class("dim-label");
    mirror_label.set_visible(false);
    preview_section.append(&mirror_label);
    let preview_hint = gtk::Label::new(Some(
        "Drag the external display; it snaps to the closest edge.",
    ));
    preview_hint.set_xalign(0.0);
    preview_hint.add_css_class("caption");
    preview_hint.add_css_class("dim-label");
    preview_section.append(&preview_hint);
    content.append(&preview_section);

    let outputs_group = adw::PreferencesGroup::new();
    outputs_group.set_title("Displays");
    outputs_group.set_description(Some(
        "Click a display to change its resolution, refresh rate, scale and rotation.",
    ));
    content.append(&outputs_group);

    let validation_label = gtk::Label::new(None);
    validation_label.set_xalign(0.0);
    validation_label.set_wrap(true);
    validation_label.set_visible(false);
    content.append(&validation_label);

    let status_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    content.append(&status_separator);

    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_bar.add_css_class("status-bar");
    let status_dot = gtk::Label::new(Some("●"));
    status_dot.add_css_class("status-dot");
    status_bar.append(&status_dot);
    let status_label = gtk::Label::new(None);
    status_label.set_xalign(0.0);
    status_label.set_hexpand(true);
    status_label.set_wrap(true);
    status_label.add_css_class("caption");
    status_bar.append(&status_label);
    let engine_label = gtk::Label::new(None);
    engine_label.add_css_class("caption");
    engine_label.add_css_class("dim-label");
    status_bar.append(&engine_label);
    content.append(&status_bar);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(760);
    clamp.set_child(Some(&content));
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_child(Some(&clamp));
    scroller.set_vexpand(true);
    toolbar.set_content(Some(&scroller));

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&toolbar));
    window.set_content(Some(&toast));

    let widgets = Rc::new(Widgets {
        window: window.clone(),
        toast,
        apply_button: apply_button.clone(),
        spinner,
        presets_box,
        presets: RefCell::new(Vec::new()),
        presets_guard: Cell::new(false),
        preview,
        outputs_group,
        rows: RefCell::new(BTreeMap::new()),
        row_names: RefCell::new(Vec::new()),
        syncing: Cell::new(false),
        status_dot,
        status_label,
        engine_label,
        mirror_label,
        preview_hint,
        validation_label,
    });

    // Preset cards need the Rc<Widgets> for their callbacks.
    let (cards, preset_list) = {
        let model = Rc::downgrade(&model);
        let widgets = Rc::downgrade(&widgets);
        layout_cards::build(move |kind| {
            if let Some(widgets) = widgets.upgrade() {
                if widgets.presets_guard.get() {
                    return;
                }
            }
            if let Some(model) = model.upgrade() {
                model.borrow_mut().select_preset(kind);
                if let Some(widgets) = widgets.upgrade() {
                    update_ui(&model, &widgets);
                }
            }
        })
    };
    widgets.presets_box.append(&cards);
    *widgets.presets.borrow_mut() = preset_list;

    {
        let model = model.clone();
        let widgets = Rc::downgrade(&widgets);
        apply_button.connect_clicked(move |_| {
            if let Some(widgets) = widgets.upgrade() {
                start_apply(&model, &widgets);
            }
        });
    }
    {
        let parent = window.clone();
        about_button.connect_clicked(move |_| {
            let about = adw::AboutDialog::builder()
                .application_name("Displays")
                .application_icon(APP_ID)
                .version(env!("CARGO_PKG_VERSION"))
                .comments("Arrange displays and save the layout as a kanshi profile")
                .website("https://github.com/grigio/kanshi-gtk4")
                .issue_url("https://github.com/grigio/kanshi-gtk4/issues")
                .developers(["grigio"])
                .license_type(gtk::License::Gpl30Only)
                .build();
            about.present(Some(&parent));
        });
    }

    {
        let model = model.clone();
        let widgets = Rc::downgrade(&widgets);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            if key == gdk::Key::F5 {
                if let Some(widgets) = widgets.upgrade() {
                    request_refresh(&model, &widgets);
                }
                glib::Propagation::Stop
            } else if key == gdk::Key::Return && state.contains(gdk::ModifierType::CONTROL_MASK) {
                if let Some(widgets) = widgets.upgrade() {
                    start_apply(&model, &widgets);
                }
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        window.add_controller(keys);
    }

    {
        let model = model.clone();
        window.connect_close_request(move |_| {
            // Closing the main window during the countdown must not write a
            // keep marker. Dismiss the dialog and let the watchdog roll back
            // unless the in-app timer has already done so.
            let handle = model.borrow_mut().countdown.take();
            if let Some(handle) = handle {
                handle.close();
            }
            glib::Propagation::Proceed
        });
    }

    update_ui(&model, &widgets);
    install_preview_drag(&model, &widgets);

    {
        let model = model.clone();
        let widgets = Rc::downgrade(&widgets);
        glib::timeout_add_seconds_local(2, move || {
            if let Some(widgets) = widgets.upgrade() {
                request_refresh(&model, &widgets);
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let model = model.clone();
        let widgets = Rc::downgrade(&widgets);
        glib::timeout_add_local_once(Duration::from_millis(400), move || {
            if let Some(widgets) = widgets.upgrade() {
                check_stale_transactions(&model, &widgets);
            }
        });
    }

    AppWindow {
        window,
        _widgets: widgets,
    }
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label
}

#[derive(Clone)]
struct DragState {
    name: String,
    origin: (i32, i32),
    size: (i32, i32),
    scale: f64,
    /// The display this one is magnetised to: `(x, y, width, height)` in
    /// logical layout units. `None` when it is the only enabled display.
    anchor: Option<(i32, i32, i32, i32)>,
}

/// How close, in preview pixels, a dragged display must be to a flush position
/// before it is pulled in.
const PREVIEW_MAGNET_PX: f64 = 24.0;

/// The enabled display a dragged one should snap to: the built-in panel when
/// there is one, otherwise the first other enabled output.
fn snap_anchor(
    plan: &LayoutPlan,
    internal: Option<&str>,
    dragged: &str,
) -> Option<(i32, i32, i32, i32)> {
    let anchor = internal
        .filter(|name| *name != dragged)
        .and_then(|name| plan.outputs.get(name))
        .filter(|output| output.enabled)
        .or_else(|| {
            plan.outputs
                .iter()
                .find(|(name, output)| name.as_str() != dragged && output.enabled)
                .map(|(_, output)| output)
        })?;
    let (w, h) = anchor.logical_size();
    Some((anchor.position.0, anchor.position.1, w, h))
}

/// Let the user drag the external display around the preview. The laptop panel
/// stays where it is; a manual move turns the plan into `Custom`. While the
/// pointer is near the other display the dragged rectangle is pulled flush
/// against it, and on release it settles on the closest side.
fn install_preview_drag(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let state: Rc<RefCell<Option<DragState>>> = Rc::new(RefCell::new(None));
    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);

    {
        let state = state.clone();
        let model = model.clone();
        let widgets = Rc::downgrade(widgets);
        drag.connect_drag_begin(move |gesture, x, y| {
            let Some(widgets) = widgets.upgrade() else {
                return;
            };
            let app = model.borrow();
            if app.busy || app.tx.is_some() {
                return;
            }
            let plan = &app.plan;
            let area = &widgets.preview.area;
            let Some(geometry) =
                preview::Geometry::new(f64::from(area.width()), f64::from(area.height()), plan)
            else {
                return;
            };
            let Some(rect) = geometry.hit(x, y) else {
                return;
            };
            if app.internal.as_deref() == Some(rect.name.as_str()) {
                return;
            }
            let Some(output) = plan.outputs.get(&rect.name) else {
                return;
            };
            *state.borrow_mut() = Some(DragState {
                name: rect.name.clone(),
                origin: output.position,
                size: output.logical_size(),
                scale: geometry.scale,
                anchor: snap_anchor(plan, app.internal.as_deref(), &rect.name),
            });
            if let Some(widget) = gesture.widget() {
                widget.set_cursor_from_name(Some("grabbing"));
            }
        });
    }

    {
        let state = state.clone();
        let model = model.clone();
        let widgets = Rc::downgrade(widgets);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(widgets) = widgets.upgrade() else {
                return;
            };
            let Some(drag_state) = state.borrow().clone() else {
                return;
            };
            let position = drag_position(&drag_state, dx, dy, true);
            if model.borrow_mut().move_output(&drag_state.name, position) {
                update_ui(&model, &widgets);
            }
        });
    }

    {
        let state = state.clone();
        let model = model.clone();
        let widgets = Rc::downgrade(widgets);
        drag.connect_drag_end(move |gesture, dx, dy| {
            if let Some(widget) = gesture.widget() {
                widget.set_cursor_from_name(Some("grab"));
            }
            let Some(drag_state) = state.borrow_mut().take() else {
                return;
            };
            // Always settle on the closest side of the other display, even if
            // the drag ended far away: that is the magnetic auto-adjust.
            let position = drag_position(&drag_state, dx, dy, false);
            if model.borrow_mut().move_output(&drag_state.name, position) {
                if let Some(widgets) = widgets.upgrade() {
                    update_ui(&model, &widgets);
                }
            }
        });
    }

    {
        let model = model.clone();
        let motion_widgets = Rc::downgrade(widgets);
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            let Some(widgets) = motion_widgets.upgrade() else {
                return;
            };
            if state.borrow().is_some() {
                return;
            }
            let app = model.borrow();
            let area = &widgets.preview.area;
            let movable = preview::Geometry::new(
                f64::from(area.width()),
                f64::from(area.height()),
                &app.plan,
            )
            .as_ref()
            .and_then(|geometry| geometry.hit(x, y))
            .is_some_and(|rect| app.internal.as_deref() != Some(rect.name.as_str()));
            area.set_cursor_from_name(if movable { Some("grab") } else { None });
        });
        widgets.preview.area.add_controller(motion);
    }

    widgets.preview.area.add_controller(drag);
}

/// Where the dragged display goes for a pointer offset.
///
/// With `magnet` the flush position is used only when the pointer is near it;
/// without it the display always settles on the closest side.
fn drag_position(drag_state: &DragState, dx: f64, dy: f64, magnet: bool) -> (i32, i32) {
    let free = (
        drag_state.origin.0 + (dx / drag_state.scale).round() as i32,
        drag_state.origin.1 + (dy / drag_state.scale).round() as i32,
    );
    let Some(anchor) = drag_state.anchor else {
        return free;
    };
    if magnet {
        let threshold = ((PREVIEW_MAGNET_PX / drag_state.scale).round() as i32).max(1);
        preview::magnetic_position(free, drag_state.size, anchor, threshold)
    } else {
        preview::nearest_side_position(free, drag_state.size, anchor)
    }
}

fn update_ui(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    // `sync` writes widget state, which can emit notifications that call back
    // into here. A nested pass would only redo work that is already in flight.
    if widgets.syncing.replace(true) {
        return;
    }
    ensure_rows(model, widgets);
    {
        let model = model.borrow();
        widgets.sync(&model);
    }
    widgets.syncing.set(false);
}

/// Output names in display-list order: the built-in panel first, then the rest
/// alphabetically. The panel keeps its controls at the top of the list no
/// matter what its connector is called.
fn display_order(mut names: Vec<String>, internal: Option<&str>) -> Vec<String> {
    names.sort_by(|a, b| {
        let a_internal = Some(a.as_str()) == internal;
        let b_internal = Some(b.as_str()) == internal;
        b_internal.cmp(&a_internal).then_with(|| a.cmp(b))
    });
    names
}

fn ensure_rows(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let names = {
        let current = model.borrow();
        display_order(
            current.plan.outputs.keys().cloned().collect(),
            current.internal.as_deref(),
        )
    };
    if *widgets.row_names.borrow() == names {
        return;
    }
    let old_rows = std::mem::take(&mut *widgets.rows.borrow_mut());
    for (_, row) in old_rows {
        widgets.outputs_group.remove(&row.expander);
    }

    let model_weak = Rc::downgrade(model);
    let widgets_weak = Rc::downgrade(widgets);
    let handler: EditHandler = Rc::new(move |edit| {
        if let Some(model) = model_weak.upgrade() {
            let changed = model.borrow_mut().apply_edit(edit);
            if changed {
                if let Some(widgets) = widgets_weak.upgrade() {
                    update_ui(&model, &widgets);
                }
            }
        }
    });

    let current = model.borrow();
    for name in &names {
        let Some(output) = current.outputs.iter().find(|o| &o.name == name) else {
            continue;
        };
        let row = OutputRow::new(
            name,
            output,
            current.expanded.contains(name),
            handler.clone(),
        );
        widgets.outputs_group.add(&row.expander);
        widgets.rows.borrow_mut().insert(name.clone(), row);
    }
    *widgets.row_names.borrow_mut() = names;
}

fn reload_state(model: &Rc<RefCell<AppModel>>) {
    let path = model.borrow().paths.state_path.clone();
    if let Ok(state) = AppState::load(&path) {
        model.borrow_mut().state = state;
    }
}

fn toast(widgets: &Widgets, message: impl Into<String>) {
    let toast = adw::Toast::new(&message.into());
    toast.set_timeout(7);
    widgets.toast.add_toast(toast);
}

/// A failed background action. The toast gets the innermost reason (long text
/// is clipped), the log keeps the whole context chain.
struct Failure {
    short: String,
    full: String,
}

impl Failure {
    fn new(err: anyhow::Error) -> Self {
        Self {
            short: err.root_cause().to_string(),
            full: format!("{err:#}"),
        }
    }
}

/// Write a failure to stderr and to `<state dir>/errors.log`, so an instance
/// started from a menu still leaves something to read afterwards.
fn log_failure(paths: &Paths, action: &str, failure: &Failure) {
    eprintln!("{action} failed: {}", failure.full);
    let Some(dir) = paths.state_path.parent() else {
        return;
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("errors.log"))
    else {
        return;
    };
    let _ = writeln!(
        file,
        "{} {action} failed: {}",
        config::now_rfc3339(),
        failure.full
    );
}

struct RefreshResult {
    backend_info: BackendInfo,
    kanshi_info: KanshiInfo,
    outputs: Result<Vec<Output>, String>,
}

fn request_refresh(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let paths = {
        let mut model = model.borrow_mut();
        if model.refreshing {
            return;
        }
        if model.busy && model.tx.is_none() {
            return;
        }
        model.refreshing = true;
        model.paths.clone()
    };

    let model = model.clone();
    let widgets = widgets.clone();
    spawn_background(
        move || {
            let backend = WlrRandrBackend::new();
            let backend_info = backend
                .detect()
                .unwrap_or_else(|err| BackendInfo::unsupported(format!("{err:#}")));
            let outputs = if backend_info.supported {
                backend.list().map_err(|err| format!("{err:#}"))
            } else {
                Ok(Vec::new())
            };
            let kanshi_info = KanshiClient::new().detect(&paths.config_path);
            RefreshResult {
                backend_info,
                kanshi_info,
                outputs,
            }
        },
        move |result| {
            handle_refresh(result, &model, &widgets);
        },
    );
}

fn handle_refresh(result: RefreshResult, model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let mut abort_tx: Option<PreparedTx> = None;
    let mut message: Option<String> = None;

    {
        let mut model = model.borrow_mut();
        model.refreshing = false;
        model.backend_info = result.backend_info;
        model.kanshi_info = result.kanshi_info;
        match result.outputs {
            Ok(outputs) => {
                model.last_refresh_error = None;
                if let Some(tx) = &model.tx {
                    let prepared: BTreeSet<String> = tx.plan.outputs.keys().cloned().collect();
                    let live: BTreeSet<String> = outputs.iter().map(|o| o.name.clone()).collect();
                    if prepared != live {
                        abort_tx = model.tx.take();
                        if let Some(handle) = model.countdown.take() {
                            handle.close();
                        }
                    } else {
                        model.outputs = outputs;
                    }
                } else if !model.busy {
                    model.reconcile_outputs(outputs);
                }
            }
            Err(err) => {
                if model.last_refresh_error.as_deref() != Some(err.as_str()) {
                    message = Some(err.clone());
                    model.last_refresh_error = Some(err);
                }
            }
        }
    }

    if let Some(tx) = abort_tx {
        update_ui(model, widgets);
        toast(
            widgets,
            "A display was disconnected; reverting the pending layout.",
        );
        start_revert_with(tx, model, widgets);
        return;
    }
    update_ui(model, widgets);
    if let Some(message) = message {
        toast(widgets, message);
    }
}

fn start_apply(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let params = {
        let model = model.borrow();
        if model.busy || model.tx.is_some() || !model.backend_info.supported {
            return;
        }
        let validation = validate(&model.plan, &model.outputs);
        if !validation.is_valid() {
            toast(widgets, validation.errors.join(" "));
            return;
        }
        if model.plan.matches_live(&model.outputs) {
            toast(widgets, "The current layout is already applied.");
            return;
        }
        let timeout_secs = std::env::var("KANSHI_GTK4_TIMEOUT")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(watchdog::DEFAULT_TIMEOUT);
        ApplyParams {
            paths: model.paths.clone(),
            plan: model.plan.clone(),
            state: model.state.clone(),
            set_key: model.set_key.clone(),
            internal: model.internal.clone(),
            external: model.external.clone(),
            desktop_scales: model.desktop_scales.clone(),
            timeout_secs,
        }
    };

    {
        let mut model = model.borrow_mut();
        model.busy = true;
    }
    update_ui(model, widgets);

    let model = model.clone();
    let widgets = widgets.clone();
    spawn_background(
        move || apply::prepare_and_apply(params).map_err(Failure::new),
        move |result| match result {
            Ok(tx) => {
                {
                    let mut model = model.borrow_mut();
                    model.busy = false;
                    if let Ok(state) = serde_json::from_slice::<AppState>(&tx.state_after) {
                        model.state = state;
                    }
                    model.tx = Some(tx.clone());
                }
                update_ui(&model, &widgets);

                let note = apply::persistence_hint(&tx);
                let keep_model = model.clone();
                let keep_widgets = widgets.clone();
                let keep_tx = tx.clone();
                let on_keep = move || {
                    start_keep(keep_tx.clone(), &keep_model, &keep_widgets);
                };

                let revert_model = model.clone();
                let revert_widgets = widgets.clone();
                let revert_tx = tx.clone();
                let on_revert = move || {
                    start_revert(revert_tx.clone(), &revert_model, &revert_widgets);
                };

                let handle = countdown::present(
                    &widgets.window,
                    tx.remaining_secs(),
                    note,
                    on_keep,
                    on_revert,
                );
                model.borrow_mut().countdown = Some(handle);
            }
            Err(failure) => {
                {
                    let mut model = model.borrow_mut();
                    model.busy = false;
                }
                update_ui(&model, &widgets);
                let paths = model.borrow().paths.clone();
                log_failure(&paths, "apply", &failure);
                toast(
                    &widgets,
                    format!("Could not apply the layout: {}", failure.short),
                );
                request_refresh(&model, &widgets);
            }
        },
    );
}

fn start_keep(tx: PreparedTx, model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    {
        let mut model = model.borrow_mut();
        if model.busy || model.tx.as_ref().map(|current| &current.dir) != Some(&tx.dir) {
            return;
        }
        model.busy = true;
        model.tx = None;
        model.countdown = None;
    }
    update_ui(model, widgets);

    let model = model.clone();
    let widgets = widgets.clone();
    spawn_background(
        move || apply::finalize_keep(&tx).map_err(Failure::new),
        move |result| {
            {
                let mut model = model.borrow_mut();
                model.busy = false;
            }
            reload_state(&model);
            match result {
                Ok(warnings) => {
                    toast(&widgets, "Layout saved.");
                    for warning in warnings {
                        toast(&widgets, warning);
                    }
                }
                Err(failure) => {
                    let paths = model.borrow().paths.clone();
                    log_failure(&paths, "keep", &failure);
                    toast(
                        &widgets,
                        format!("Could not save the layout: {}", failure.short),
                    );
                }
            }
            update_ui(&model, &widgets);
            request_refresh(&model, &widgets);
        },
    );
}

fn start_revert(tx: PreparedTx, model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    {
        let mut model = model.borrow_mut();
        if model.busy {
            return;
        }
        if model.tx.as_ref().map(|current| &current.dir) != Some(&tx.dir) {
            return;
        }
        model.busy = true;
        model.tx = None;
        model.countdown = None;
    }
    start_revert_with(tx, model, widgets);
}

fn start_revert_with(tx: PreparedTx, model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    {
        let mut model = model.borrow_mut();
        if model.tx.is_some() {
            return;
        }
        model.busy = true;
    }
    update_ui(model, widgets);
    let model = model.clone();
    let widgets = widgets.clone();
    spawn_background(
        move || apply::revert(&tx).map_err(Failure::new),
        move |result| {
            {
                let mut model = model.borrow_mut();
                model.busy = false;
            }
            reload_state(&model);
            match result {
                Ok(report) => {
                    if report.fully_restored() {
                        toast(&widgets, "Reverted to the previous layout.");
                    } else {
                        toast(
                            &widgets,
                            format!("Reverted what was still connected ({}).", report.summary()),
                        );
                    }
                }
                Err(failure) => {
                    let paths = model.borrow().paths.clone();
                    log_failure(&paths, "revert", &failure);
                    toast(
                        &widgets,
                        format!(
                            "Could not revert the layout; the watchdog will retry: {}",
                            failure.short
                        ),
                    );
                }
            }
            update_ui(&model, &widgets);
            request_refresh(&model, &widgets);
        },
    );
}

fn check_stale_transactions(model: &Rc<RefCell<AppModel>>, widgets: &Rc<Widgets>) {
    let root = model.borrow().paths.transaction_root();
    let stale = watchdog::scan_stale(&root);
    let Some(dir) = stale.into_iter().next() else {
        return;
    };

    let dialog = adw::AlertDialog::new(
        Some("Restore a previous layout?"),
        Some(
            "kanshi-gtk4 found an interrupted display change. Restore the layout that was in \
             use before it, or discard the pending transaction?",
        ),
    );
    dialog.add_response("discard", "Discard");
    dialog.add_response("restore", "Restore layout");
    dialog.set_response_appearance("restore", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("restore"));
    dialog.set_close_response("discard");

    let model = model.clone();
    let widgets = widgets.clone();
    let parent = widgets.window.clone();
    dialog.choose(Some(&parent), gtk::gio::Cancellable::NONE, move |choice| {
        if choice == "restore" {
            let dir = dir.clone();
            let model2 = model.clone();
            let widgets2 = widgets.clone();
            spawn_background(
                move || watchdog::revert_cli(&dir).map_err(|err| format!("{err:#}")),
                move |result| {
                    match result {
                        Ok(report) => toast(
                            &widgets2,
                            format!("Restored the previous layout ({}).", report.summary()),
                        ),
                        Err(err) => toast(&widgets2, err),
                    }
                    request_refresh(&model2, &widgets2);
                },
            );
        } else {
            watchdog::cleanup_transaction(&dir);
        }
    });
}

/// Full-window status page for unsupported compositors or missing tools.
pub fn build_unavailable(
    application: &adw::Application,
    title: &str,
    description: &str,
) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Displays")
        .default_width(640)
        .default_height(480)
        .build();
    let header = adw::HeaderBar::new();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    let page = layout_cards::status_page(title, description);
    toolbar.set_content(Some(&page));
    window.set_content(Some(&toolbar));
    window
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::detect_layout;
    use crate::model::output::{Mode, Transform};
    use std::path::PathBuf;

    fn output(name: &str, modes: Vec<Mode>, scale: f64, position: (i32, i32)) -> Output {
        Output {
            name: name.to_string(),
            description: String::new(),
            make: None,
            model: None,
            serial: None,
            enabled: true,
            modes,
            position,
            scale,
            transform: Transform::default(),
            adaptive_sync: false,
        }
    }

    fn model() -> AppModel {
        let outputs = vec![
            output(
                "eDP-1",
                vec![
                    Mode::new(3120, 2080, 120_000),
                    Mode::new(1920, 1080, 120_000),
                ],
                2.0,
                (0, 0),
            ),
            output(
                "DP-1",
                vec![Mode::new(1920, 1200, 60_000), Mode::new(1600, 1200, 60_000)],
                1.5,
                (1560, 0),
            ),
        ];
        let internal = Some("eDP-1".to_string());
        let external = Some("DP-1".to_string());
        let desktop_scales = outputs.iter().map(|o| (o.name.clone(), o.scale)).collect();
        let kind = detect_layout(&outputs, internal.as_deref(), external.as_deref());
        let plan = LayoutPlan::from_outputs(
            &outputs,
            kind,
            internal.as_deref(),
            external.as_deref(),
            &desktop_scales,
        );
        let set_key = set_key(&["eDP-1".to_string(), "DP-1".to_string()], Some("eDP-1"));
        AppModel::from_session(Session {
            paths: Paths {
                config_path: PathBuf::from("/nonexistent/kanshi/config"),
                state_path: PathBuf::from("/nonexistent/state.json"),
                runtime_dir: PathBuf::from("/nonexistent/run"),
            },
            backend_info: BackendInfo {
                supported: true,
                reason: None,
                protocol_version: Some(4),
            },
            kanshi_info: KanshiInfo::default(),
            outputs,
            state: AppState::default(),
            internal,
            external,
            set_key,
            desktop_scales,
            kind,
            plan,
        })
    }

    #[test]
    fn repeating_an_edit_reports_no_change() {
        let mut model = model();
        let mode = Mode::new(1600, 1200, 60_000);
        assert!(model.apply_edit(OutputEdit::Mode {
            name: "DP-1".to_string(),
            mode: mode.clone(),
        }));

        // A drop-down reports its state back while it is being synced. Those
        // echoes must be no-ops: rebuilding the window for them re-splices the
        // list that raised the notification and the two spin forever.
        assert!(!model.apply_edit(OutputEdit::Mode {
            name: "DP-1".to_string(),
            mode,
        }));
        assert!(!model.apply_edit(OutputEdit::Scale {
            name: "DP-1".to_string(),
            scale: 1.5,
        }));
        assert!(!model.apply_edit(OutputEdit::AdaptiveSync {
            name: "DP-1".to_string(),
            enabled: false,
        }));
        assert!(!model.apply_edit(OutputEdit::Transform {
            name: "DP-1".to_string(),
            transform: Transform::Normal,
        }));
        assert!(!model.apply_edit(OutputEdit::Enabled {
            name: "DP-1".to_string(),
            enabled: true,
        }));
    }

    #[test]
    fn resolution_is_set_per_display() {
        let mut model = model();
        // Extend places the external display at the laptop's logical width.
        assert_eq!(model.plan.outputs["DP-1"].position, (1560, 0));

        // Shrinking the laptop panel moves the external display...
        assert!(model.apply_edit(OutputEdit::Mode {
            name: "eDP-1".to_string(),
            mode: Mode::new(1920, 1080, 120_000),
        }));
        assert_eq!(
            model.plan.outputs["eDP-1"].mode,
            Mode::new(1920, 1080, 120_000)
        );
        assert_eq!(model.plan.outputs["DP-1"].position, (960, 0));
        // ...without touching the external display's own mode.
        assert_eq!(
            model.plan.outputs["DP-1"].mode,
            Mode::new(1920, 1200, 60_000)
        );

        // And the other way round: the laptop panel keeps its resolution.
        assert!(model.apply_edit(OutputEdit::Mode {
            name: "DP-1".to_string(),
            mode: Mode::new(1600, 1200, 60_000),
        }));
        assert_eq!(
            model.plan.outputs["DP-1"].mode,
            Mode::new(1600, 1200, 60_000)
        );
        assert_eq!(
            model.plan.outputs["eDP-1"].mode,
            Mode::new(1920, 1080, 120_000)
        );
        assert_eq!(model.plan.outputs["eDP-1"].position, (0, 0));
    }

    #[test]
    fn periodic_refresh_keeps_the_edited_resolution() {
        let mut model = model();
        let chosen = Mode::new(1600, 1200, 60_000);
        assert!(model.apply_edit(OutputEdit::Mode {
            name: "DP-1".to_string(),
            mode: chosen.clone(),
        }));

        // The two-second refresh reports the live state, which still has the
        // old mode; the user's pick must survive.
        let live = model.outputs.clone();
        model.reconcile_outputs(live);

        assert_eq!(model.plan.outputs["DP-1"].mode, chosen);
        assert_eq!(model.plan.outputs["DP-1"].position, (1560, 0));
    }

    #[test]
    fn real_edits_report_change() {
        let mut model = model();
        assert!(model.apply_edit(OutputEdit::Scale {
            name: "DP-1".to_string(),
            scale: 1.0,
        }));
        assert!(model.apply_edit(OutputEdit::Enabled {
            name: "DP-1".to_string(),
            enabled: false,
        }));
    }

    #[test]
    fn internal_display_is_listed_first() {
        let names = vec![
            "DP-1".to_string(),
            "HDMI-A-1".to_string(),
            "eDP-1".to_string(),
        ];
        assert_eq!(
            display_order(names, Some("eDP-1")),
            vec!["eDP-1", "DP-1", "HDMI-A-1"]
        );
        // Without a built-in panel the connector order stays alphabetical.
        assert_eq!(
            display_order(vec!["DP-2".to_string(), "DP-1".to_string()], None),
            vec!["DP-1", "DP-2"]
        );
    }

    #[test]
    fn releasing_the_drag_settles_against_the_nearest_side() {
        let state = DragState {
            name: "DP-1".to_string(),
            origin: (1560, 0),
            size: (80, 60),
            scale: 2.0,
            anchor: Some((0, 0, 100, 100)),
        };
        // Dragged above the panel: the release snaps it on top, not at the
        // arbitrary pointer position.
        assert_eq!(drag_position(&state, -3120.0, -400.0, false), (0, -60));
        // A tiny offset that is already flush stays where it is.
        assert_eq!(drag_position(&state, 0.0, 0.0, false), (100, 0));
    }
}
