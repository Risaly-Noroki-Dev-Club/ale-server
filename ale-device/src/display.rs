//! Display / desktop session discovery.
//!
//! Owner: capture teammate. The public API below is the contract used by
//! `capture.rs`, `session.rs` and the server; keep the signatures stable.
//!
//! Discovery shells out to external tools (`xrandr`, `xdpyinfo`, `wlr-randr`,
//! `swaymsg`, `system_profiler`) so this crate never links X11/Wayland. Every
//! parser tolerates truncated or unexpected tool output and simply contributes
//! no displays; [`list_displays`] then tries the next probe and finally reports
//! [`crate::error::DeviceError::Unavailable`] with the attempts it made.
//!
//! Probe order by session: Wayland -> `wlr-randr`, `swaymsg`, `xrandr`,
//! `xdpyinfo`; X11 -> `xrandr`, `xdpyinfo`; macOS -> `system_profiler` (a
//! synthetic display derived from the captured image is the documented fallback
//! in `capture.rs` when discovery fails).
//!
//! The `parse_*` helpers below are public only so integration tests can feed
//! captured tool output to them; they are not part of the frozen API contract.

use crate::capture::{command_exists, run_command};
use crate::config::CaptureConfig;
use crate::error::{DeviceError, Result};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    /// Stable index within one discovery result (0-based, primary first).
    pub id: u32,
    pub name: String,
    /// Position of the display's top-left corner in the virtual desktop.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub primary: bool,
    /// Backend that reported the display (`xrandr`, `wlr-randr`, ...).
    pub backend: String,
}

impl DisplayInfo {
    pub fn contains_point(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x as f64 + self.width as f64)
            && y < (self.y as f64 + self.height as f64)
    }

    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    X11,
    Wayland,
    MacOs,
    Unknown,
}

impl SessionKind {
    /// Detect the graphics session from the environment.
    pub fn detect() -> Self {
        Self::detect_from_env(|key| std::env::var(key).ok())
    }

    /// Environment-injectable variant of [`SessionKind::detect`].
    ///
    /// Public (hidden) so integration tests can exercise the detection rules
    /// without mutating the process environment. Not part of the frozen API.
    #[doc(hidden)]
    pub fn detect_from_env<F>(get_env: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let non_empty = |key: &str| get_env(key).filter(|value| !value.trim().is_empty());

        if let Some(session) = non_empty("XDG_SESSION_TYPE") {
            match session.trim().to_ascii_lowercase().as_str() {
                "wayland" => return Self::Wayland,
                "x11" => return Self::X11,
                _ => {}
            }
        }

        // The macOS dev build always uses `screencapture`; an XQuartz `DISPLAY`
        // is irrelevant there.
        if cfg!(target_os = "macos") {
            return Self::MacOs;
        }

        if non_empty("WAYLAND_DISPLAY").is_some() {
            return Self::Wayland;
        }
        if non_empty("DISPLAY").is_some() {
            return Self::X11;
        }
        Self::Unknown
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplaySet {
    pub displays: Vec<DisplayInfo>,
    /// Backend used for discovery.
    pub backend: String,
}

impl DisplaySet {
    pub fn primary(&self) -> Option<&DisplayInfo> {
        self.displays.iter().find(|display| display.primary)
    }

    pub fn display(&self, id: u32) -> Option<&DisplayInfo> {
        self.displays.iter().find(|display| display.id == id)
    }

    pub fn total_bounds(&self) -> Option<(i32, i32, u32, u32)> {
        let first = self.displays.first()?;
        let mut min_x = first.x;
        let mut min_y = first.y;
        let mut max_x = first.x.saturating_add(first.width as i32);
        let mut max_y = first.y.saturating_add(first.height as i32);
        for display in &self.displays[1..] {
            min_x = min_x.min(display.x);
            min_y = min_y.min(display.y);
            max_x = max_x.max(display.x.saturating_add(display.width as i32));
            max_y = max_y.max(display.y.saturating_add(display.height as i32));
        }
        Some((min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32))
    }
}

/// Discover connected displays. Must never panic; if discovery fails the caller
/// falls back to a single synthetic display derived from the captured image.
pub fn list_displays(cfg: &CaptureConfig) -> Result<DisplaySet> {
    let session = SessionKind::detect();
    let timeout = Duration::from_millis(cfg.timeout_ms.clamp(200, 60_000));
    let mut attempts: Vec<String> = Vec::new();

    for probe in probes_for(session) {
        let program = probe.program();
        if !command_exists(program) {
            attempts.push(format!("{program}: not installed"));
            continue;
        }

        let mut command = Command::new(program);
        command.args(probe.args());
        match run_command(command, timeout) {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let displays = normalize_displays(probe.parse(&text));
                if displays.is_empty() {
                    attempts.push(format!(
                        "{program}: command succeeded but reported no connected displays"
                    ));
                    continue;
                }
                return Ok(DisplaySet {
                    displays,
                    backend: probe.backend().to_string(),
                });
            }
            Ok(output) => attempts.push(format!("{program}: exited with {}", output.status)),
            Err(DeviceError::Timeout) => attempts.push(format!(
                "{program}: timed out after {} ms",
                timeout.as_millis()
            )),
            Err(error) => attempts.push(format!("{program}: {error}")),
        }
    }

    let details = if attempts.is_empty() {
        "no discovery command was attempted".to_string()
    } else {
        attempts.join("; ")
    };
    Err(DeviceError::Unavailable(format!(
        "could not discover displays for a {session:?} session ({details}); \
         install xrandr (X11) or wlr-randr/swaymsg (Wayland). Screenshots still work: \
         frames fall back to a synthetic display derived from the captured image"
    )))
}

/// Resolve which display a capture request refers to. `None` means the whole
/// virtual desktop or the primary display, depending on backend support.
// The explicit lifetimes are part of the frozen public signature.
#[allow(clippy::needless_lifetimes)]
pub fn select_display<'a>(set: &'a DisplaySet, requested: Option<u32>) -> Option<&'a DisplayInfo> {
    match requested {
        Some(id) => set.display(id),
        None => set.primary(),
    }
}

// ---------------------------------------------------------------------------
// Discovery probes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayProbe {
    Xrandr,
    Xdpyinfo,
    WlrRandr,
    Swaymsg,
    SystemProfiler,
}

impl DisplayProbe {
    fn program(self) -> &'static str {
        match self {
            Self::Xrandr => "xrandr",
            Self::Xdpyinfo => "xdpyinfo",
            Self::WlrRandr => "wlr-randr",
            Self::Swaymsg => "swaymsg",
            Self::SystemProfiler => "system_profiler",
        }
    }

    fn args(self) -> &'static [&'static str] {
        match self {
            Self::Xrandr => &["--query"],
            Self::Swaymsg => &["-t", "get_outputs", "-r"],
            Self::SystemProfiler => &["SPDisplaysDataType"],
            Self::Xdpyinfo | Self::WlrRandr => &[],
        }
    }

    fn backend(self) -> &'static str {
        match self {
            Self::Xrandr => "xrandr",
            Self::Xdpyinfo => "xdpyinfo",
            Self::WlrRandr => "wlr-randr",
            Self::Swaymsg => "swaymsg",
            Self::SystemProfiler => "system_profiler",
        }
    }

    fn parse(self, output: &str) -> Vec<DisplayInfo> {
        match self {
            Self::Xrandr => parse_xrandr_query(output),
            Self::Xdpyinfo => parse_xdpyinfo(output),
            Self::WlrRandr => parse_wlr_randr(output),
            Self::Swaymsg => parse_swaymsg_outputs(output),
            Self::SystemProfiler => parse_system_profiler_displays(output),
        }
    }
}

fn probes_for(session: SessionKind) -> &'static [DisplayProbe] {
    match session {
        SessionKind::Wayland => &[
            DisplayProbe::WlrRandr,
            DisplayProbe::Swaymsg,
            DisplayProbe::Xrandr,
            DisplayProbe::Xdpyinfo,
        ],
        SessionKind::X11 => &[DisplayProbe::Xrandr, DisplayProbe::Xdpyinfo],
        SessionKind::MacOs => &[DisplayProbe::SystemProfiler],
        SessionKind::Unknown => &[
            DisplayProbe::WlrRandr,
            DisplayProbe::Swaymsg,
            DisplayProbe::Xrandr,
            DisplayProbe::Xdpyinfo,
        ],
    }
}

/// Drop unusable entries, guarantee a primary display and renumber ids with the
/// primary first, so callers can rely on `id == 0` being the primary display.
fn normalize_displays(mut displays: Vec<DisplayInfo>) -> Vec<DisplayInfo> {
    displays.retain(|display| {
        display.width > 0 && display.height > 0 && !display.name.trim().is_empty()
    });
    if displays.is_empty() {
        return displays;
    }
    if !displays.iter().any(|display| display.primary) {
        if let Some(first) = displays.first_mut() {
            first.primary = true;
        }
    }
    displays.sort_by(|a, b| {
        b.primary
            .cmp(&a.primary)
            .then_with(|| a.x.cmp(&b.x))
            .then_with(|| a.y.cmp(&b.y))
            .then_with(|| a.name.cmp(&b.name))
    });
    for (index, display) in displays.iter_mut().enumerate() {
        display.id = index as u32;
    }
    displays
}

// ---------------------------------------------------------------------------
// Parsers (public only for integration tests)
// ---------------------------------------------------------------------------

/// Parse the output of `xrandr --query`.
///
/// Only lines for `connected` outputs carry a `WxH+X+Y` geometry token; mode
/// lines are indented and ignored. Disconnected outputs are skipped.
#[doc(hidden)]
pub fn parse_xrandr_query(output: &str) -> Vec<DisplayInfo> {
    let mut displays = Vec::new();
    for line in output.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        if tokens.get(1) != Some(&"connected") {
            continue;
        }
        let name = tokens[0].trim();
        if name.is_empty() {
            continue;
        }
        let primary = tokens[2..].contains(&"primary");
        let geometry = tokens[2..]
            .iter()
            .find_map(|token| parse_xrandr_geometry(token));
        let Some((width, height, x, y)) = geometry else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        displays.push(DisplayInfo {
            id: 0,
            name: name.to_string(),
            x,
            y,
            width,
            height,
            scale_factor: 1.0,
            primary,
            backend: "xrandr".to_string(),
        });
    }
    displays
}

/// Parse `WxH+X+Y` as printed by xrandr. Offsets are signed; xrandr renders a
/// negative offset as `+-1920`, which this parser accepts as `-1920`.
fn parse_xrandr_geometry(token: &str) -> Option<(u32, u32, i32, i32)> {
    let (width_text, rest) = token.split_once('x')?;
    if width_text.is_empty() || !width_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let width: u32 = width_text.parse().ok()?;

    let height_end = rest.find(['+', '-'])?;
    let height: u32 = rest[..height_end].parse().ok()?;

    let tail = &rest[height_end..];
    if !tail.starts_with(['+', '-']) {
        return None;
    }
    let (x, tail) = parse_signed_prefix(tail)?;
    if !tail.starts_with(['+', '-']) {
        return None;
    }
    let (y, _tail) = parse_signed_prefix(tail)?;
    Some((width, height, x, y))
}

fn parse_signed_prefix(value: &str) -> Option<(i32, &str)> {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut sign: i64 = 1;
    if bytes.get(index) == Some(&b'+') {
        index += 1;
    }
    if bytes.get(index) == Some(&b'-') {
        sign = -1;
        index += 1;
    }
    let start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == start {
        return None;
    }
    let magnitude: i64 = value[start..index].parse().ok()?;
    Some(((magnitude * sign) as i32, &value[index..]))
}

/// Parse the `dimensions: WxH pixels (...)` line of `xdpyinfo`.
#[doc(hidden)]
pub fn parse_xdpyinfo(output: &str) -> Vec<DisplayInfo> {
    let mut name: Option<String> = None;
    let mut dimensions: Option<(u32, u32)> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name of display:") {
            let value = rest.trim();
            if !value.is_empty() {
                name = Some(value.to_string());
            }
        }
        if let Some(rest) = trimmed.strip_prefix("dimensions:") {
            let token = rest.split_whitespace().next().unwrap_or_default();
            if let Some((width, height)) = token.split_once('x') {
                if let (Ok(width), Ok(height)) = (width.parse::<u32>(), height.parse::<u32>()) {
                    if width > 0 && height > 0 {
                        dimensions = Some((width, height));
                        break;
                    }
                }
            }
        }
    }
    match dimensions {
        Some((width, height)) => vec![DisplayInfo {
            id: 0,
            name: name.unwrap_or_else(|| "screen".to_string()),
            x: 0,
            y: 0,
            width,
            height,
            scale_factor: 1.0,
            primary: true,
            backend: "xdpyinfo".to_string(),
        }],
        None => Vec::new(),
    }
}

/// Parse `wlr-randr` output (one block per output).
#[doc(hidden)]
pub fn parse_wlr_randr(output: &str) -> Vec<DisplayInfo> {
    #[derive(Default)]
    struct Builder {
        name: String,
        enabled: Option<bool>,
        current_mode: Option<(u32, u32)>,
        fallback_mode: Option<(u32, u32)>,
        position: (i32, i32),
        scale: f32,
        in_modes: bool,
    }

    fn parse_mode(line: &str) -> Option<(u32, u32)> {
        let token = line.split_whitespace().next()?;
        let (width, height) = token.split_once('x')?;
        let width: u32 = width.parse().ok()?;
        let height: u32 = height.parse().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some((width, height))
    }

    let mut builders: Vec<Builder> = Vec::new();
    let mut current: Option<Builder> = None;
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            if let Some(builder) = current.take() {
                builders.push(builder);
            }
            let name = line.split_whitespace().next().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            current = Some(Builder {
                name: name.to_string(),
                scale: 1.0,
                ..Builder::default()
            });
            continue;
        }
        let Some(builder) = current.as_mut() else {
            continue;
        };
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Enabled:") {
            builder.enabled = Some(rest.trim().eq_ignore_ascii_case("yes"));
        } else if trimmed.starts_with("Modes:") {
            builder.in_modes = true;
        } else if let Some(rest) = trimmed.strip_prefix("Position:") {
            let mut parts = rest.split(',');
            if let (Some(x), Some(y)) = (parts.next(), parts.next()) {
                if let (Ok(x), Ok(y)) = (x.trim().parse::<i32>(), y.trim().parse::<i32>()) {
                    builder.position = (x, y);
                }
            }
        } else if let Some(rest) = trimmed.strip_prefix("Scale:") {
            if let Ok(scale) = rest.trim().parse::<f32>() {
                if scale.is_finite() && scale > 0.0 {
                    builder.scale = scale;
                }
            }
        } else if builder.in_modes {
            if let Some(mode) = parse_mode(trimmed) {
                if trimmed.contains("current") {
                    builder.current_mode = Some(mode);
                } else if builder.fallback_mode.is_none() {
                    builder.fallback_mode = Some(mode);
                }
            }
        }
    }
    if let Some(builder) = current.take() {
        builders.push(builder);
    }

    builders
        .into_iter()
        .filter_map(|builder| {
            if builder.enabled == Some(false) {
                return None;
            }
            let (width, height) = builder.current_mode.or(builder.fallback_mode)?;
            Some(DisplayInfo {
                id: 0,
                name: builder.name,
                x: builder.position.0,
                y: builder.position.1,
                width,
                height,
                scale_factor: builder.scale,
                primary: false,
                backend: "wlr-randr".to_string(),
            })
        })
        .collect()
}

/// Parse `swaymsg -t get_outputs` JSON.
#[doc(hidden)]
pub fn parse_swaymsg_outputs(json: &str) -> Vec<DisplayInfo> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(outputs) = value.as_array() else {
        return Vec::new();
    };

    let mut displays = Vec::new();
    for output in outputs {
        if output.get("active").and_then(|value| value.as_bool()) == Some(false) {
            continue;
        }
        let name = output
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("output")
            .to_string();
        let rect = output.get("rect");
        let mut width = rect
            .and_then(|rect| rect.get("width"))
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let mut height = rect
            .and_then(|rect| rect.get("height"))
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        if width == 0 || height == 0 {
            if let Some(mode) = output.get("current_mode") {
                width = mode
                    .get("width")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(width);
                height = mode
                    .get("height")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(height);
            }
        }
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            continue;
        };
        if width == 0 || height == 0 {
            continue;
        }
        let x = rect
            .and_then(|rect| rect.get("x"))
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(0);
        let y = rect
            .and_then(|rect| rect.get("y"))
            .and_then(|value| value.as_i64())
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or(0);
        let scale = output
            .get("scale")
            .and_then(|value| value.as_f64())
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .map_or(1.0, |scale| scale as f32);
        let primary = output
            .get("primary")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        displays.push(DisplayInfo {
            id: 0,
            name,
            x,
            y,
            width,
            height,
            scale_factor: scale,
            primary,
            backend: "swaymsg".to_string(),
        });
    }
    displays
}

/// Parse `system_profiler SPDisplaysDataType` (macOS dev fallback).
#[doc(hidden)]
pub fn parse_system_profiler_displays(output: &str) -> Vec<DisplayInfo> {
    struct Pending {
        width: u32,
        height: u32,
        primary: bool,
        online: bool,
    }

    fn flush(pending: &mut Option<Pending>, displays: &mut Vec<DisplayInfo>, index: &mut u32) {
        let Some(pending) = pending.take() else {
            return;
        };
        if !pending.online {
            return;
        }
        displays.push(DisplayInfo {
            id: 0,
            name: format!("display-{index}"),
            x: 0,
            y: 0,
            width: pending.width,
            height: pending.height,
            scale_factor: 1.0,
            primary: pending.primary,
            backend: "system_profiler".to_string(),
        });
        *index += 1;
    }

    let mut displays = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut index = 0u32;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Resolution:") {
            flush(&mut pending, &mut displays, &mut index);
            let mut numbers = rest
                .split_whitespace()
                .filter_map(|token| token.parse::<u32>().ok());
            if let (Some(width), Some(height)) = (numbers.next(), numbers.next()) {
                if width > 0 && height > 0 {
                    pending = Some(Pending {
                        width,
                        height,
                        primary: false,
                        online: true,
                    });
                }
            }
        } else if trimmed.eq_ignore_ascii_case("Main Display: Yes") {
            if let Some(pending) = pending.as_mut() {
                pending.primary = true;
            }
        } else if trimmed.eq_ignore_ascii_case("Online: No") {
            if let Some(pending) = pending.as_mut() {
                pending.online = false;
            }
        }
    }
    flush(&mut pending, &mut displays, &mut index);
    displays
}
