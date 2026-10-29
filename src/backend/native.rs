//! Native `zwlr_output_manager_v1` backend.
//!
//! Talks the protocol directly through `wayland-client` instead of spawning
//! `wlr-randr` and parsing its JSON. This removes a runtime dependency, the
//! argv-building layer and the process-per-refresh cost, and it exposes the
//! EDID make/model/serial events the shell-out backend cannot report.
//!
//! Every operation uses a short-lived connection. The protocol is a snapshot
//! protocol: a configuration is only valid for the `serial` of the `done`
//! event it was built from, so a fresh capture per apply is simpler and safer
//! than keeping a long-lived connection in sync. If the outputs change
//! between capture and apply the compositor answers `cancelled`, and
//! [`NativeBackend::apply`] retries once with a new snapshot.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;

use wayland_client::{
    globals::{registry_queue_init, BindError, GlobalListContents},
    protocol::{wl_output, wl_registry},
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self as config_head, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self as configuration, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self as output_head, AdaptiveSyncState, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self as manager, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self as output_mode, ZwlrOutputModeV1},
};

use super::{BackendInfo, OutputBackend};
use crate::model::layout::LayoutPlan;
use crate::model::output::{Mode, Output, Transform};

/// A mode advertised by a head.
struct ModeRec {
    width: u32,
    height: u32,
    refresh_mhz: u32,
    preferred: bool,
}

impl ModeRec {
    fn matches(&self, wanted: &Mode) -> bool {
        if self.width != wanted.width || self.height != wanted.height {
            return false;
        }
        wanted.refresh_mhz == 0 || self.refresh_mhz.abs_diff(wanted.refresh_mhz) <= 1
    }
}

struct Head {
    proxy: ZwlrOutputHeadV1,
    name: Option<String>,
    description: String,
    make: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    enabled: bool,
    current_mode: Option<u32>,
    position: (i32, i32),
    transform: Transform,
    scale: f64,
    adaptive_sync: bool,
    /// Protocol ids of the modes advertised for this head, in event order.
    modes: Vec<u32>,
}

impl Head {
    fn new(proxy: ZwlrOutputHeadV1) -> Self {
        Self {
            proxy,
            name: None,
            description: String::new(),
            make: None,
            model: None,
            serial: None,
            enabled: false,
            current_mode: None,
            position: (0, 0),
            transform: Transform::Normal,
            scale: 1.0,
            adaptive_sync: false,
            modes: Vec::new(),
        }
    }

    fn find_mode<'a>(
        &self,
        modes: &'a BTreeMap<u32, ModeRec>,
        wanted: &Mode,
    ) -> Option<(u32, &'a ModeRec)> {
        self.modes
            .iter()
            .filter_map(|id| modes.get(id).map(|mode| (*id, mode)))
            .find(|(_, mode)| mode.matches(wanted))
    }

    /// The advertised mode to select for `wanted`: the exact timing, or the
    /// same size with the closest refresh rate. A saved plan may carry a
    /// rounded rate that never matches the compositor's value exactly.
    fn best_mode<'a>(
        &self,
        modes: &'a BTreeMap<u32, ModeRec>,
        wanted: &Mode,
    ) -> Option<(u32, &'a ModeRec)> {
        self.find_mode(modes, wanted).or_else(|| {
            self.modes
                .iter()
                .filter_map(|id| modes.get(id).map(|mode| (*id, mode)))
                .filter(|(_, mode)| mode.width == wanted.width && mode.height == wanted.height)
                .min_by_key(|(_, mode)| mode.refresh_mhz.abs_diff(wanted.refresh_mhz))
        })
    }
}

enum ConfigResult {
    Succeeded,
    Failed,
    Cancelled,
}

struct NativeState {
    manager: ZwlrOutputManagerV1,
    /// Heads keyed by protocol id; `to_outputs` sorts by name.
    heads: BTreeMap<u32, Head>,
    modes: BTreeMap<u32, ModeRec>,
    /// Proxy for each advertised mode, needed to select it in a configuration.
    mode_proxies: BTreeMap<u32, ZwlrOutputModeV1>,
    serial: Option<u32>,
    result: Option<ConfigResult>,
}

impl NativeState {
    fn new(manager: ZwlrOutputManagerV1) -> Self {
        Self {
            manager,
            heads: BTreeMap::new(),
            modes: BTreeMap::new(),
            mode_proxies: BTreeMap::new(),
            serial: None,
            result: None,
        }
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        let mut outputs = Vec::with_capacity(self.heads.len());
        for head in self.heads.values() {
            let Some(name) = head.name.clone() else {
                bail!("the compositor advertised a head without a name");
            };
            let mut modes: Vec<Mode> = head
                .modes
                .iter()
                .filter_map(|id| self.modes.get(id))
                .map(|mode| Mode {
                    width: mode.width,
                    height: mode.height,
                    refresh_mhz: mode.refresh_mhz,
                    preferred: mode.preferred,
                    current: false,
                    custom: false,
                    // `current` is filled in below: it is a property of the
                    // head, not of the mode object.
                })
                .collect();
            let current = head
                .current_mode
                .and_then(|id| self.modes.get(&id))
                .map(|mode| (mode.width, mode.height, mode.refresh_mhz));
            if let Some((width, height, refresh)) = current {
                match modes.iter_mut().find(|mode| {
                    mode.width == width
                        && mode.height == height
                        && mode.refresh_mhz.abs_diff(refresh) <= 1
                }) {
                    Some(mode) => mode.current = true,
                    // Defensive: a compositor may report a current mode that
                    // was not advertised (a custom mode). Keep the timing so
                    // the UI shows what is actually on screen.
                    None => {
                        let mut mode = Mode::new(width, height, refresh);
                        mode.current = true;
                        mode.custom = true;
                        modes.push(mode);
                    }
                }
            }
            // Show the biggest and fastest modes first, like `wlr-randr`.
            modes.sort_by(|a, b| {
                (b.width * b.height)
                    .cmp(&(a.width * a.height))
                    .then(b.refresh_mhz.cmp(&a.refresh_mhz))
                    .then(b.preferred.cmp(&a.preferred))
            });
            outputs.push(Output {
                name,
                description: head.description.clone(),
                make: head.make.clone(),
                model: head.model.clone(),
                serial: head.serial.clone(),
                enabled: head.enabled,
                modes,
                position: head.position,
                scale: head.scale,
                transform: head.transform,
                adaptive_sync: head.adaptive_sync,
            });
        }
        outputs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(outputs)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for NativeState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `registry_queue_init` keeps the global list up to date; the manager
        // is bound once and no other global is interesting.
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for NativeState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            manager::Event::Head { head } => {
                let id = head.id().protocol_id();
                state.heads.insert(id, Head::new(head));
            }
            manager::Event::Done { serial } => state.serial = Some(serial),
            manager::Event::Finished => {}
            _ => {}
        }
    }

    wayland_client::event_created_child!(NativeState, ZwlrOutputManagerV1, [
        manager::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for NativeState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: output_head::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        let Some(head) = state.heads.get_mut(&id) else {
            return;
        };
        match event {
            output_head::Event::Name { name } => head.name = Some(name),
            output_head::Event::Description { description } => head.description = description,
            output_head::Event::Make { make } => head.make = Some(make).filter(|s| !s.is_empty()),
            output_head::Event::Model { model } => {
                head.model = Some(model).filter(|s| !s.is_empty())
            }
            output_head::Event::SerialNumber { serial_number } => {
                head.serial = Some(serial_number).filter(|s| !s.is_empty())
            }
            output_head::Event::Mode { mode } => {
                let mode_id = mode.id().protocol_id();
                state.mode_proxies.insert(mode_id, mode.clone());
                state.modes.insert(
                    mode_id,
                    ModeRec {
                        width: 0,
                        height: 0,
                        refresh_mhz: 0,
                        preferred: false,
                    },
                );
                head.modes.push(mode_id);
            }
            output_head::Event::Enabled { enabled } => head.enabled = enabled != 0,
            output_head::Event::CurrentMode { mode } => {
                head.current_mode = Some(mode.id().protocol_id())
            }
            output_head::Event::Position { x, y } => head.position = (x, y),
            output_head::Event::Transform { transform } => {
                head.transform = match transform {
                    WEnum::Value(transform) => transform_from_wayland(transform),
                    WEnum::Unknown(_) => Transform::Normal,
                }
            }
            output_head::Event::Scale { scale } => head.scale = scale,
            output_head::Event::AdaptiveSync { state: sync } => {
                head.adaptive_sync = matches!(sync, WEnum::Value(AdaptiveSyncState::Enabled));
            }
            output_head::Event::Finished => {
                state.heads.remove(&id);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(NativeState, ZwlrOutputHeadV1, [
        output_head::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for NativeState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: output_mode::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        match event {
            output_mode::Event::Size { width, height } => {
                if let Some(mode) = state.modes.get_mut(&id) {
                    mode.width = width.max(0) as u32;
                    mode.height = height.max(0) as u32;
                }
            }
            output_mode::Event::Refresh { refresh } => {
                if let Some(mode) = state.modes.get_mut(&id) {
                    mode.refresh_mhz = refresh.max(0) as u32;
                }
            }
            output_mode::Event::Preferred => {
                if let Some(mode) = state.modes.get_mut(&id) {
                    mode.preferred = true;
                }
            }
            output_mode::Event::Finished => {
                state.modes.remove(&id);
                state.mode_proxies.remove(&id);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for NativeState {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: configuration::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.result = Some(match event {
            configuration::Event::Succeeded => ConfigResult::Succeeded,
            configuration::Event::Failed => ConfigResult::Failed,
            configuration::Event::Cancelled => ConfigResult::Cancelled,
            _ => return,
        });
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for NativeState {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: config_head::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The interface has no events.
    }
}

/// One connected protocol session holding a captured output state.
struct Session {
    queue: EventQueue<NativeState>,
    state: NativeState,
}

impl Session {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connecting to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;
        let (globals, queue) =
            registry_queue_init::<NativeState>(&conn).context("reading the Wayland registry")?;
        let qh = queue.handle();
        let manager = match globals.bind::<ZwlrOutputManagerV1, _, _>(&qh, 1..=4, ()) {
            Ok(manager) => manager,
            Err(BindError::NotPresent) => bail!(
                "This compositor does not support zwlr_output_manager_v1. \
                 The protocol comes from wlroots: labwc, sway, niri and Hyprland \
                 provide it. GNOME (Mutter) and KDE (KWin) do not, so this app \
                 cannot change outputs there."
            ),
            Err(BindError::UnsupportedVersion) => {
                bail!("the compositor advertises an unsupported zwlr_output_manager_v1 version")
            }
        };
        let mut session = Self {
            queue,
            state: NativeState::new(manager),
        };
        // The first roundtrip delivers the heads, their modes and `done`.
        // A second one covers compositors that send properties in several
        // batches.
        for _ in 0..3 {
            session.queue.roundtrip(&mut session.state)?;
            if session.state.serial.is_some() {
                return Ok(session);
            }
        }
        bail!("the compositor did not report its output configuration")
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        self.state.outputs()
    }

    fn serial(&self) -> Result<u32> {
        self.state
            .serial
            .ok_or_else(|| anyhow!("the compositor did not send a configuration serial"))
    }

    /// Create a configuration covering every connected head, as the protocol
    /// requires, and let the caller send `apply` or `test`.
    fn build(&self, plan: &LayoutPlan) -> Result<ZwlrOutputConfigurationV1> {
        let qh = self.queue.handle();
        for name in plan.outputs.keys() {
            if !self
                .state
                .heads
                .values()
                .any(|h| h.name.as_deref() == Some(name))
            {
                bail!("{name} is not connected");
            }
        }
        let config = self
            .state
            .manager
            .create_configuration(self.serial()?, &qh, ());
        for head in self.state.heads.values() {
            let Some(name) = head.name.as_deref() else {
                bail!("the compositor advertised a head without a name");
            };
            let Some(wanted) = plan.outputs.get(name) else {
                // The protocol errors on omitted heads. A plan that does not
                // mention this head is stale; disable it rather than leave the
                // configuration incomplete.
                config.disable_head(&head.proxy);
                continue;
            };
            if !wanted.enabled {
                config.disable_head(&head.proxy);
                continue;
            }
            let head_config = config.enable_head(&head.proxy, &qh, ());
            match head.best_mode(&self.state.modes, &wanted.mode) {
                Some((mode_id, _)) if !wanted.mode.custom => {
                    if let Some(proxy) = self.state.mode_proxies.get(&mode_id) {
                        head_config.set_mode(proxy);
                    }
                }
                // A custom mode is either explicitly requested or not
                // advertised at all (wlr-randr used `--custom-mode` for this).
                _ => head_config.set_custom_mode(
                    wanted.mode.width.min(i32::MAX as u32) as i32,
                    wanted.mode.height.min(i32::MAX as u32) as i32,
                    wanted.mode.refresh_mhz.min(i32::MAX as u32) as i32,
                ),
            }
            head_config.set_position(wanted.position.0, wanted.position.1);
            head_config.set_transform(transform_to_wayland(wanted.transform));
            head_config.set_scale(wanted.scale);
            if self.state.manager.version() >= 4 {
                let sync = if wanted.adaptive_sync {
                    AdaptiveSyncState::Enabled
                } else {
                    AdaptiveSyncState::Disabled
                };
                head_config.set_adaptive_sync(sync);
            }
        }
        Ok(config)
    }

    fn wait(&mut self) -> Result<ConfigResult> {
        // The result arrives before the sync callback of a roundtrip. A few
        // roundtrips absorb a compositor that answers late.
        for _ in 0..4 {
            self.queue.roundtrip(&mut self.state)?;
            if let Some(result) = self.state.result.take() {
                return Ok(result);
            }
        }
        bail!("the compositor did not answer the configuration request")
    }
}

fn run(plan: &LayoutPlan, test_only: bool) -> Result<()> {
    let mut session = Session::connect()?;
    let config = session.build(plan)?;
    if test_only {
        config.test();
    } else {
        config.apply();
    }
    let result = session.wait()?;
    config.destroy();
    match result {
        ConfigResult::Succeeded => Ok(()),
        ConfigResult::Failed => bail!("the compositor rejected the display configuration"),
        ConfigResult::Cancelled => {
            bail!("Displays changed during apply, please try again")
        }
    }
}

/// The native backend. Stateless: every call connects to the compositor.
#[derive(Debug, Clone, Copy, Default)]
pub struct NativeBackend;

impl NativeBackend {
    pub fn new() -> Self {
        Self
    }
}

impl OutputBackend for NativeBackend {
    fn detect(&self) -> Result<BackendInfo> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none()
            && std::env::var_os("WAYLAND_SOCKET").is_none()
        {
            return Ok(BackendInfo::unsupported(
                "No Wayland session detected. This app needs a Wayland compositor that \
                 implements zwlr_output_manager_v1 (labwc, sway, niri, Hyprland, …).",
            ));
        }
        match Session::connect() {
            Ok(session) => Ok(BackendInfo::supported(Some(
                session.state.manager.version(),
            ))),
            Err(err) => Ok(BackendInfo::unsupported(format!(
                "The compositor did not answer the output-management protocol: {err:#}"
            ))),
        }
    }

    fn list(&self) -> Result<Vec<Output>> {
        Session::connect()?.outputs()
    }

    fn dry_run(&self, plan: &LayoutPlan) -> Result<()> {
        run(plan, true).context("dry run failed")
    }

    fn apply(&self, plan: &LayoutPlan) -> Result<()> {
        match run(plan, false) {
            // A hotplug between the capture and the apply cancels the
            // configuration. One retry with a fresh snapshot is what the
            // protocol documentation recommends.
            Err(err) if is_cancelled(&err) => run(plan, false),
            other => other,
        }
    }
}

fn is_cancelled(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("Displays changed during apply")
}

fn transform_from_wayland(transform: wl_output::Transform) -> Transform {
    match transform {
        wl_output::Transform::Normal => Transform::Normal,
        wl_output::Transform::_90 => Transform::Rotate90,
        wl_output::Transform::_180 => Transform::Rotate180,
        wl_output::Transform::_270 => Transform::Rotate270,
        wl_output::Transform::Flipped => Transform::Flipped,
        wl_output::Transform::Flipped90 => Transform::Flipped90,
        wl_output::Transform::Flipped180 => Transform::Flipped180,
        wl_output::Transform::Flipped270 => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

fn transform_to_wayland(transform: Transform) -> wl_output::Transform {
    match transform {
        Transform::Normal => wl_output::Transform::Normal,
        Transform::Rotate90 => wl_output::Transform::_90,
        Transform::Rotate180 => wl_output::Transform::_180,
        Transform::Rotate270 => wl_output::Transform::_270,
        Transform::Flipped => wl_output::Transform::Flipped,
        Transform::Flipped90 => wl_output::Transform::Flipped90,
        Transform::Flipped180 => wl_output::Transform::Flipped180,
        Transform::Flipped270 => wl_output::Transform::Flipped270,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_zero_matches_any_rate() {
        let mode = ModeRec {
            width: 1920,
            height: 1200,
            refresh_mhz: 60_000,
            preferred: true,
        };
        assert!(mode.matches(&Mode::new(1920, 1200, 0)));
        assert!(mode.matches(&Mode::new(1920, 1200, 60_000)));
        assert!(!mode.matches(&Mode::new(1920, 1200, 120_000)));
        assert!(!mode.matches(&Mode::new(2560, 1440, 60_000)));
    }

    #[test]
    fn transforms_roundtrip_through_wayland() {
        use Transform::*;
        for transform in [
            Normal, Rotate90, Rotate180, Rotate270, Flipped, Flipped90, Flipped180, Flipped270,
        ] {
            assert_eq!(
                transform_from_wayland(transform_to_wayland(transform)),
                transform
            );
        }
    }

    /// Manual check against the running compositor:
    /// `cargo test -- --ignored native_dry_run`.
    ///
    /// The plan is built from the live outputs, so `test()` validates the
    /// current layout and `apply()` re-applies exactly that layout (a no-op
    /// apart from a possible brief modeset).
    #[test]
    #[ignore = "needs a live Wayland session with zwlr_output_manager_v1"]
    fn native_dry_run_matches_the_live_layout() {
        use crate::model::layout::LayoutKind;
        use std::collections::BTreeMap;

        let backend = NativeBackend::new();
        let info = backend.detect().expect("detect");
        assert!(info.supported, "{:?}", info.reason);
        let outputs = backend.list().expect("list");
        for output in &outputs {
            println!(
                "{} enabled={} {}x{}@{} pos={:?} scale={} transform={} adaptive={}",
                output.name,
                output.enabled,
                output.active_mode().width,
                output.active_mode().height,
                output.active_mode().refresh_mhz,
                output.position,
                output.scale,
                output.transform.as_str(),
                output.adaptive_sync
            );
        }
        assert!(!outputs.is_empty(), "no outputs connected");
        let scales: BTreeMap<String, f64> =
            outputs.iter().map(|o| (o.name.clone(), o.scale)).collect();
        let internal = outputs
            .iter()
            .find(|o| o.is_internal())
            .map(|o| o.name.as_str());
        let external = outputs
            .iter()
            .map(|o| o.name.as_str())
            .find(|name| Some(*name) != internal);
        let plan =
            LayoutPlan::from_outputs(&outputs, LayoutKind::Extend, internal, external, &scales);
        backend.dry_run(&plan).expect("dry run");
        // Applying the layout the compositor just reported is a no-op, but
        // it exercises the configuration path end to end.
        backend.apply(&plan).expect("apply");
        let after = backend.list().expect("list after apply");
        assert!(
            plan.matches_live_relaxed(&after),
            "compositor did not keep the layout: {after:?}"
        );
    }
}
