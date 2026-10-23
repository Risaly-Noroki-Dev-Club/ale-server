//! Display discovery parser tests: feeds captured external-tool output to the
//! pure parsers and checks environment-based session detection. These tests
//! never spawn a tool or need a display server.

use ale_device::display::{
    parse_swaymsg_outputs, parse_system_profiler_displays, parse_wlr_randr, parse_xdpyinfo,
    parse_xrandr_query, select_display, DisplayInfo, DisplaySet, SessionKind,
};

const XRANDR_QUERY: &str = "\
Screen 0: minimum 8 x 8, current 4480 x 1440, maximum 32767 x 32767
eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 344mm x 194mm
   1920x1080     60.00*+  59.97    59.96    50.00
   1680x1050     59.95    59.88
HDMI-1 disconnected (normal left inverted right x axis y axis)
DP-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 597mm x 336mm
   2560x1440     59.95 +
";

#[test]
fn parses_xrandr_connected_displays_with_geometry_and_primary() {
    let displays = parse_xrandr_query(XRANDR_QUERY);
    assert_eq!(displays.len(), 2, "disconnected outputs must be skipped");

    let laptop = &displays[0];
    assert_eq!(laptop.name, "eDP-1");
    assert!(laptop.primary);
    assert_eq!((laptop.width, laptop.height), (1920, 1080));
    assert_eq!((laptop.x, laptop.y), (0, 0));
    assert_eq!(laptop.scale_factor, 1.0);
    assert_eq!(laptop.backend, "xrandr");

    let monitor = &displays[1];
    assert_eq!(monitor.name, "DP-1");
    assert!(!monitor.primary);
    assert_eq!((monitor.width, monitor.height), (2560, 1440));
    assert_eq!((monitor.x, monitor.y), (1920, 0));
}

#[test]
fn parses_xrandr_negative_offsets() {
    let output = "\
Screen 0: minimum 8 x 8, current 4480 x 1080, maximum 32767 x 32767
DP-2 connected 1920x1080+-1920+-200 (normal left inverted right x axis y axis) 344mm x 194mm
";
    let displays = parse_xrandr_query(output);
    assert_eq!(displays.len(), 1);
    assert_eq!((displays[0].x, displays[0].y), (-1920, -200));
    assert_eq!((displays[0].width, displays[0].height), (1920, 1080));
}

#[test]
fn ignores_disconnected_and_geometryless_xrandr_output() {
    let output = "\
Screen 0: minimum 8 x 8, current 0 x 0, maximum 32767 x 32767
eDP-1 connected primary (normal left inverted right x axis y axis) 344mm x 194mm
HDMI-1 disconnected
HDMI-2 unknown connection
garbage
";
    assert!(parse_xrandr_query(output).is_empty());
    assert!(parse_xrandr_query("").is_empty());
    assert!(parse_xrandr_query("Screen 0:\n not an output line\n").is_empty());
}

const WLR_RANDR: &str = "\
DP-1 \"Dell Inc. DELL U2720Q (DP-1)\"
  Make: Dell Inc.
  Model: DELL U2720Q
  Physical size: 600x340 mm
  Enabled: yes
  Modes:
    3840x2160 px, 60.000000 Hz (preferred, current)
    1920x1080 px, 60.000000 Hz
  Position: 0,0
  Transform: normal
  Scale: 1.500000
HDMI-A-1 \"LG Electronics LG ULTRAGEAR (HDMI-A-1)\"
  Enabled: yes
  Modes:
    2560x1440 px, 144.000000 Hz (current)
    1920x1080 px, 60.000000 Hz
  Position: 3840,0
  Scale: 1.000000
HDMI-A-2 \"Unknown\"
  Enabled: no
  Modes:
    1920x1080 px, 60.000000 Hz (current)
  Position: 6400,0
  Scale: 1.000000
";

#[test]
fn parses_wlr_randr_enabled_outputs() {
    let displays = parse_wlr_randr(WLR_RANDR);
    assert_eq!(displays.len(), 2, "disabled outputs must be skipped");

    let dell = &displays[0];
    assert_eq!(dell.name, "DP-1");
    assert_eq!((dell.width, dell.height), (3840, 2160));
    assert_eq!((dell.x, dell.y), (0, 0));
    assert_eq!(dell.scale_factor, 1.5);
    assert_eq!(dell.backend, "wlr-randr");

    let lg = &displays[1];
    assert_eq!(lg.name, "HDMI-A-1");
    assert_eq!((lg.width, lg.height), (2560, 1440));
    assert_eq!((lg.x, lg.y), (3840, 0));
    assert_eq!(lg.scale_factor, 1.0);
}

#[test]
fn wlr_randr_without_enabled_or_current_mode_still_parses_or_is_skipped() {
    // No "Enabled:" line: the output is assumed enabled when a mode is reported.
    let no_enabled = "\
DP-3 \"Generic\"
  Modes:
    1920x1200 px, 60.000000 Hz
  Position: -100,0
  Scale: 1.000000
";
    let displays = parse_wlr_randr(no_enabled);
    assert_eq!(displays.len(), 1);
    assert_eq!((displays[0].width, displays[0].height), (1920, 1200));
    assert_eq!((displays[0].x, displays[0].y), (-100, 0));

    assert!(parse_wlr_randr("garbage\n  Enabled: yes\n").is_empty());
}

const SWAYMSG_OUTPUTS: &str = r#"[
  {
    "name": "eDP-1",
    "active": true,
    "primary": true,
    "scale": 2.0,
    "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
    "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
    "focused": true
  },
  {
    "name": "HDMI-A-1",
    "active": true,
    "primary": false,
    "scale": 1.0,
    "rect": {"x": 1920, "y": 0, "width": 2560, "height": 1440},
    "current_mode": {"width": 2560, "height": 1440, "refresh": 144000}
  },
  {
    "name": "DP-2",
    "active": false,
    "primary": false,
    "rect": {"x": 0, "y": 0, "width": 0, "height": 0}
  }
]"#;

#[test]
fn parses_swaymsg_json_outputs() {
    let displays = parse_swaymsg_outputs(SWAYMSG_OUTPUTS);
    assert_eq!(displays.len(), 2, "inactive outputs must be skipped");

    let internal = &displays[0];
    assert_eq!(internal.name, "eDP-1");
    assert!(internal.primary);
    assert_eq!(internal.scale_factor, 2.0);
    assert_eq!((internal.width, internal.height), (1920, 1080));
    assert_eq!(internal.backend, "swaymsg");

    let external = &displays[1];
    assert_eq!(external.name, "HDMI-A-1");
    assert_eq!((external.x, external.y), (1920, 0));
    assert_eq!((external.width, external.height), (2560, 1440));
}

#[test]
fn tolerates_malformed_swaymsg_json() {
    assert!(parse_swaymsg_outputs("not json").is_empty());
    assert!(parse_swaymsg_outputs("{}").is_empty());
    assert!(parse_swaymsg_outputs("").is_empty());
    assert!(
        parse_swaymsg_outputs(r#"[{"name": "X", "rect": {"width": 0, "height": 0}}]"#).is_empty()
    );
}

#[test]
fn parses_xdpyinfo_dimensions() {
    let output = "\
name of display:    :0
version number:    11.0
vendor string:    The X.Org Foundation
dimensions:    1920x1080 pixels (508x285 millimeters)
depths (7):    24, 1, 4, 8, 15, 16, 32
";
    let displays = parse_xdpyinfo(output);
    assert_eq!(displays.len(), 1);
    assert_eq!(displays[0].name, ":0");
    assert_eq!((displays[0].width, displays[0].height), (1920, 1080));
    assert!(displays[0].primary);
    assert_eq!(displays[0].backend, "xdpyinfo");

    assert!(parse_xdpyinfo("version number: 11.0\n").is_empty());
}

const SYSTEM_PROFILER: &str = "\
Graphics/Displays:

    Apple M2:

      Chipset Model: Apple M2
      Type: GPU
      Displays:
        Color LCD:
          Display Type: Built-In Liquid Retina XDR Display
          Resolution: 3456 x 2234 Retina
          Main Display: Yes
          Mirror: Off
          Online: Yes

    DisplayLink:

      Displays:
        DELL U2720Q:
          Resolution: 3840 x 2160
          Main Display: No
          Mirror: Off
          Online: Yes
";

#[test]
fn parses_system_profiler_displays_on_macos_fallback() {
    let displays = parse_system_profiler_displays(SYSTEM_PROFILER);
    assert_eq!(displays.len(), 2);
    assert_eq!((displays[0].width, displays[0].height), (3456, 2234));
    assert!(displays[0].primary);
    assert_eq!((displays[1].width, displays[1].height), (3840, 2160));
    assert!(!displays[1].primary);
    assert_eq!(displays[0].backend, "system_profiler");
}

fn env_lookup<'a>(vars: &'a [(&'a str, &'a str)], key: &str) -> Option<String> {
    vars.iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| (*value).to_string())
}

#[test]
fn session_detection_prefers_xdg_session_type() {
    let wayland = [("XDG_SESSION_TYPE", "wayland")];
    assert_eq!(
        SessionKind::detect_from_env(|key| env_lookup(&wayland, key)),
        SessionKind::Wayland
    );

    let x11 = [("XDG_SESSION_TYPE", "X11")];
    assert_eq!(
        SessionKind::detect_from_env(|key| env_lookup(&x11, key)),
        SessionKind::X11
    );

    let mixed_case = [("XDG_SESSION_TYPE", "  Wayland ")];
    assert_eq!(
        SessionKind::detect_from_env(|key| env_lookup(&mixed_case, key)),
        SessionKind::Wayland
    );
}

#[test]
fn session_detection_falls_back_to_display_variables() {
    // XDG_SESSION_TYPE is often unset in ssh/headless environments.
    let unknown_xdg = [("XDG_SESSION_TYPE", "tty")];
    let detected = SessionKind::detect_from_env(|key| env_lookup(&unknown_xdg, key));
    if cfg!(target_os = "macos") {
        assert_eq!(detected, SessionKind::MacOs);
    } else {
        assert_eq!(detected, SessionKind::Unknown);
    }

    let no_env: [(&str, &str); 0] = [];
    let detected = SessionKind::detect_from_env(|key| env_lookup(&no_env, key));
    if cfg!(target_os = "macos") {
        assert_eq!(detected, SessionKind::MacOs);
    } else {
        assert_eq!(detected, SessionKind::Unknown);
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn session_detection_uses_wayland_display_then_display_on_linux() {
    let wayland = [("WAYLAND_DISPLAY", "wayland-0")];
    assert_eq!(
        SessionKind::detect_from_env(|key| env_lookup(&wayland, key)),
        SessionKind::Wayland
    );

    let x11 = [("DISPLAY", ":0")];
    assert_eq!(
        SessionKind::detect_from_env(|key| env_lookup(&x11, key)),
        SessionKind::X11
    );
}

fn display(id: u32, name: &str, x: i32, primary: bool) -> DisplayInfo {
    DisplayInfo {
        id,
        name: name.to_string(),
        x,
        y: 0,
        width: 1920,
        height: 1080,
        scale_factor: 1.0,
        primary,
        backend: "test".to_string(),
    }
}

#[test]
fn display_set_helpers_and_selection() {
    let set = DisplaySet {
        displays: vec![
            display(0, "primary", 0, true),
            display(1, "right", 1920, false),
        ],
        backend: "test".to_string(),
    };
    assert_eq!(
        set.primary().map(|display| display.name.as_str()),
        Some("primary")
    );
    assert_eq!(
        set.display(1).map(|display| display.name.as_str()),
        Some("right")
    );
    assert_eq!(set.total_bounds(), Some((0, 0, 3840, 1080)));

    assert_eq!(
        select_display(&set, None).map(|display| display.name.as_str()),
        Some("primary")
    );
    assert_eq!(
        select_display(&set, Some(1)).map(|display| display.name.as_str()),
        Some("right")
    );
    assert!(select_display(&set, Some(7)).is_none());
}
