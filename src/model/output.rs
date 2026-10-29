//! Outputs, modes and transforms as reported by the compositor
//! (`zwlr_output_manager_v1`; also the shape of `wlr-randr --json`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Output transform, matching the `wl_output` and kanshi values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transform {
    #[serde(rename = "normal")]
    #[default]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    #[serde(rename = "flipped")]
    Flipped,
    #[serde(rename = "flipped-90")]
    Flipped90,
    #[serde(rename = "flipped-180")]
    Flipped180,
    #[serde(rename = "flipped-270")]
    Flipped270,
}

impl Transform {
    pub const ROTATIONS: [Transform; 4] = [
        Transform::Normal,
        Transform::Rotate90,
        Transform::Rotate180,
        Transform::Rotate270,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Transform::Normal => "normal",
            Transform::Rotate90 => "90",
            Transform::Rotate180 => "180",
            Transform::Rotate270 => "270",
            Transform::Flipped => "flipped",
            Transform::Flipped90 => "flipped-90",
            Transform::Flipped180 => "flipped-180",
            Transform::Flipped270 => "flipped-270",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Transform::Normal => "Normal",
            Transform::Rotate90 => "90°",
            Transform::Rotate180 => "180°",
            Transform::Rotate270 => "270°",
            Transform::Flipped => "Flipped",
            Transform::Flipped90 => "Flipped 90°",
            Transform::Flipped180 => "Flipped 180°",
            Transform::Flipped270 => "Flipped 270°",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "normal" => Self::Normal,
            "90" => Self::Rotate90,
            "180" => Self::Rotate180,
            "270" => Self::Rotate270,
            "flipped" => Self::Flipped,
            "flipped-90" => Self::Flipped90,
            "flipped-180" => Self::Flipped180,
            "flipped-270" => Self::Flipped270,
            _ => return None,
        })
    }

    /// True when the transform swaps width and height.
    pub fn rotates(self) -> bool {
        matches!(
            self,
            Transform::Rotate90
                | Transform::Rotate270
                | Transform::Flipped90
                | Transform::Flipped270
        )
    }
}

impl fmt::Display for Transform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One mode advertised by an output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in milli-Hz. Integer math avoids float equality issues.
    pub refresh_mhz: u32,
    #[serde(default)]
    pub preferred: bool,
    #[serde(default)]
    pub current: bool,
    /// True when this mode is not advertised by the panel and has to be set
    /// with the compositor's custom-mode request (`--custom-mode` in wlr-randr,
    /// kanshi `mode --custom`).
    #[serde(default)]
    pub custom: bool,
}

impl PartialEq for Mode {
    fn eq(&self, other: &Self) -> bool {
        self.same_timing(other)
    }
}

impl Eq for Mode {}

impl Mode {
    pub fn new(width: u32, height: u32, refresh_mhz: u32) -> Self {
        Self {
            width,
            height,
            refresh_mhz,
            preferred: false,
            current: false,
            custom: false,
        }
    }

    pub fn refresh_hz(&self) -> f64 {
        f64::from(self.refresh_mhz) / 1000.0
    }

    /// True when width, height and refresh match (within one milli-Hz).
    pub fn same_timing(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.refresh_mhz.abs_diff(other.refresh_mhz) <= 1
    }

    pub fn same_size(&self, other: &Self) -> bool {
        self.width == other.width && self.height == other.height
    }

    /// Refresh formatted with up to three decimals, as accepted by
    /// kanshi: `120`, `59.951`.
    pub fn refresh_string(&self) -> String {
        let hz = self.refresh_hz();
        let rounded = (hz * 1000.0).round() / 1000.0;
        if (rounded - rounded.round()).abs() < 0.0005 {
            format!("{}", rounded.round() as i64)
        } else {
            let mut s = format!("{rounded:.3}");
            while s.ends_with('0') {
                s.pop();
            }
            s
        }
    }

    /// `3120x2080@120` — the format used in kanshi configs. A mode without
    /// an explicit refresh keeps the short `1920x1080` form.
    pub fn to_kanshi(&self) -> String {
        if self.refresh_mhz == 0 {
            format!("{}x{}", self.width, self.height)
        } else {
            format!("{}x{}@{}", self.width, self.height, self.refresh_string())
        }
    }

    /// Parse `3120x2080@120`, `1920x1200` or `1920x1200@60Hz`.
    pub fn parse_kanshi(value: &str) -> Option<Self> {
        let value = value.trim().trim_end_matches("Hz");
        let (size, refresh) = match value.split_once('@') {
            Some((size, refresh)) => (size, Some(refresh)),
            None => (value, None),
        };
        let (w, h) = size.split_once('x')?;
        let width: u32 = w.parse().ok()?;
        let height: u32 = h.parse().ok()?;
        let refresh_mhz = match refresh {
            Some(r) => (r.parse::<f64>().ok()? * 1000.0).round() as u32,
            // Kanshi accepts a mode without a rate; the protocol does too.
            None => 0,
        };
        Some(Self {
            width,
            height,
            refresh_mhz,
            preferred: false,
            current: false,
            custom: false,
        })
    }

    /// Logical size after scale and rotation.
    pub fn logical_size(&self, scale: f64, transform: Transform) -> (i32, i32) {
        let (w, h) = if transform.rotates() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        };
        let scale = if scale > 0.0 { scale } else { 1.0 };
        (
            (f64::from(w) / scale).round() as i32,
            (f64::from(h) / scale).round() as i32,
        )
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format!(
            "{}x{}@{}",
            self.width,
            self.height,
            self.refresh_string()
        ))
    }
}

/// A connected head as reported by the compositor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub make: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub serial: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub modes: Vec<Mode>,
    pub position: (i32, i32),
    pub scale: f64,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default)]
    pub adaptive_sync: bool,
}

impl Output {
    pub fn is_internal(&self) -> bool {
        is_internal_name(&self.name)
    }

    /// Stable identity of the physical head, independent of the connector
    /// name. Built from the EDID make/model/serial the compositor reports, so
    /// remembered settings survive a dock or reboot that renames `DP-1` to
    /// `DP-3`.
    ///
    /// Returns `None` when the compositor reports no model and no serial: a
    /// make alone is not enough to tell two monitors apart.
    pub fn identity(&self) -> Option<String> {
        fn clean(value: Option<&str>) -> Option<String> {
            let value = value?.trim();
            if value.is_empty() {
                return None;
            }
            let cleaned: String = value
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect();
            let cleaned = cleaned.trim_matches('-').to_string();
            (!cleaned.is_empty()).then_some(cleaned)
        }

        let make = clean(self.make.as_deref());
        let model = clean(self.model.as_deref());
        let serial = clean(self.serial.as_deref());
        if model.is_none() && serial.is_none() {
            return None;
        }
        Some(
            [make, model, serial]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("|"),
        )
    }

    pub fn current_mode(&self) -> Option<&Mode> {
        self.modes
            .iter()
            .find(|m| m.current)
            .or_else(|| self.modes.iter().find(|m| m.preferred))
            .or_else(|| self.modes.first())
    }

    /// The currently active mode, falling back to the preferred one.
    pub fn active_mode(&self) -> Mode {
        self.current_mode()
            .cloned()
            .unwrap_or_else(|| Mode::new(0, 0, 0))
    }

    pub fn find_mode(&self, mode: &Mode) -> Option<&Mode> {
        self.modes.iter().find(|m| m.same_timing(mode)).or_else(|| {
            // Kanshi drops the refresh when only a size was requested.
            if mode.refresh_mhz == 0 {
                self.modes.iter().find(|m| m.same_size(mode))
            } else {
                None
            }
        })
    }
}

/// Lower resolutions to offer for an output that exposes a single real size.
///
/// Many laptop panels advertise only their native mode. Compositors still
/// accept a custom mode at a lower size (`--custom-mode` in wlr-randr, kanshi
/// `mode --custom`), so add a handful of sizes that keep the panel's aspect
/// ratio and every refresh rate of the native mode.
///
/// Outputs that already expose more than one size are left alone: their own
/// mode list is the better source of truth.
pub fn derived_modes(output: &Output) -> Vec<Mode> {
    let Some(native) = output.modes.iter().find(|m| m.preferred).or_else(|| {
        output
            .modes
            .iter()
            .max_by_key(|m| u64::from(m.width) * u64::from(m.height))
    }) else {
        return Vec::new();
    };

    // A custom mode is reported as current only while it is active, so ignore
    // that one when deciding whether this is a single-resolution output.
    let mut sizes: Vec<(u32, u32)> = output
        .modes
        .iter()
        .filter(|m| !(m.current && !m.same_size(native)))
        .map(|m| (m.width, m.height))
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    if sizes.len() != 1 {
        return Vec::new();
    }

    let mut rates: Vec<u32> = output
        .modes
        .iter()
        .filter(|m| m.same_size(native))
        .map(|m| m.refresh_mhz)
        .filter(|rate| *rate > 0)
        .collect();
    rates.sort_unstable_by(|a, b| b.cmp(a));
    rates.dedup();
    if rates.is_empty() {
        rates.push(60_000);
    }

    let mut modes = Vec::new();
    for (numerator, denominator) in [(3u64, 4u64), (2, 3), (1, 2), (2, 5), (1, 3)] {
        let width = round_even(u64::from(native.width) * numerator / denominator);
        let height = round_even(u64::from(native.height) * numerator / denominator);
        if width < 320 || height < 200 || width >= native.width || height >= native.height {
            continue;
        }
        for rate in &rates {
            let mut mode = Mode::new(width, height, *rate);
            mode.custom = true;
            modes.push(mode);
        }
    }
    modes
}

fn round_even(value: u64) -> u32 {
    let value = value + value % 2;
    value.min(u64::from(u32::MAX)) as u32
}

/// Connector-name heuristic for the laptop panel.
pub fn is_internal_name(name: &str) -> bool {
    name.starts_with("eDP-") || name.starts_with("LVDS-") || name.starts_with("DSI-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_formatting() {
        let mut m = Mode::new(3120, 2080, 120_000);
        assert_eq!(m.refresh_string(), "120");
        assert_eq!(m.to_kanshi(), "3120x2080@120");
        m.refresh_mhz = 59_951;
        assert_eq!(m.refresh_string(), "59.951");
        m.refresh_mhz = 60_000;
        assert_eq!(m.refresh_string(), "60");
    }

    #[test]
    fn kanshi_mode_roundtrip() {
        for text in ["3120x2080@120", "1920x1200@59.951", "1920x1080"] {
            let parsed = Mode::parse_kanshi(text).expect("parse");
            assert_eq!(parsed.to_kanshi(), text);
        }
    }

    #[test]
    fn logical_sizes_rotate() {
        let m = Mode::new(3120, 2080, 120_000);
        assert_eq!(m.logical_size(2.0, Transform::Normal), (1560, 1040));
        assert_eq!(m.logical_size(2.0, Transform::Rotate90), (1040, 1560));
        assert_eq!(
            Mode::new(1920, 1200, 60_000).logical_size(1.5, Transform::Normal),
            (1280, 800)
        );
    }

    #[test]
    fn identity_comes_from_edid_not_the_connector_name() {
        let mut first = output_with_modes("DP-1", vec![Mode::new(1920, 1080, 60_000)]);
        first.make = Some("Dell Inc.".into());
        first.model = Some("DELL U2720Q".into());
        first.serial = Some("ABC123".into());
        assert_eq!(
            first.identity().as_deref(),
            Some("dell-inc|dell-u2720q|abc123")
        );

        // The same physical head under another connector name keeps its id.
        let mut renamed = first.clone();
        renamed.name = "DP-7".into();
        assert_eq!(first.identity(), renamed.identity());

        // Make alone is not enough to identify a head, and an empty identity
        // is no identity at all.
        let mut make_only = first.clone();
        make_only.model = None;
        make_only.serial = None;
        assert_eq!(make_only.identity(), None);
        let mut empty = first;
        empty.make = Some("  ".into());
        empty.model = Some("".into());
        empty.serial = None;
        assert_eq!(empty.identity(), None);
    }

    #[test]
    fn internal_detection() {
        assert!(is_internal_name("eDP-1"));
        assert!(is_internal_name("LVDS-1"));
        assert!(!is_internal_name("DP-1"));
        assert!(!is_internal_name("HDMI-A-1"));
    }

    fn output_with_modes(name: &str, modes: Vec<Mode>) -> Output {
        Output {
            name: name.into(),
            description: String::new(),
            make: None,
            model: None,
            serial: None,
            enabled: true,
            modes,
            position: (0, 0),
            scale: 1.0,
            transform: Transform::Normal,
            adaptive_sync: false,
        }
    }

    #[test]
    fn derives_lower_resolutions_for_a_single_mode_panel() {
        let mut native = Mode::new(3120, 2080, 120_000);
        native.preferred = true;
        native.current = true;
        let output = output_with_modes("eDP-1", vec![native, Mode::new(3120, 2080, 60_000)]);
        let modes = derived_modes(&output);
        assert!(!modes.is_empty());
        assert!(modes.iter().all(|mode| mode.custom));
        assert!(modes.iter().all(|mode| mode.width < 3120));
        assert!(modes
            .iter()
            .any(|mode| mode.width == 2340 && mode.height == 1560 && mode.refresh_mhz == 120_000));
        assert!(modes
            .iter()
            .any(|mode| mode.width == 2340 && mode.height == 1560 && mode.refresh_mhz == 60_000));
        assert!(modes
            .iter()
            .any(|mode| mode.width == 1560 && mode.height == 1040));
    }

    #[test]
    fn keeps_deriving_while_a_custom_mode_is_current() {
        let mut native = Mode::new(3120, 2080, 120_000);
        native.preferred = true;
        let mut custom = Mode::new(1920, 1280, 120_000);
        custom.current = true;
        let output = output_with_modes("eDP-1", vec![native, custom]);
        let modes = derived_modes(&output);
        assert!(modes
            .iter()
            .any(|mode| mode.width == 2340 && mode.height == 1560));
    }

    #[test]
    fn does_not_derive_for_a_multi_mode_output() {
        let mut native = Mode::new(1920, 1200, 60_000);
        native.preferred = true;
        native.current = true;
        let output = output_with_modes("HDMI-A-1", vec![native, Mode::new(1600, 900, 60_000)]);
        assert!(derived_modes(&output).is_empty());
    }
}
