//! Per-output expander row with resolution, refresh, scale, rotation and VRR.

use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use crate::model::layout::{format_scale, OutputPlan};
use crate::model::output::{derived_modes, Mode, Output, Transform};

const BASE_SCALES: [f64; 11] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0];

#[derive(Debug, Clone)]
pub enum OutputEdit {
    Enabled { name: String, enabled: bool },
    Mode { name: String, mode: Mode },
    Scale { name: String, scale: f64 },
    Transform { name: String, transform: Transform },
    AdaptiveSync { name: String, enabled: bool },
    Expanded { name: String, expanded: bool },
}

pub type EditHandler = Rc<dyn Fn(OutputEdit)>;

#[derive(Default)]
struct RowData {
    modes: Vec<Mode>,
    resolutions: Vec<(u32, u32)>,
    custom_sizes: BTreeSet<(u32, u32)>,
    refresh_modes: Vec<Mode>,
    scales: Vec<f64>,
    current_mode: Option<Mode>,
    current_scale: f64,
    enabled: bool,
}

pub struct OutputRow {
    pub expander: adw::ExpanderRow,
    enabled: gtk::Switch,
    resolution: gtk::DropDown,
    refresh: gtk::DropDown,
    scale: gtk::DropDown,
    rotation: gtk::DropDown,
    vrr: gtk::Switch,
    resolution_list: gtk::StringList,
    refresh_list: gtk::StringList,
    scale_list: gtk::StringList,
    data: Rc<RefCell<RowData>>,
    updating: Rc<Cell<bool>>,
}

impl OutputRow {
    pub fn new(name: &str, output: &Output, expanded: bool, on_edit: EditHandler) -> Self {
        let expander = adw::ExpanderRow::new();
        expander.set_title(name);
        expander.set_expanded(expanded);

        let resolution_list = gtk::StringList::new(&[]);
        let resolution = gtk::DropDown::new(Some(resolution_list.clone()), None::<gtk::Expression>);
        resolution.set_valign(gtk::Align::Center);
        resolution.set_hexpand(false);
        resolution.set_width_request(190);
        resolution.set_tooltip_text(Some(
            "Resolution — entries marked (custom) are lower sizes requested from the compositor",
        ));

        let enabled = gtk::Switch::new();
        enabled.set_valign(gtk::Align::Center);
        let switch_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        switch_box.set_valign(gtk::Align::Center);
        switch_box.append(&resolution);
        let on_label = gtk::Label::new(Some("On"));
        on_label.add_css_class("dim-label");
        switch_box.append(&on_label);
        switch_box.append(&enabled);
        expander.add_suffix(&switch_box);

        let refresh_list = gtk::StringList::new(&[]);
        let refresh = gtk::DropDown::new(Some(refresh_list.clone()), None::<gtk::Expression>);
        refresh.set_valign(gtk::Align::Center);
        refresh.set_width_request(150);
        expander.add_row(&control_row("Refresh rate", &refresh));

        let scale_list = gtk::StringList::new(&[]);
        let scale = gtk::DropDown::new(Some(scale_list.clone()), None::<gtk::Expression>);
        scale.set_valign(gtk::Align::Center);
        scale.set_width_request(130);
        expander.add_row(&control_row("Scale", &scale));

        let rotation_labels: Vec<&str> = Transform::ROTATIONS.iter().map(|t| t.label()).collect();
        let rotation_list = gtk::StringList::new(&rotation_labels);
        let rotation = gtk::DropDown::new(Some(rotation_list.clone()), None::<gtk::Expression>);
        rotation.set_valign(gtk::Align::Center);
        rotation.set_width_request(150);
        expander.add_row(&control_row("Rotation", &rotation));

        let vrr = gtk::Switch::new();
        vrr.set_valign(gtk::Align::Center);
        let vrr_row = adw::ActionRow::builder()
            .title("Adaptive sync")
            .subtitle("Variable refresh rate (VRR), when supported")
            .build();
        vrr_row.add_suffix(&vrr);
        vrr_row.set_activatable_widget(Some(&vrr));
        expander.add_row(&vrr_row);

        let data = Rc::new(RefCell::new(RowData {
            modes: output.modes.clone(),
            ..RowData::default()
        }));
        let updating = Rc::new(Cell::new(false));

        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let updating = updating.clone();
            enabled.connect_active_notify(move |switch| {
                if updating.get() {
                    return;
                }
                on_edit(OutputEdit::Enabled {
                    name: name.clone(),
                    enabled: switch.is_active(),
                });
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let data = data.clone();
            let updating = updating.clone();
            resolution.connect_selected_notify(move |dropdown| {
                if updating.get() {
                    return;
                }
                let index = dropdown.selected() as usize;
                let data = data.borrow();
                let Some(size) = data.resolutions.get(index).copied() else {
                    return;
                };
                let candidates: Vec<&Mode> = data
                    .modes
                    .iter()
                    .filter(|m| (m.width, m.height) == size)
                    .collect();
                let selected = data
                    .current_mode
                    .as_ref()
                    .and_then(|current| candidates.iter().find(|m| m.same_timing(current)).copied())
                    .or_else(|| candidates.iter().find(|m| m.preferred).copied())
                    // Synthesized modes are not preferred; use the fastest
                    // refresh so a size change keeps the current 120 Hz.
                    .or_else(|| candidates.iter().max_by_key(|m| m.refresh_mhz).copied())
                    .cloned();
                drop(data);
                if let Some(mode) = selected {
                    on_edit(OutputEdit::Mode {
                        name: name.clone(),
                        mode,
                    });
                }
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let data = data.clone();
            let updating = updating.clone();
            refresh.connect_selected_notify(move |dropdown| {
                if updating.get() {
                    return;
                }
                let index = dropdown.selected() as usize;
                let mode = data.borrow().refresh_modes.get(index).cloned();
                if let Some(mode) = mode {
                    on_edit(OutputEdit::Mode {
                        name: name.clone(),
                        mode,
                    });
                }
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let data = data.clone();
            let updating = updating.clone();
            scale.connect_selected_notify(move |dropdown| {
                if updating.get() {
                    return;
                }
                let index = dropdown.selected() as usize;
                let value = data.borrow().scales.get(index).copied();
                if let Some(scale) = value {
                    on_edit(OutputEdit::Scale {
                        name: name.clone(),
                        scale,
                    });
                }
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let updating = updating.clone();
            rotation.connect_selected_notify(move |dropdown| {
                if updating.get() {
                    return;
                }
                let index = dropdown.selected() as usize;
                if let Some(transform) = Transform::ROTATIONS.get(index).copied() {
                    on_edit(OutputEdit::Transform {
                        name: name.clone(),
                        transform,
                    });
                }
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let updating = updating.clone();
            vrr.connect_active_notify(move |switch| {
                if updating.get() {
                    return;
                }
                on_edit(OutputEdit::AdaptiveSync {
                    name: name.clone(),
                    enabled: switch.is_active(),
                });
            });
        }
        {
            let on_edit = on_edit.clone();
            let name = name.to_string();
            let updating = updating.clone();
            expander.connect_expanded_notify(move |row| {
                if updating.get() {
                    return;
                }
                on_edit(OutputEdit::Expanded {
                    name: name.clone(),
                    expanded: row.is_expanded(),
                });
            });
        }

        let row = Self {
            expander,
            enabled,
            resolution,
            refresh,
            scale,
            rotation,
            vrr,
            resolution_list,
            refresh_list,
            scale_list,
            data,
            updating,
        };
        row.sync(output, None, expanded, output.scale);
        row
    }

    pub fn sync(
        &self,
        output: &Output,
        plan: Option<&OutputPlan>,
        expanded: bool,
        desktop_scale: f64,
    ) {
        self.updating.set(true);
        let fallback = plan.is_none();
        let current_mode = plan
            .map(|p| p.mode.clone())
            .unwrap_or_else(|| output.active_mode());
        let current_scale = plan.map(|p| p.scale).unwrap_or(output.scale);
        let current_transform = plan.map(|p| p.transform).unwrap_or(output.transform);
        let enabled = plan.map(|p| p.enabled).unwrap_or(output.enabled);
        let adaptive_sync = plan
            .map(|p| p.adaptive_sync)
            .unwrap_or(output.adaptive_sync);

        {
            let mut data = self.data.borrow_mut();
            // Advertised modes plus lower synthesized sizes. When a
            // synthesized mode is currently active the compositor lists it as
            // an ordinary mode, so mark the matching entry as custom instead
            // of adding a duplicate.
            let derived = derived_modes(output);
            let mut modes = output.modes.clone();
            for mode in &derived {
                if let Some(existing) = modes.iter_mut().find(|m| m.same_timing(mode)) {
                    existing.custom = true;
                } else {
                    modes.push(mode.clone());
                }
            }
            data.modes = modes;
            data.custom_sizes = derived.iter().map(|m| (m.width, m.height)).collect();
            data.current_mode = Some(current_mode.clone());
            data.current_scale = current_scale;
            data.enabled = enabled;

            let mut resolutions: Vec<(u32, u32)> =
                data.modes.iter().map(|m| (m.width, m.height)).collect();
            resolutions.sort_by(|a, b| {
                let area_a = u64::from(a.0) * u64::from(a.1);
                let area_b = u64::from(b.0) * u64::from(b.1);
                area_b.cmp(&area_a).then_with(|| b.0.cmp(&a.0))
            });
            resolutions.dedup();
            data.resolutions = resolutions;

            let mut refresh_modes: Vec<Mode> = data
                .modes
                .iter()
                .filter(|m| m.width == current_mode.width && m.height == current_mode.height)
                .cloned()
                .collect();
            refresh_modes.sort_by_key(|mode| std::cmp::Reverse(mode.refresh_mhz));
            data.refresh_modes = refresh_modes;

            let mut scales = BASE_SCALES.to_vec();
            if !scales.iter().any(|s| (s - current_scale).abs() < 0.001) {
                scales.push(current_scale);
            }
            if !scales.iter().any(|s| (s - desktop_scale).abs() < 0.001) {
                scales.push(desktop_scale);
            }
            scales.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            data.scales = scales;
        }

        if self.enabled.is_active() != enabled {
            self.enabled.set_active(enabled);
        }
        if self.vrr.is_active() != adaptive_sync {
            self.vrr.set_active(adaptive_sync);
        }

        // Resolution list.
        let (labels, selected) = {
            let data = self.data.borrow();
            let labels: Vec<String> = data
                .resolutions
                .iter()
                .map(|(w, h)| {
                    if data.custom_sizes.contains(&(*w, *h)) {
                        format!("{w} × {h} (custom)")
                    } else {
                        format!("{w} × {h}")
                    }
                })
                .collect();
            let selected = data
                .resolutions
                .iter()
                .position(|(w, h)| *w == current_mode.width && *h == current_mode.height);
            (labels, selected)
        };
        set_items(&self.resolution_list, &labels);
        set_selected(
            &self.resolution,
            selected
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );

        // Refresh list.
        let refresh_labels: Vec<String> = self
            .data
            .borrow()
            .refresh_modes
            .iter()
            .map(|m| format!("{} Hz", m.refresh_string()))
            .collect();
        set_items(&self.refresh_list, &refresh_labels);
        let selected = self
            .data
            .borrow()
            .refresh_modes
            .iter()
            .position(|m| m.same_timing(&current_mode))
            .or_else(|| {
                self.data
                    .borrow()
                    .refresh_modes
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, m)| m.refresh_mhz.abs_diff(current_mode.refresh_mhz))
                    .map(|(i, _)| i)
            });
        set_selected(
            &self.refresh,
            selected
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );

        // Scale list.
        let scale_labels: Vec<String> = self
            .data
            .borrow()
            .scales
            .iter()
            .map(|s| format_scale(*s))
            .collect();
        set_items(&self.scale_list, &scale_labels);
        let selected = self
            .data
            .borrow()
            .scales
            .iter()
            .position(|s| (s - current_scale).abs() < 0.001);
        set_selected(
            &self.scale,
            selected
                .map(|i| i as u32)
                .unwrap_or(gtk::INVALID_LIST_POSITION),
        );

        let rotation_index = Transform::ROTATIONS
            .iter()
            .position(|t| *t == current_transform)
            .unwrap_or(0);
        set_selected(&self.rotation, rotation_index as u32);

        self.resolution.set_sensitive(enabled);
        self.refresh.set_sensitive(enabled);
        self.scale.set_sensitive(enabled);
        self.rotation.set_sensitive(enabled);
        self.vrr.set_sensitive(enabled);

        let description = if output.description.trim().is_empty() {
            "Connected display".to_string()
        } else {
            output.description.trim().to_string()
        };
        let state = if enabled {
            format!(
                "{} · {} · scale {}{}",
                description,
                current_mode,
                format_scale(current_scale),
                if fallback { " · live" } else { "" }
            )
        } else {
            format!("{description} · disabled")
        };
        self.expander.set_subtitle(&state);
        self.expander.set_expanded(expanded);

        self.updating.set(false);
    }
}

fn control_row(title: &str, dropdown: &gtk::DropDown) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.add_suffix(dropdown);
    row.set_activatable_widget(Some(dropdown));
    row
}

/// Replace `list`'s items only when they actually differ from `labels`.
///
/// `GtkStringList::splice` always emits `items-changed`, and a drop-down reacts
/// to that by resetting its selection and emitting `notify::selected`. Doing it
/// on every two-second refresh therefore wakes the edit handlers for no reason
/// (and spins forever while a popup is open), so keep no-op updates silent.
fn set_items(list: &gtk::StringList, labels: &[String]) {
    let same = list.n_items() as usize == labels.len()
        && labels.iter().enumerate().all(|(index, label)| {
            list.string(index as u32)
                .is_some_and(|current| current.as_str() == label.as_str())
        });
    if same {
        return;
    }
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    list.splice(0, list.n_items(), &refs);
}

/// Select `index` only when it is not already selected, avoiding the property
/// notification (and the edit it triggers) for an unchanged value.
fn set_selected(dropdown: &gtk::DropDown, index: u32) {
    if dropdown.selected() != index {
        dropdown.set_selected(index);
    }
}
