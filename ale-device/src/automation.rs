//! Guarded execution of automation action plans.
//!
//! Owner: automation teammate. Backends are external commands (`xdotool`,
//! `ydotool`, `dotool`, `wtype`) so no native input libraries are linked.
//!
//! # Security invariants (enforced here, not only in the session layer)
//!
//! * `plan.validate()` from `ale_core::actions` always runs first;
//! * action count never exceeds `max_actions`;
//! * risk is always re-derived from the actions (`inferred_risk`); the
//!   caller-provided `plan.risk_level` is never trusted;
//! * high-risk actions (`CloseApp`, `FileOperation`) require `allow_high_risk`;
//! * `OpenApp`/`OpenUrl` require `allow_process_actions`;
//! * `ControlledTestClick` is always rejected (Windows-only desktop feature);
//! * plans that need confirmation are rejected unless `approved == true`;
//! * coordinates must be finite and inside the discovered virtual desktop
//!   (with a small tolerance, see below);
//! * every action checks `ExecutionControl` and aborts promptly on cancel;
//! * commands are bounded by `timeout_ms` and their process is killed on
//!   cancellation or timeout;
//! * `dry_run` never spawns a process: no backend command and no display probe;
//! * logs contain action kinds and counts only - never typed text, key
//!   combinations, URLs or coordinates.
//!
//! # Guard pipeline (`validate_plan`)
//!
//! The checks run in this order and the first failure wins:
//!
//! 1. `ActionPlan::validate()` (empty plan, per-action limits, finite numbers);
//! 2. `actions.len() <= automation.max_actions`;
//! 3. inferred risk `>= High` requires `automation.allow_high_risk`;
//! 4. `OpenApp`/`OpenUrl` require `automation.allow_process_actions`;
//! 5. `ControlledTestClick` is always denied;
//! 6. `plan.requires_confirmation || inferred >= Medium` requires
//!    `approved == true`;
//! 7. coordinate-bearing actions must be inside the cached virtual-desktop
//!    bounds (when bounds could be discovered).
//!
//! # Backend command matrix
//!
//! Coordinates are rounded to the nearest integer pixel. `Click` first moves
//! the pointer to the coordinate and then clicks; `DoubleClick` moves and then
//! clicks twice; `Scroll` moves and then emits wheel clicks.
//!
//! | Action | `xdotool` (X11) | `ydotool` (Wayland) | `dotool` | `wtype` |
//! | --- | --- | --- | --- | --- |
//! | `MouseMove {x,y}` | `mousemove --sync x y` | `mousemove --absolute -x x -y y` | stdin `mouseto x y` | unsupported |
//! | `Click {x,y,button}` | move; `click --clearmodifiers N` | move; `click 0xCN` | stdin `mouseto` + `click B` | unsupported |
//! | `DoubleClick {x,y}` | move; `click --clearmodifiers --repeat 2 1` | move; `click 0xC0` twice | stdin move + `click left` twice | unsupported |
//! | `Scroll {x,y,dx,dy}` | move; `click --clearmodifiers --repeat N 4\|5` for vertical and `6\|7` for horizontal | move; `mousemove --wheel -x dx -y dy` | stdin `mouseto` + `wheel N` (vertical only) | unsupported |
//! | `Type {text}` | `type --clearmodifiers -- text` | `type text` (or `wtype -- text` when detected) | stdin `type text` (or `wtype`) | `wtype -- text` |
//! | `Key {key,mods}` | `key --clearmodifiers mod1+mod2+key` | `key code:1 ... code:0` (evdev key codes) | stdin `key mod+key` | `wtype -M mod -k key -m mod` |
//!
//! xdotool mouse button numbers: `1` left, `2` middle, `3` right; scroll wheel
//! buttons `4` up, `5` down, `6` left, `7` right. ydotool button codes follow
//! ydotool's convention: `0xC0` left, `0xC1` right, `0xC2` middle. Wheel steps
//! are `round(abs(delta))`, at least 1 and at most [`MAX_WHEEL_STEPS`]; positive
//! `delta_y` scrolls down and positive `delta_x` scrolls right.
//!
//! `wtype` is only used as a fallback text tool when it is available on a
//! Wayland session, or when it is selected explicitly through
//! `automation.preferred_backend` (it cannot move or click the pointer).
//! `dotool` receives one command script per action on stdin.
//!
//! # Backend detection
//!
//! The backend is detected once in [`AutomationService::new`] and cached.
//! `automation.preferred_backend` may be a bare command name (looked up on
//! `PATH`) or a path (used as-is, which lets tests inject a fake backend).
//! Otherwise the session type is derived from `XDG_SESSION_TYPE`,
//! `WAYLAND_DISPLAY` and `DISPLAY` and the first installed backend from
//! `xdotool`, `ydotool`, `dotool` is used.
//!
//! # Coordinates and the virtual desktop
//!
//! Display bounds are discovered once at startup through `xrandr --query`
//! (X11), `wlr-randr` or `swaymsg -t get_outputs -r` (Wayland). Coordinates may
//! be up to [`BOUNDS_TOLERANCE_PX`] pixels outside the discovered bounds (to
//! absorb backend rounding and scale-factor slack). If the bounds cannot be
//! discovered, the bounds check is skipped; invisible failures must not block
//! automation on unusual sessions.
//!
//! For headless and test rigs the discovery can be overridden with
//! `ALE_AUTOMATION_VIRTUAL_DESKTOP="x,y,width,height"`; the special value
//! `off` (or an empty value) disables the bounds check.
//!
//! # Config-gated process and file actions
//!
//! * `OpenUrl`: only `http://` and `https://` URLs; opened with `xdg-open`, or
//!   `gio open` when `xdg-open` is not installed;
//! * `OpenApp`: name must match `[A-Za-z0-9._-]+`; opened with `xdg-open`, or
//!   `gio launch` when `xdg-open` is not installed;
//! * `CloseApp`: name must match `[A-Za-z0-9._-]+`; executed as
//!   `pkill -x -- name`;
//! * `FileOperation`: always denied by the executor for now. This version
//!   deliberately does not mutate the file system; the guard pipeline still
//!   requires `allow_high_risk` before the action is even considered.

use crate::config::AutomationConfig;
use crate::error::{DeviceError, Result};
use ale_core::actions::{Action, ActionPlan, MouseButton, RiskLevel};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Coordinates up to this many pixels outside the virtual desktop are accepted.
const BOUNDS_TOLERANCE_PX: f64 = 4.0;
/// Maximum number of wheel clicks emitted for one `Scroll` action.
const MAX_WHEEL_STEPS: u32 = 20;
/// Poll interval while a backend child process runs.
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Slice used for interruptible `Wait` actions.
const WAIT_SLICE: Duration = Duration::from_millis(20);
/// Timeout for one display-discovery probe.
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(750);
/// Environment override for the virtual desktop: `x,y,width,height` or `off`.
const VIRTUAL_DESKTOP_ENV: &str = "ALE_AUTOMATION_VIRTUAL_DESKTOP";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutomationCapabilities {
    pub available: bool,
    pub backend: Option<String>,
    pub dry_run: bool,
    pub allow_high_risk: bool,
    pub allow_process_actions: bool,
    pub error: Option<String>,
}

/// Cooperative cancellation shared between the session layer and the executor.
#[derive(Debug, Clone, Default)]
pub struct ExecutionControl {
    cancelled: Arc<AtomicBool>,
}

impl ExecutionControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(DeviceError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionOutcome {
    pub actions_executed: usize,
    /// `true` when execution stopped early because of cancellation.
    pub cancelled: bool,
    pub dry_run: bool,
}

pub struct AutomationService {
    cfg: AutomationConfig,
    backend: Option<Backend>,
    /// Why no backend could be detected (exposed through `capabilities`).
    backend_error: Option<String>,
    /// Cached virtual-desktop bounding box `(x, y, width, height)`.
    bounds: Option<VirtualBounds>,
}

impl AutomationService {
    pub fn new(cfg: AutomationConfig) -> Self {
        let (backend, backend_error) = match detect_backend(&cfg) {
            Ok(backend) => (Some(backend), None),
            Err(error) => (None, Some(error)),
        };
        // `dry_run` must not touch the system at all: probing the display
        // server would spawn processes, so bounds discovery is skipped.
        let bounds = if cfg.dry_run {
            None
        } else {
            discover_virtual_bounds()
        };
        Self {
            cfg,
            backend,
            backend_error,
            bounds,
        }
    }

    pub fn config(&self) -> &AutomationConfig {
        &self.cfg
    }

    /// Detected backend name, if the current session can execute input.
    pub fn backend_name(&self) -> Option<String> {
        self.backend
            .as_ref()
            .map(|backend| backend.kind.name().to_string())
    }

    pub fn capabilities(&self) -> AutomationCapabilities {
        let available = self.cfg.dry_run || self.backend.is_some();
        AutomationCapabilities {
            available,
            backend: self.backend_name(),
            dry_run: self.cfg.dry_run,
            allow_high_risk: self.cfg.allow_high_risk,
            allow_process_actions: self.cfg.allow_process_actions,
            error: self.backend_error.clone(),
        }
    }

    /// Validate a plan against the configured policy. Must be pure and cheap;
    /// the session layer calls it before executing anything.
    pub fn validate_plan(&self, plan: &ActionPlan, approved: bool) -> Result<()> {
        // 1. Structural validation owned by ale-core (also rejects non-finite
        //    coordinates, oversized text and excessive waits).
        plan.validate()
            .map_err(|error| DeviceError::Denied(format!("invalid action plan: {error}")))?;

        // 2. Our own, possibly stricter, action-count limit.
        if plan.actions.len() > self.cfg.max_actions {
            return Err(DeviceError::Denied(format!(
                "plan has {} actions; automation.max_actions is {}",
                plan.actions.len(),
                self.cfg.max_actions
            )));
        }

        // 3. Risk is re-derived; `plan.risk_level` is untrusted model output.
        let risk = inferred_risk(plan);
        if risk >= RiskLevel::High && !self.cfg.allow_high_risk {
            return Err(DeviceError::Denied(
                "high-risk actions are disabled (automation.allow_high_risk)".into(),
            ));
        }

        // 4. Process actions are gated separately from risk.
        if !self.cfg.allow_process_actions {
            for action in &plan.actions {
                if matches!(action, Action::OpenApp { .. } | Action::OpenUrl { .. }) {
                    return Err(DeviceError::Denied(
                        "opening applications or URLs is disabled (automation.allow_process_actions)"
                            .into(),
                    ));
                }
            }
        }

        // 5. `ControlledTestClick` is a Windows-only desktop test feature and is
        //    never available in the headless service.
        if plan
            .actions
            .iter()
            .any(|action| matches!(action, Action::ControlledTestClick { .. }))
        {
            return Err(DeviceError::Denied(
                "ControlledTestClick is not supported by the headless device service".into(),
            ));
        }

        // 6. Approval gate: neither a medium-or-higher inferred risk nor the
        //    plan's confirmation flag can be bypassed by a missing approval.
        if (plan.requires_confirmation || risk >= RiskLevel::Medium) && !approved {
            return Err(DeviceError::Denied(
                "automation plan requires explicit user approval".into(),
            ));
        }

        // 7. Bounds check, only when the virtual desktop was discoverable.
        if let Some(bounds) = self.bounds {
            for action in &plan.actions {
                let (x, y) = match action {
                    Action::Click { x, y, .. }
                    | Action::DoubleClick { x, y }
                    | Action::MouseMove { x, y }
                    | Action::Scroll { x, y, .. } => (*x, *y),
                    _ => continue,
                };
                if !bounds.contains_with_tolerance(x, y) {
                    return Err(DeviceError::Denied(format!(
                        "coordinate ({x}, {y}) is outside the virtual desktop \
                         ({}x{} at {}, {}; tolerance {BOUNDS_TOLERANCE_PX} px)",
                        bounds.width, bounds.height, bounds.x, bounds.y
                    )));
                }
            }
        }

        Ok(())
    }

    /// Execute an already approved plan. Call from `spawn_blocking`.
    pub fn execute(
        &self,
        plan: &ActionPlan,
        approved: bool,
        control: &ExecutionControl,
    ) -> Result<ActionOutcome> {
        // Defence in depth: execution never relies on the caller having run the
        // guards, even though the session layer validates first.
        self.validate_plan(plan, approved)?;
        self.execute_validated(plan, control)
    }

    /// Convenience used by the session layer: validate then execute.
    pub fn execute_guarded(
        &self,
        plan: &ActionPlan,
        approved: bool,
        control: &ExecutionControl,
    ) -> Result<ActionOutcome> {
        self.execute(plan, approved, control)
    }

    fn execute_validated(
        &self,
        plan: &ActionPlan,
        control: &ExecutionControl,
    ) -> Result<ActionOutcome> {
        // A request cancelled before it started is reported as a cancelled
        // outcome rather than an error: nothing was executed.
        if control.is_cancelled() {
            return Ok(self.cancelled_outcome(0));
        }

        let deadline = Instant::now() + Duration::from_millis(self.cfg.timeout_ms);
        info!(
            target: "ale_device::automation",
            actions = plan.actions.len(),
            risk = ?inferred_risk(plan),
            dry_run = self.cfg.dry_run,
            "automation plan started"
        );
        let mut actions_executed = 0usize;
        for action in &plan.actions {
            if control.is_cancelled() {
                return Ok(self.cancelled_outcome(actions_executed));
            }
            if Instant::now() >= deadline {
                return Err(DeviceError::Timeout);
            }

            if self.cfg.dry_run {
                debug!(
                    target: "ale_device::automation",
                    action = action_kind(action),
                    "dry-run action; nothing is executed"
                );
            } else {
                let result = match action {
                    Action::Wait { ms } => self.wait_interruptible(*ms, control, deadline)?,
                    other => self.execute_action(other, control, deadline)?,
                };
                if result == StepResult::Cancelled {
                    return Ok(self.cancelled_outcome(actions_executed));
                }
            }

            actions_executed += 1;
            if control.is_cancelled() {
                return Ok(self.cancelled_outcome(actions_executed));
            }
        }
        if control.is_cancelled() {
            return Ok(self.cancelled_outcome(actions_executed));
        }

        info!(
            target: "ale_device::automation",
            actions = actions_executed,
            dry_run = self.cfg.dry_run,
            "automation plan finished"
        );
        Ok(ActionOutcome {
            actions_executed,
            cancelled: false,
            dry_run: self.cfg.dry_run,
        })
    }

    fn cancelled_outcome(&self, actions_executed: usize) -> ActionOutcome {
        ActionOutcome {
            actions_executed,
            cancelled: true,
            dry_run: self.cfg.dry_run,
        }
    }

    /// Sleep for `ms`, but wake up to check cancellation and the overall
    /// deadline every [`WAIT_SLICE`].
    fn wait_interruptible(
        &self,
        ms: u64,
        control: &ExecutionControl,
        deadline: Instant,
    ) -> Result<StepResult> {
        let target = Instant::now() + Duration::from_millis(ms);
        loop {
            if control.is_cancelled() {
                return Ok(StepResult::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(DeviceError::Timeout);
            }
            if now >= target {
                return Ok(StepResult::Completed);
            }
            let remaining = target.saturating_duration_since(now);
            std::thread::sleep(remaining.min(WAIT_SLICE));
        }
    }

    fn execute_action(
        &self,
        action: &Action,
        control: &ExecutionControl,
        deadline: Instant,
    ) -> Result<StepResult> {
        match action {
            Action::OpenUrl { url } => {
                let url = validate_url(url)?;
                let invocation = opener_invocation(url, "open")?;
                run_invocation(&invocation, control, deadline)
            }
            Action::OpenApp { name } => {
                let name = validate_application_name(name)?;
                let invocation = opener_invocation(name, "launch")?;
                run_invocation(&invocation, control, deadline)
            }
            Action::CloseApp { name } => {
                let name = validate_application_name(name)?;
                let invocation = Invocation::argv(Path::new("pkill"), &["-x", "--", name]);
                run_invocation(&invocation, control, deadline)
            }
            Action::FileOperation { .. } => Err(DeviceError::Denied(
                "file operations are not supported by this version of the device service".into(),
            )),
            Action::ControlledTestClick { .. } => Err(DeviceError::Denied(
                "ControlledTestClick is not supported by the headless device service".into(),
            )),
            Action::Wait { .. } => Err(DeviceError::Backend(
                "internal error: Wait actions must be handled by the executor".into(),
            )),
            other => {
                let backend = self.backend.as_ref().ok_or_else(|| {
                    DeviceError::Unavailable(
                        self.backend_error
                            .clone()
                            .unwrap_or_else(|| "no automation backend is available".to_string()),
                    )
                })?;
                for invocation in invocations_for(backend, other)? {
                    match run_invocation(&invocation, control, deadline)? {
                        StepResult::Completed => {}
                        StepResult::Cancelled => return Ok(StepResult::Cancelled),
                    }
                }
                Ok(StepResult::Completed)
            }
        }
    }
}

/// Highest risk found in a plan, ignoring the caller-supplied `risk_level`.
pub fn inferred_risk(plan: &ActionPlan) -> RiskLevel {
    plan.actions
        .iter()
        .map(Action::risk_level)
        .max()
        .unwrap_or(RiskLevel::Low)
}

// ---------------------------------------------------------------------------
// Backend detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Xdotool,
    Ydotool,
    Dotool,
    Wtype,
}

impl BackendKind {
    fn name(self) -> &'static str {
        match self {
            Self::Xdotool => "xdotool",
            Self::Ydotool => "ydotool",
            Self::Dotool => "dotool",
            Self::Wtype => "wtype",
        }
    }
}

#[derive(Debug, Clone)]
struct Backend {
    kind: BackendKind,
    /// Executable used for pointer/keyboard actions.
    program: PathBuf,
    /// Optional standalone Wayland text tool (`wtype`).
    text_program: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnvironment {
    X11,
    Wayland,
    Unknown,
}

fn detect_session_environment() -> SessionEnvironment {
    match std::env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "wayland" => return SessionEnvironment::Wayland,
        "x11" => return SessionEnvironment::X11,
        _ => {}
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()) {
        return SessionEnvironment::Wayland;
    }
    if std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty()) {
        return SessionEnvironment::X11;
    }
    SessionEnvironment::Unknown
}

/// Detect the backend once; `preferred_backend` wins over auto-detection.
fn detect_backend(cfg: &AutomationConfig) -> std::result::Result<Backend, String> {
    if let Some(preferred) = cfg
        .preferred_backend
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let program = resolve_backend_program(preferred)?;
        let kind = backend_kind_from_name(preferred).ok_or_else(|| {
            format!(
                "cannot determine the automation backend type of '{preferred}' \
                 (expected xdotool, ydotool, dotool or wtype)"
            )
        })?;
        return Ok(Backend {
            kind,
            program,
            text_program: None,
        });
    }

    let session = detect_session_environment();
    let candidates: &[&str] = match session {
        SessionEnvironment::X11 => &["xdotool", "ydotool", "dotool"],
        SessionEnvironment::Wayland => &["ydotool", "dotool", "xdotool"],
        SessionEnvironment::Unknown => &["xdotool", "ydotool", "dotool"],
    };
    for candidate in candidates {
        if let Some(program) = find_in_path(candidate) {
            let kind = backend_kind_from_name(candidate)
                .ok_or_else(|| "internal automation backend table error".to_string())?;
            let text_program = match kind {
                BackendKind::Ydotool | BackendKind::Dotool => find_in_path("wtype"),
                _ => None,
            };
            return Ok(Backend {
                kind,
                program,
                text_program,
            });
        }
    }
    Err(match session {
        SessionEnvironment::X11 => {
            "no automation backend found; install xdotool (recommended for X11)".to_string()
        }
        SessionEnvironment::Wayland => {
            "no automation backend found; install ydotool or dotool (and wtype for typing)"
                .to_string()
        }
        SessionEnvironment::Unknown => {
            "no automation backend found; install xdotool, ydotool or dotool".to_string()
        }
    })
}

/// `preferred_backend` may be a bare name (looked up on `PATH`) or a path
/// (absolute paths are how tests inject fake backends).
fn resolve_backend_program(preferred: &str) -> std::result::Result<PathBuf, String> {
    if preferred.contains('/') {
        let path = PathBuf::from(preferred);
        if is_executable_file(&path) {
            Ok(path)
        } else {
            Err(format!(
                "preferred automation backend '{preferred}' is not an executable file"
            ))
        }
    } else {
        find_in_path(preferred).ok_or_else(|| {
            format!("preferred automation backend '{preferred}' was not found on PATH")
        })
    }
}

fn backend_kind_from_name(name: &str) -> Option<BackendKind> {
    let path = Path::new(name);
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(file_name);
    for candidate in [file_name, stem] {
        match candidate.to_ascii_lowercase().as_str() {
            "xdotool" => return Some(BackendKind::Xdotool),
            "ydotool" => return Some(BackendKind::Ydotool),
            "dotool" => return Some(BackendKind::Dotool),
            "wtype" => return Some(BackendKind::Wtype),
            _ => {}
        }
    }
    None
}

/// Resolve a command name against `PATH`, or accept a path-like value as-is.
fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable_file(&path).then_some(path);
    }
    let path_value = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_value) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ---------------------------------------------------------------------------
// Virtual desktop bounds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
struct VirtualBounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl VirtualBounds {
    fn contains_with_tolerance(&self, x: f64, y: f64) -> bool {
        if !x.is_finite() || !y.is_finite() {
            return false;
        }
        let min_x = self.x - BOUNDS_TOLERANCE_PX;
        let min_y = self.y - BOUNDS_TOLERANCE_PX;
        let max_x = self.x + self.width - 1.0 + BOUNDS_TOLERANCE_PX;
        let max_y = self.y + self.height - 1.0 + BOUNDS_TOLERANCE_PX;
        x >= min_x && y >= min_y && x <= max_x && y <= max_y
    }
}

/// Discover the virtual desktop bounding box once; `None` means "unknown, skip
/// the bounds check".
fn discover_virtual_bounds() -> Option<VirtualBounds> {
    if let Ok(raw) = std::env::var(VIRTUAL_DESKTOP_ENV) {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("off") {
            return None;
        }
        match parse_bounds_override(raw) {
            Some(bounds) => return Some(bounds),
            None => warn!(
                target: "ale_device::automation",
                "ignoring invalid ALE_AUTOMATION_VIRTUAL_DESKTOP value; probing the display server"
            ),
        }
    }

    let probe = |program: &str, args: &[&str], parser: fn(&str) -> Option<VirtualBounds>| {
        run_bounded_output(program, args, DISCOVERY_TIMEOUT).and_then(|output| parser(&output))
    };
    let wayland = detect_session_environment() == SessionEnvironment::Wayland;
    if wayland {
        probe("wlr-randr", &[], parse_wlr_randr_bounds)
            .or_else(|| {
                probe(
                    "swaymsg",
                    &["-t", "get_outputs", "-r"],
                    parse_swaymsg_bounds,
                )
            })
            .or_else(|| probe("xrandr", &["--query"], parse_xrandr_bounds))
    } else {
        probe("xrandr", &["--query"], parse_xrandr_bounds)
            .or_else(|| probe("wlr-randr", &[], parse_wlr_randr_bounds))
            .or_else(|| {
                probe(
                    "swaymsg",
                    &["-t", "get_outputs", "-r"],
                    parse_swaymsg_bounds,
                )
            })
    }
}

fn parse_bounds_override(raw: &str) -> Option<VirtualBounds> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return None;
    }
    let x = parts[0].parse::<f64>().ok()?;
    let y = parts[1].parse::<f64>().ok()?;
    let width = parts[2].parse::<f64>().ok()?;
    let height = parts[3].parse::<f64>().ok()?;
    if !x.is_finite() || !y.is_finite() || !width.is_finite() || !height.is_finite() {
        return None;
    }
    if width < 1.0 || height < 1.0 || width > 1_000_000.0 || height > 1_000_000.0 {
        return None;
    }
    Some(VirtualBounds {
        x,
        y,
        width,
        height,
    })
}

/// `xrandr --query`: `Screen 0: minimum 320 x 200, current 1920 x 1080, ...`.
fn parse_xrandr_bounds(output: &str) -> Option<VirtualBounds> {
    for line in output.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(index) = tokens.iter().position(|token| *token == "current") else {
            continue;
        };
        if tokens.get(index + 2) != Some(&"x") {
            continue;
        }
        let Some(width) = tokens.get(index + 1) else {
            continue;
        };
        let Some(height) = tokens.get(index + 3) else {
            continue;
        };
        let Ok(width) = width.trim_end_matches(',').parse::<f64>() else {
            continue;
        };
        let Ok(height) = height.trim_end_matches(',').parse::<f64>() else {
            continue;
        };
        if width >= 1.0 && height >= 1.0 {
            return Some(VirtualBounds {
                x: 0.0,
                y: 0.0,
                width,
                height,
            });
        }
    }
    None
}

/// `swaymsg -t get_outputs -r`: JSON array with `rect: {x,y,width,height}`.
fn parse_swaymsg_bounds(output: &str) -> Option<VirtualBounds> {
    let value: serde_json::Value = serde_json::from_str(output).ok()?;
    let outputs = value.as_array()?;
    let mut rectangles = Vec::new();
    for output in outputs {
        if output.get("active").and_then(serde_json::Value::as_bool) == Some(false) {
            continue;
        }
        let rect = output.get("rect")?;
        let (Some(x), Some(y), Some(width), Some(height)) = (
            rect.get("x").and_then(serde_json::Value::as_f64),
            rect.get("y").and_then(serde_json::Value::as_f64),
            rect.get("width").and_then(serde_json::Value::as_f64),
            rect.get("height").and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        if width >= 1.0 && height >= 1.0 {
            rectangles.push((x, y, width, height));
        }
    }
    bounds_from_rectangles(rectangles)
}

/// `wlr-randr`: one block per output with `Position:` and a current mode line.
fn parse_wlr_randr_bounds(output: &str) -> Option<VirtualBounds> {
    let mut blocks = Vec::new();
    let mut block = String::new();
    for line in output.lines() {
        if !line.is_empty() && !line.starts_with(char::is_whitespace) && !block.is_empty() {
            blocks.push(std::mem::take(&mut block));
        }
        block.push_str(line);
        block.push('\n');
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    let rectangles: Vec<(f64, f64, f64, f64)> = blocks
        .iter()
        .filter_map(|block| parse_wlr_output_block(block))
        .collect();
    bounds_from_rectangles(rectangles)
}

fn parse_wlr_output_block(block: &str) -> Option<(f64, f64, f64, f64)> {
    let mut enabled = true;
    let mut position = None;
    let mut current_mode = None;
    let mut resolution = None;
    for line in block.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Position:") {
            position = parse_number_pair(rest);
        } else if let Some(rest) = trimmed.strip_prefix("Enabled:") {
            enabled = rest.trim().eq_ignore_ascii_case("yes");
        }
        if let Some(rest) = trimmed.strip_prefix("Current mode:") {
            current_mode = parse_resolution(rest);
        } else if trimmed.contains("current") {
            if let Some(value) = parse_resolution(trimmed) {
                resolution = Some(value);
            }
        }
    }
    if !enabled {
        return None;
    }
    let (x, y) = position?;
    let (width, height) = current_mode.or(resolution)?;
    Some((x, y, width, height))
}

fn parse_number_pair(text: &str) -> Option<(f64, f64)> {
    let (left, right) = text.trim().split_once(',')?;
    Some((
        left.trim().parse::<f64>().ok()?,
        right.trim().parse::<f64>().ok()?,
    ))
}

/// Find the first `WIDTHxHEIGHT` token in a line.
fn parse_resolution(text: &str) -> Option<(f64, f64)> {
    for token in text.split_whitespace() {
        let token = token.trim_end_matches(',');
        let Some((width, height)) = token.split_once('x') else {
            continue;
        };
        let (Ok(width), Ok(height)) = (width.parse::<f64>(), height.parse::<f64>()) else {
            continue;
        };
        if width >= 1.0 && height >= 1.0 {
            return Some((width, height));
        }
    }
    None
}

fn bounds_from_rectangles(rectangles: Vec<(f64, f64, f64, f64)>) -> Option<VirtualBounds> {
    if rectangles.is_empty() {
        return None;
    }
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (x, y, width, height) in rectangles {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + width);
        max_y = max_y.max(y + height);
    }
    let width = max_x - min_x;
    let height = max_y - min_y;
    if width < 1.0 || height < 1.0 {
        return None;
    }
    Some(VirtualBounds {
        x: min_x,
        y: min_y,
        width,
        height,
    })
}

/// Run a discovery command with a bounded timeout and captured stdout.
fn run_bounded_output(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        let _ = sender.send(bytes);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = receiver.recv_timeout(Duration::from_millis(250)).ok()?;
    String::from_utf8(bytes).ok()
}

// ---------------------------------------------------------------------------
// Argument generation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Invocation {
    program: PathBuf,
    args: Vec<String>,
    stdin_text: Option<String>,
}

impl Invocation {
    fn argv(program: &Path, args: &[&str]) -> Self {
        Self {
            program: program.to_path_buf(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            stdin_text: None,
        }
    }

    fn args(program: &Path, args: Vec<String>) -> Self {
        Self {
            program: program.to_path_buf(),
            args,
            stdin_text: None,
        }
    }

    fn stdin(program: &Path, script: String) -> Self {
        Self {
            program: program.to_path_buf(),
            args: Vec::new(),
            stdin_text: Some(script),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepResult {
    Completed,
    Cancelled,
}

fn invocations_for(backend: &Backend, action: &Action) -> Result<Vec<Invocation>> {
    match backend.kind {
        BackendKind::Xdotool => xdotool_invocations(&backend.program, action),
        BackendKind::Ydotool => ydotool_invocations(backend, action),
        BackendKind::Dotool => dotool_invocations(backend, action),
        BackendKind::Wtype => wtype_invocations(&backend.program, action),
    }
}

fn xdotool_invocations(program: &Path, action: &Action) -> Result<Vec<Invocation>> {
    fn move_to(program: &Path, x: f64, y: f64) -> Invocation {
        Invocation::args(
            program,
            vec![
                "mousemove".to_string(),
                "--sync".to_string(),
                coordinate(x),
                coordinate(y),
            ],
        )
    }
    fn click_with_repeat(program: &Path, repeat: u32, button: &str) -> Invocation {
        if repeat <= 1 {
            Invocation::argv(program, &["click", "--clearmodifiers", button])
        } else {
            Invocation::args(
                program,
                vec![
                    "click".to_string(),
                    "--clearmodifiers".to_string(),
                    "--repeat".to_string(),
                    repeat.to_string(),
                    button.to_string(),
                ],
            )
        }
    }

    match action {
        Action::MouseMove { x, y } => Ok(vec![move_to(program, *x, *y)]),
        Action::Click { x, y, button } => Ok(vec![
            move_to(program, *x, *y),
            click_with_repeat(program, 1, xdotool_button(*button)),
        ]),
        Action::DoubleClick { x, y } => Ok(vec![
            move_to(program, *x, *y),
            click_with_repeat(program, 2, "1"),
        ]),
        Action::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            let mut invocations = vec![move_to(program, *x, *y)];
            let vertical = wheel_steps(*delta_y);
            if vertical > 0 {
                let button = if *delta_y > 0.0 { "5" } else { "4" };
                invocations.push(click_with_repeat(program, vertical, button));
            }
            let horizontal = wheel_steps(*delta_x);
            if horizontal > 0 {
                let button = if *delta_x > 0.0 { "7" } else { "6" };
                invocations.push(click_with_repeat(program, horizontal, button));
            }
            Ok(invocations)
        }
        Action::Type { text } => Ok(vec![Invocation::argv(
            program,
            &["type", "--clearmodifiers", "--", text],
        )]),
        Action::Key { key, modifiers } => {
            let combo = xdotool_combo(key, modifiers)?;
            Ok(vec![Invocation::argv(
                program,
                &["key", "--clearmodifiers", &combo],
            )])
        }
        _ => Err(DeviceError::Denied(
            "action is not supported by the xdotool backend".into(),
        )),
    }
}

fn ydotool_invocations(backend: &Backend, action: &Action) -> Result<Vec<Invocation>> {
    let program = backend.program.as_path();
    fn move_to(program: &Path, x: f64, y: f64) -> Invocation {
        Invocation::args(
            program,
            vec![
                "mousemove".to_string(),
                "--absolute".to_string(),
                "-x".to_string(),
                coordinate(x),
                "-y".to_string(),
                coordinate(y),
            ],
        )
    }
    fn click(program: &Path, code: &str) -> Invocation {
        Invocation::argv(program, &["click", code])
    }

    match action {
        Action::MouseMove { x, y } => Ok(vec![move_to(program, *x, *y)]),
        Action::Click { x, y, button } => Ok(vec![
            move_to(program, *x, *y),
            click(program, ydotool_button(*button)),
        ]),
        Action::DoubleClick { x, y } => Ok(vec![
            move_to(program, *x, *y),
            click(program, "0xC0"),
            click(program, "0xC0"),
        ]),
        Action::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            let mut invocations = vec![move_to(program, *x, *y)];
            // Linux wheel axes use REL_WHEEL/REL_HWHEEL: positive vertical means
            // scroll up, while `Action::Scroll` uses positive `delta_y` for down.
            let horizontal = signed_wheel_steps(*delta_x);
            let vertical = -signed_wheel_steps(*delta_y);
            if horizontal != 0 || vertical != 0 {
                invocations.push(Invocation::args(
                    program,
                    vec![
                        "mousemove".to_string(),
                        "--wheel".to_string(),
                        "-x".to_string(),
                        horizontal.to_string(),
                        "-y".to_string(),
                        vertical.to_string(),
                    ],
                ));
            }
            Ok(invocations)
        }
        Action::Type { text } => {
            if let Some(text_program) = backend.text_program.as_deref() {
                Ok(vec![wtype_text_invocation(text_program, text)])
            } else {
                Ok(vec![Invocation::argv(program, &["type", text])])
            }
        }
        Action::Key { key, modifiers } => {
            let mut args = vec!["key".to_string()];
            args.extend(ydotool_key_sequence(key, modifiers)?);
            Ok(vec![Invocation::args(program, args)])
        }
        _ => Err(DeviceError::Denied(
            "action is not supported by the ydotool backend".into(),
        )),
    }
}

fn dotool_invocations(backend: &Backend, action: &Action) -> Result<Vec<Invocation>> {
    let program = backend.program.as_path();
    let script = match action {
        Action::MouseMove { x, y } => format!("mouseto {} {}\n", coordinate(*x), coordinate(*y)),
        Action::Click { x, y, button } => format!(
            "mouseto {} {}\nclick {}\n",
            coordinate(*x),
            coordinate(*y),
            dotool_button(*button)
        ),
        Action::DoubleClick { x, y } => format!(
            "mouseto {} {}\nclick left\nclick left\n",
            coordinate(*x),
            coordinate(*y)
        ),
        Action::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            if *delta_x != 0.0 {
                return Err(DeviceError::Denied(
                    "horizontal scrolling is not supported by the dotool backend".into(),
                ));
            }
            format!(
                "mouseto {} {}\nwheel {}\n",
                coordinate(*x),
                coordinate(*y),
                -signed_wheel_steps(*delta_y)
            )
        }
        Action::Type { text } => {
            if let Some(text_program) = backend.text_program.as_deref() {
                return Ok(vec![wtype_text_invocation(text_program, text)]);
            }
            // `dotool` reads a command script from stdin; control characters
            // could inject an extra command, so they are rejected instead of
            // being typed.
            if text.chars().any(char::is_control) {
                return Err(DeviceError::Denied(
                    "the dotool backend cannot type newlines or control characters".into(),
                ));
            }
            format!("type {text}\n")
        }
        Action::Key { key, modifiers } => format!("key {}\n", xdotool_combo(key, modifiers)?),
        _ => {
            return Err(DeviceError::Denied(
                "action is not supported by the dotool backend".into(),
            ))
        }
    };
    Ok(vec![Invocation::stdin(program, script)])
}

fn wtype_invocations(program: &Path, action: &Action) -> Result<Vec<Invocation>> {
    match action {
        Action::Type { text } => Ok(vec![wtype_text_invocation(program, text)]),
        Action::Key { key, modifiers } => {
            let keysym = named_keysym(key)?;
            let mut args: Vec<String> = Vec::new();
            let mut pressed: Vec<&'static str> = Vec::new();
            for modifier in modifiers {
                let modifier = wtype_modifier(modifier)?;
                args.push("-M".to_string());
                args.push(modifier.to_string());
                pressed.push(modifier);
            }
            args.push("-k".to_string());
            args.push(keysym);
            for modifier in pressed.iter().rev() {
                args.push("-m".to_string());
                args.push((*modifier).to_string());
            }
            Ok(vec![Invocation::args(program, args)])
        }
        Action::MouseMove { .. }
        | Action::Click { .. }
        | Action::DoubleClick { .. }
        | Action::Scroll { .. } => Err(DeviceError::Backend(
            "the wtype backend cannot move the pointer or click; install xdotool (X11) \
             or ydotool/dotool (Wayland)"
                .into(),
        )),
        _ => Err(DeviceError::Denied(
            "action is not supported by the wtype backend".into(),
        )),
    }
}

fn wtype_text_invocation(program: &Path, text: &str) -> Invocation {
    Invocation::argv(program, &["--", text])
}

fn xdotool_button(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "1",
        MouseButton::Middle => "2",
        MouseButton::Right => "3",
    }
}

fn ydotool_button(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "0xC0",
        MouseButton::Right => "0xC1",
        MouseButton::Middle => "0xC2",
    }
}

fn dotool_button(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Middle => "middle",
        MouseButton::Right => "right",
    }
}

/// Rounded integer coordinate. The guard pipeline already rejected non-finite
/// and absurd values; saturating here avoids panics on `f64::INFINITY`.
fn coordinate(value: f64) -> String {
    (value.round() as i64).to_string()
}

/// Number of wheel clicks for one scroll delta.
fn wheel_steps(delta: f64) -> u32 {
    if !delta.is_finite() || delta == 0.0 {
        return 0;
    }
    let steps = delta.abs().round().max(1.0);
    steps.min(f64::from(MAX_WHEEL_STEPS)) as u32
}

/// Signed wheel steps: positive delta -> positive steps.
fn signed_wheel_steps(delta: f64) -> i64 {
    let steps = i64::from(wheel_steps(delta));
    if delta < 0.0 {
        -steps
    } else {
        steps
    }
}

fn modifier_name(name: &str) -> Result<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok("ctrl"),
        "alt" | "option" => Ok("alt"),
        "shift" => Ok("shift"),
        "super" | "meta" | "win" | "windows" | "cmd" | "command" => Ok("super"),
        _ => Err(DeviceError::Denied("unsupported key modifier".into())),
    }
}

fn wtype_modifier(name: &str) -> Result<&'static str> {
    let modifier = modifier_name(name)?;
    Ok(if modifier == "super" {
        "logo"
    } else {
        modifier
    })
}

/// xdotool/XKB keysym for a named key or single alphanumeric character.
fn named_keysym(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(DeviceError::Denied("key name must not be empty".into()));
    }
    let named = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some("Return"),
        "tab" => Some("Tab"),
        "space" => Some("space"),
        "backspace" => Some("BackSpace"),
        "delete" | "del" => Some("Delete"),
        "escape" | "esc" => Some("Escape"),
        "up" => Some("Up"),
        "down" => Some("Down"),
        "left" => Some("Left"),
        "right" => Some("Right"),
        "home" => Some("Home"),
        "end" => Some("End"),
        "pageup" | "page_up" => Some("Page_Up"),
        "pagedown" | "page_down" => Some("Page_Down"),
        "insert" | "ins" => Some("Insert"),
        "print" | "printscreen" | "print_screen" => Some("Print"),
        "menu" => Some("Menu"),
        "pause" => Some("Pause"),
        "capslock" | "caps_lock" => Some("Caps_Lock"),
        "f1" => Some("F1"),
        "f2" => Some("F2"),
        "f3" => Some("F3"),
        "f4" => Some("F4"),
        "f5" => Some("F5"),
        "f6" => Some("F6"),
        "f7" => Some("F7"),
        "f8" => Some("F8"),
        "f9" => Some("F9"),
        "f10" => Some("F10"),
        "f11" => Some("F11"),
        "f12" => Some("F12"),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named.to_string());
    }
    if key.chars().count() == 1
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return Ok(key.to_string());
    }
    Err(DeviceError::Denied("unsupported key name".into()))
}

fn xdotool_combo(key: &str, modifiers: &[String]) -> Result<String> {
    let mut combo = String::new();
    for modifier in modifiers {
        combo.push_str(modifier_name(modifier)?);
        combo.push('+');
    }
    combo.push_str(&named_keysym(key)?);
    Ok(combo)
}

fn ydotool_modifier_code(name: &str) -> Result<u16> {
    Ok(match modifier_name(name)? {
        "ctrl" => 29,
        "alt" => 56,
        "shift" => 42,
        "super" => 125,
        _ => return Err(DeviceError::Denied("unsupported key modifier".into())),
    })
}

/// Linux evdev key code for a named key or single character.
fn ydotool_keycode(key: &str) -> Result<u16> {
    let key = key.trim();
    if key.is_empty() {
        return Err(DeviceError::Denied("key name must not be empty".into()));
    }
    let named = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Some(28),
        "tab" => Some(15),
        "space" => Some(57),
        "backspace" => Some(14),
        "delete" | "del" => Some(111),
        "escape" | "esc" => Some(1),
        "up" => Some(103),
        "down" => Some(108),
        "left" => Some(105),
        "right" => Some(106),
        "home" => Some(102),
        "end" => Some(107),
        "pageup" | "page_up" => Some(104),
        "pagedown" | "page_down" => Some(109),
        "insert" | "ins" => Some(110),
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(87),
        "f12" => Some(88),
        _ => None,
    };
    if let Some(named) = named {
        return Ok(named);
    }
    if key.chars().count() == 1 {
        let character = key.chars().next().unwrap_or_default();
        if let Some(code) = evdev_character_code(character) {
            return Ok(code);
        }
    }
    Err(DeviceError::Denied("unsupported key name".into()))
}

fn evdev_character_code(character: char) -> Option<u16> {
    let code = match character.to_ascii_lowercase() {
        'a' => 30,
        'b' => 48,
        'c' => 46,
        'd' => 32,
        'e' => 18,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'i' => 23,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'm' => 50,
        'n' => 49,
        'o' => 24,
        'p' => 25,
        'q' => 16,
        'r' => 19,
        's' => 31,
        't' => 20,
        'u' => 22,
        'v' => 47,
        'w' => 17,
        'x' => 45,
        'y' => 21,
        'z' => 44,
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        '-' => 12,
        '=' => 13,
        '[' => 26,
        ']' => 27,
        ';' => 39,
        '\'' => 40,
        '`' => 41,
        '\\' => 43,
        ',' => 51,
        '.' => 52,
        '/' => 53,
        _ => return None,
    };
    Some(code)
}

/// Build the `ydotool key` argument sequence: press modifiers, press/release
/// the key, release modifiers in reverse order (`keycode:state` per ydotool).
fn ydotool_key_sequence(key: &str, modifiers: &[String]) -> Result<Vec<String>> {
    let mut modifier_codes = Vec::new();
    for modifier in modifiers {
        let code = ydotool_modifier_code(modifier)?;
        if !modifier_codes.contains(&code) {
            modifier_codes.push(code);
        }
    }
    let key = key.trim();
    let key_code = ydotool_keycode(key)?;
    // An uppercase letter needs an implicit shift press.
    if key.chars().count() == 1
        && key.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && !modifier_codes.contains(&42)
    {
        modifier_codes.push(42);
    }
    let mut sequence = Vec::with_capacity(modifier_codes.len() * 2 + 3);
    for code in &modifier_codes {
        sequence.push(format!("{code}:1"));
    }
    sequence.push(format!("{key_code}:1"));
    sequence.push(format!("{key_code}:0"));
    for code in modifier_codes.iter().rev() {
        sequence.push(format!("{code}:0"));
    }
    Ok(sequence)
}

// ---------------------------------------------------------------------------
// Config-gated process actions
// ---------------------------------------------------------------------------

fn validate_url(url: &str) -> Result<&str> {
    let url = url.trim();
    if url.is_empty() {
        return Err(DeviceError::Denied("URL must not be empty".into()));
    }
    if url.len() > 2_048 {
        return Err(DeviceError::Denied("URL is too long".into()));
    }
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(DeviceError::Denied(
            "only http:// and https:// URLs can be opened".into(),
        ));
    }
    if url
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(DeviceError::Denied(
            "URL must not contain whitespace or control characters".into(),
        ));
    }
    Ok(url)
}

fn validate_application_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(DeviceError::Denied(
            "application name must not be empty".into(),
        ));
    }
    if name.len() > 128 {
        return Err(DeviceError::Denied("application name is too long".into()));
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    {
        return Err(DeviceError::Denied(
            "application name may only contain letters, digits, '.', '_' and '-'".into(),
        ));
    }
    if name == "." || name == ".." {
        return Err(DeviceError::Denied("application name is not valid".into()));
    }
    Ok(name)
}

/// Open URLs/apps through `xdg-open`, falling back to `gio`.
fn opener_invocation(target: &str, gio_subcommand: &str) -> Result<Invocation> {
    if let Some(program) = find_in_path("xdg-open") {
        return Ok(Invocation::argv(&program, &[target]));
    }
    if let Some(program) = find_in_path("gio") {
        return Ok(Invocation::argv(&program, &[gio_subcommand, target]));
    }
    Err(DeviceError::Denied(
        "neither xdg-open nor gio is installed; process actions are unavailable".into(),
    ))
}

// ---------------------------------------------------------------------------
// Child process execution
// ---------------------------------------------------------------------------

fn run_invocation(
    invocation: &Invocation,
    control: &ExecutionControl,
    deadline: Instant,
) -> Result<StepResult> {
    if control.is_cancelled() {
        return Ok(StepResult::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(DeviceError::Timeout);
    }
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .stdin(if invocation.stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        DeviceError::Backend(format!(
            "failed to start {}: {error}",
            program_label(&invocation.program)
        ))
    })?;
    if let Some(text) = &invocation.stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
    }
    loop {
        if control.is_cancelled() {
            terminate(&mut child);
            return Ok(StepResult::Cancelled);
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            return Err(DeviceError::Timeout);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(StepResult::Completed)
                } else {
                    Err(DeviceError::Backend(format!(
                        "{} exited with status {status}",
                        program_label(&invocation.program)
                    )))
                };
            }
            Ok(None) => std::thread::sleep(CHILD_POLL_INTERVAL),
            Err(error) => {
                terminate(&mut child);
                return Err(DeviceError::Backend(format!(
                    "failed to wait for {}: {error}",
                    program_label(&invocation.program)
                )));
            }
        }
    }
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn program_label(program: &Path) -> String {
    program
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("backend")
        .to_string()
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Click { .. } => "click",
        Action::ControlledTestClick { .. } => "controlled_test_click",
        Action::DoubleClick { .. } => "double_click",
        Action::MouseMove { .. } => "mouse_move",
        Action::Scroll { .. } => "scroll",
        Action::Type { .. } => "type",
        Action::Key { .. } => "key",
        Action::Wait { .. } => "wait",
        Action::OpenApp { .. } => "open_app",
        Action::CloseApp { .. } => "close_app",
        Action::OpenUrl { .. } => "open_url",
        Action::FileOperation { .. } => "file_operation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f64, y: f64, width: f64, height: f64) -> VirtualBounds {
        VirtualBounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn parses_xrandr_current_screen_size() {
        let output = "Screen 0: minimum 320 x 200, current 1920 x 1080, \
                      maximum 16384 x 16384\nHDMI-1 connected primary 1920x1080+0+0\n";
        assert_eq!(
            parse_xrandr_bounds(output),
            Some(bounds(0.0, 0.0, 1920.0, 1080.0))
        );
        assert_eq!(parse_xrandr_bounds("garbage"), None);
    }

    #[test]
    fn parses_swaymsg_output_rectangles() {
        let output = r#"[
            {"name": "HDMI-A-1", "active": true, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}},
            {"name": "DP-1", "active": true, "rect": {"x": 1920, "y": 200, "width": 1280, "height": 1024}},
            {"name": "HEADLESS-1", "active": false, "rect": {"x": 0, "y": 0, "width": 800, "height": 600}}
        ]"#;
        assert_eq!(
            parse_swaymsg_bounds(output),
            Some(bounds(0.0, 0.0, 3200.0, 1224.0))
        );
        assert_eq!(parse_swaymsg_bounds("not json"), None);
    }

    #[test]
    fn parses_wlr_randr_output_blocks() {
        let output = "HDMI-A-1 \"Dell Inc. DELL U2419H\"\n\
                      \x20 Physical size: 600x340 mm\n\
                      \x20 Enabled: yes\n\
                      \x20 Modes:\n\
                      \x20   1920x1080 px, 60.000000 Hz (preferred, current)\n\
                      \x20 Position: 0,0\n\
                      \x20 Transform: normal\n\
                      \x20 Scale: 1.000000\n\
                      DP-1 \"Unknown\"\n\
                      \x20 Enabled: no\n\
                      \x20 Position: 1920,0\n";
        assert_eq!(
            parse_wlr_randr_bounds(output),
            Some(bounds(0.0, 0.0, 1920.0, 1080.0))
        );
    }

    #[test]
    fn parses_current_mode_variant_and_negative_positions() {
        let output = "DP-2 \"Unknown\"\n\
                      \x20 Enabled: yes\n\
                      \x20 Current mode: 2560x1440 @ 59.951 Hz\n\
                      \x20 Position: -2560,0\n";
        assert_eq!(
            parse_wlr_randr_bounds(output),
            Some(bounds(-2560.0, 0.0, 2560.0, 1440.0))
        );
    }

    #[test]
    fn bounds_tolerance_allows_a_few_pixels_outside() {
        let bounds = bounds(0.0, 0.0, 1920.0, 1080.0);
        assert!(bounds.contains_with_tolerance(0.0, 0.0));
        assert!(bounds.contains_with_tolerance(1919.0, 1079.0));
        assert!(bounds.contains_with_tolerance(-BOUNDS_TOLERANCE_PX, 100.0));
        assert!(bounds.contains_with_tolerance(1919.0 + BOUNDS_TOLERANCE_PX, 100.0));
        assert!(!bounds.contains_with_tolerance(1919.0 + BOUNDS_TOLERANCE_PX + 1.0, 100.0));
        assert!(!bounds.contains_with_tolerance(-BOUNDS_TOLERANCE_PX - 1.0, 100.0));
        assert!(!bounds.contains_with_tolerance(f64::NAN, 100.0));
    }

    #[test]
    fn parses_bounds_override_format() {
        assert_eq!(
            parse_bounds_override("-1920, 0, 3840, 1080"),
            Some(bounds(-1920.0, 0.0, 3840.0, 1080.0))
        );
        assert_eq!(parse_bounds_override("0,0,0,1080"), None);
        assert_eq!(parse_bounds_override("0,0,1920"), None);
        assert_eq!(parse_bounds_override("nan,0,1920,1080"), None);
    }

    #[test]
    fn maps_wheel_steps_and_signs() {
        assert_eq!(wheel_steps(0.0), 0);
        assert_eq!(wheel_steps(0.4), 1);
        assert_eq!(wheel_steps(-3.2), 3);
        assert_eq!(wheel_steps(1_000.0), MAX_WHEEL_STEPS);
        assert_eq!(signed_wheel_steps(-2.0), -2);
        assert_eq!(signed_wheel_steps(2.0), 2);
    }

    #[test]
    fn builds_xdotool_bindings() {
        let program = Path::new("/tmp/fake-xdotool");
        let invocations =
            xdotool_invocations(program, &Action::DoubleClick { x: 10.4, y: 20.6 }).unwrap();
        assert_eq!(invocations[0].args, vec!["mousemove", "--sync", "10", "21"]);
        assert_eq!(
            invocations[1].args,
            vec!["click", "--clearmodifiers", "--repeat", "2", "1"]
        );
        let invocations = xdotool_invocations(
            program,
            &Action::Scroll {
                x: 1.0,
                y: 2.0,
                delta_x: -2.0,
                delta_y: 3.0,
            },
        )
        .unwrap();
        assert_eq!(
            invocations[1].args,
            vec!["click", "--clearmodifiers", "--repeat", "3", "5"]
        );
        assert_eq!(
            invocations[2].args,
            vec!["click", "--clearmodifiers", "--repeat", "2", "6"]
        );
    }

    #[test]
    fn builds_ydotool_key_sequences() {
        let sequence =
            ydotool_key_sequence("t", &["ctrl".to_string(), "shift".to_string()]).unwrap();
        assert_eq!(
            sequence,
            vec!["29:1", "42:1", "20:1", "20:0", "42:0", "29:0"]
        );
        let sequence = ydotool_key_sequence("A", &[]).unwrap();
        assert_eq!(sequence, vec!["42:1", "30:1", "30:0", "42:0"]);
        assert!(ydotool_key_sequence("not-a-key", &[]).is_err());
    }

    #[test]
    fn rejects_unsafe_application_names_and_urls() {
        assert_eq!(validate_application_name("firefox").unwrap(), "firefox");
        assert!(validate_application_name("calc && rm -rf ~").is_err());
        assert!(validate_application_name("../evil").is_err());
        assert!(validate_application_name("").is_err());
        assert!(validate_url("https://example.com/a?b=c").is_ok());
        assert!(validate_url("HTTP://example.com").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("https://example.com/\nevil").is_err());
    }
}
