//! Screenshot capture, downscaling and JPEG/PNG encoding.
//!
//! Owner: capture teammate. Backends are external commands (`grim`, `spectacle`,
//! `gnome-screenshot`, `import`, `scrot`, `screencapture`) so the crate builds and
//! cross-compiles without any native X11/Wayland link dependency.
//!
//! Backend selection (`capture.preferred_backend` wins when set; the tool must
//! exist on `PATH` or be an executable path):
//!
//! | session | order |
//! | --- | --- |
//! | Wayland (`XDG_SESSION_TYPE`/`WAYLAND_DISPLAY`) | `grim` (stdout), `spectacle` (file), `gnome-screenshot` (file) |
//! | X11 (`XDG_SESSION_TYPE`/`DISPLAY`) | `import -window root png:-` (stdout), `scrot` (file), `gnome-screenshot` (file) |
//! | macOS (dev) | `screencapture -x` (file) |
//! | unknown | probe all of the above |
//!
//! File-only backends write a unique PNG under the system temp directory which
//! is read and then deleted.
//!
//! Invariants:
//! * a successful capture always returns a `frame_id` and decoded dimensions;
//! * `data` honors `ImageMime` and is never larger than `protocol::MAX_FRAME_BYTES`;
//! * commands are bounded by `capture.timeout_ms` and killed on timeout;
//! * the service is `Send + Sync` and safe to share behind `Arc`.

use crate::config::CaptureConfig;
use crate::display::{self, DisplayInfo, DisplaySet};
use crate::error::{DeviceError, Result};
use crate::protocol::MAX_FRAME_BYTES;
use image::{DynamicImage, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Hard ceiling on the raw bytes read back from one external command.
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Widest/highest image accepted when decoding, to bound decompression memory.
const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// Maximum allocation (bytes) allowed when decoding a screenshot.
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
/// Display discovery results are reused for this long inside one service.
const DISPLAY_CACHE_TTL_MS: u64 = 5_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMime {
    #[default]
    Jpeg,
    Png,
}

impl ImageMime {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureRequest {
    /// Target display; `None` captures the primary display.
    pub display_id: Option<u32>,
    /// Downscale so the width never exceeds this value.
    pub max_width: Option<u32>,
    pub format: ImageMime,
    /// Ask the backend for a fresh frame instead of a cached one.
    pub fresh: bool,
}

impl Default for CaptureRequest {
    fn default() -> Self {
        Self {
            display_id: None,
            max_width: None,
            format: ImageMime::Jpeg,
            fresh: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapturedFrame {
    pub frame_id: String,
    #[serde(default)]
    pub display_id: Option<u32>,
    pub mime: ImageMime,
    /// Encoded image bytes; never serialized into JSON.
    #[serde(skip)]
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub captured_at_unix_ms: u64,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CaptureCapabilities {
    pub available: bool,
    pub backend: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheKey {
    display_id: Option<u32>,
    /// Effective downscale target; the same source can produce different frames.
    max_width: u32,
    mime: ImageMime,
}

struct CacheEntry {
    frame: CapturedFrame,
    key: CacheKey,
}

struct DisplayCache {
    fetched_at_unix_ms: u64,
    set: DisplaySet,
}

/// Caches the most recent frame so the terminal can reference it when it asks
/// the computer to execute an action plan.
pub struct CaptureService {
    cfg: CaptureConfig,
    last_frame: Mutex<Option<CacheEntry>>,
    displays_cache: Mutex<Option<DisplayCache>>,
}

impl CaptureService {
    pub fn new(cfg: CaptureConfig) -> Self {
        Self {
            cfg,
            last_frame: Mutex::new(None),
            displays_cache: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &CaptureConfig {
        &self.cfg
    }

    /// Name of the capture backend that will be used, if any.
    pub fn backend_name(&self) -> Option<String> {
        resolve_backend(&self.cfg).ok().map(|backend| backend.name)
    }

    pub fn capabilities(&self) -> CaptureCapabilities {
        match resolve_backend(&self.cfg) {
            Ok(backend) => CaptureCapabilities {
                available: true,
                backend: Some(backend.name),
                error: None,
            },
            Err(error) => CaptureCapabilities {
                available: false,
                backend: None,
                error: Some(error),
            },
        }
    }

    pub fn displays(&self) -> Result<DisplaySet> {
        display::list_displays(&self.cfg)
    }

    /// Capture honoring the cache (`cache_ttl_ms`) unless `fresh` is requested.
    pub fn capture(&self, request: &CaptureRequest) -> Result<CapturedFrame> {
        if !request.fresh {
            if let Some(frame) = self.cached_frame(request) {
                return Ok(frame);
            }
        }
        self.capture_now(request)
    }

    /// Force a fresh capture, bypassing the cache.
    pub fn capture_now(&self, request: &CaptureRequest) -> Result<CapturedFrame> {
        let backend = resolve_backend(&self.cfg).map_err(DeviceError::Unavailable)?;
        let discovered = self.discover_displays();

        let selected = match request.display_id {
            None => None,
            Some(id) => match &discovered {
                Ok(set) => Some(set.display(id).cloned().ok_or_else(|| {
                    let known: Vec<u32> = set.displays.iter().map(|display| display.id).collect();
                    DeviceError::Unavailable(format!(
                        "display {id} is not connected; discovered display ids: {known:?}"
                    ))
                })?),
                Err(reason) => {
                    return Err(DeviceError::Unavailable(format!(
                        "display {id} was requested but display discovery failed: {reason}"
                    )))
                }
            },
        };

        let raw = run_backend(&backend, &self.cfg)?;
        let source = decode_image(&raw)?;
        let source_width = source.width();
        let source_height = source.height();

        let crop = match (&selected, &discovered) {
            (Some(display), Ok(set)) => crop_to_display(&source, display, set),
            _ => DisplayCrop::Unsupported,
        };
        let (image, frame_display_id) = match crop {
            DisplayCrop::Cropped(cropped) => (cropped, selected.as_ref().map(|display| display.id)),
            DisplayCrop::AlreadyExact => (source, selected.as_ref().map(|display| display.id)),
            DisplayCrop::Unsupported => (source, None),
        };

        let max_width = request.max_width.unwrap_or(self.cfg.max_width);
        let image = resize_to_width(image, max_width);
        let (width, height) = (image.width(), image.height());
        let data = encode_image(&image, request.format, self.cfg.jpeg_quality)?;
        enforce_frame_limit(&data)?;

        let frame = CapturedFrame {
            frame_id: Uuid::new_v4().to_string(),
            display_id: frame_display_id,
            mime: request.format,
            data,
            width,
            height,
            captured_at_unix_ms: now_unix_ms(),
            backend: backend.name.clone(),
            displays: displays_for_frame(&discovered, source_width, source_height, &backend.name),
        };
        self.store_cache(&frame, request, max_width);
        Ok(frame)
    }

    pub fn last_frame_id(&self) -> Option<String> {
        self.last_frame
            .lock()
            .ok()
            .and_then(|cache| cache.as_ref().map(|entry| entry.frame.frame_id.clone()))
    }

    pub fn last_frame_meta(&self) -> Option<CapturedFrame> {
        self.last_frame
            .lock()
            .ok()
            .and_then(|cache| cache.as_ref().map(|entry| entry.frame.clone()))
    }

    fn cached_frame(&self, request: &CaptureRequest) -> Option<CapturedFrame> {
        if self.cfg.cache_ttl_ms == 0 {
            return None;
        }
        let cache = self.last_frame.lock().ok()?;
        let entry = cache.as_ref()?;
        let key = CacheKey {
            display_id: request.display_id,
            max_width: request.max_width.unwrap_or(self.cfg.max_width),
            mime: request.format,
        };
        if entry.key != key {
            return None;
        }
        let age_ms = now_unix_ms().saturating_sub(entry.frame.captured_at_unix_ms);
        if age_ms <= self.cfg.cache_ttl_ms {
            Some(entry.frame.clone())
        } else {
            None
        }
    }

    fn store_cache(&self, frame: &CapturedFrame, request: &CaptureRequest, max_width: u32) {
        let key = CacheKey {
            display_id: request.display_id,
            max_width,
            mime: request.format,
        };
        if let Ok(mut cache) = self.last_frame.lock() {
            *cache = Some(CacheEntry {
                frame: frame.clone(),
                key,
            });
        }
    }

    /// Display discovery is comparatively expensive on macOS (system_profiler),
    /// so capture reuses a recent result. A failure is not cached.
    fn discover_displays(&self) -> std::result::Result<DisplaySet, String> {
        if let Ok(cache) = self.displays_cache.lock() {
            if let Some(cache) = cache.as_ref() {
                let age_ms = now_unix_ms().saturating_sub(cache.fetched_at_unix_ms);
                if age_ms <= DISPLAY_CACHE_TTL_MS {
                    return Ok(cache.set.clone());
                }
            }
        }
        match display::list_displays(&self.cfg) {
            Ok(set) => {
                if let Ok(mut cache) = self.displays_cache.lock() {
                    *cache = Some(DisplayCache {
                        fetched_at_unix_ms: now_unix_ms(),
                        set: set.clone(),
                    });
                }
                Ok(set)
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Milliseconds since the Unix epoch, saturating on clock errors.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Decode image bytes to discover real dimensions; used by backends that cannot
/// report geometry and by tests.
pub fn decode_dimensions(data: &[u8]) -> Result<(u32, u32)> {
    let reader = guessed_image_reader(data)?;
    reader.into_dimensions().map_err(|error| {
        DeviceError::Backend(format!("could not decode image dimensions: {error}"))
    })
}

// ---------------------------------------------------------------------------
// External command execution
// ---------------------------------------------------------------------------

pub(crate) struct CommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Run an external command with piped output, bounded by `timeout`.
///
/// Output is drained on helper threads so a chatty child can never deadlock on
/// a full pipe. On timeout the child is killed and reaped, then
/// [`DeviceError::Timeout`] is returned. The reader threads are *detached*, not
/// joined: a wrapper script's grandchildren (e.g. `sh -c 'sleep 5; tool'`) can
/// inherit the pipe write ends and keep them open after the direct child is
/// killed, and waiting for EOF would make a hard timeout unbounded.
pub(crate) fn run_command(mut command: Command, timeout: Duration) -> Result<CommandOutput> {
    let program = command.get_program().to_string_lossy().into_owned();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => {
            DeviceError::Unavailable(format!("external command `{program}` was not found"))
        }
        _ => DeviceError::Io(error),
    })?;

    let mut stdout_thread = spawn_output_reader(child.stdout.take());
    let mut stderr_thread = spawn_output_reader(child.stderr.take());

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    detach_output_reader(stdout_thread.take());
                    detach_output_reader(stderr_thread.take());
                    return Err(DeviceError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                detach_output_reader(stdout_thread.take());
                detach_output_reader(stderr_thread.take());
                return Err(DeviceError::Io(error));
            }
        }
    };

    let (stdout, stdout_truncated) = join_output_reader(stdout_thread.take());
    let (stderr, _) = join_output_reader(stderr_thread.take());
    if stdout_truncated {
        return Err(DeviceError::Backend(format!(
            "external command `{program}` produced more than {} MiB of output",
            MAX_COMMAND_OUTPUT_BYTES / (1024 * 1024)
        )));
    }
    Ok(CommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn spawn_output_reader<R>(reader: Option<R>) -> Option<JoinHandle<(Vec<u8>, bool)>>
where
    R: Read + Send + 'static,
{
    reader.map(|reader| std::thread::spawn(move || read_capped(reader)))
}

fn join_output_reader(handle: Option<JoinHandle<(Vec<u8>, bool)>>) -> (Vec<u8>, bool) {
    handle
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default()
}

/// Drop a reader thread without waiting for pipe EOF (used on timeout paths).
fn detach_output_reader(handle: Option<JoinHandle<(Vec<u8>, bool)>>) {
    drop(handle);
}

fn read_capped<R: Read>(mut reader: R) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let room = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(bytes.len());
                if count <= room {
                    bytes.extend_from_slice(&buffer[..count]);
                } else {
                    bytes.extend_from_slice(&buffer[..room]);
                    truncated = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (bytes, truncated)
}

/// Look up an executable by name on `PATH`.
pub(crate) fn command_exists(program: &str) -> bool {
    if program.contains('/') || program.contains('\\') {
        return is_executable(Path::new(program));
    }
    find_in_path(program).is_some()
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|directory| !directory.as_os_str().is_empty())
        .map(|directory| directory.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
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
// Backend selection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Grim,
    Spectacle,
    GnomeScreenshot,
    Import,
    Scrot,
    Screencapture,
    /// A user-configured executable we do not know; captures to stdout.
    Custom,
}

impl BackendKind {
    fn canonical_name(self) -> &'static str {
        match self {
            Self::Grim => "grim",
            Self::Spectacle => "spectacle",
            Self::GnomeScreenshot => "gnome-screenshot",
            Self::Import => "import",
            Self::Scrot => "scrot",
            Self::Screencapture => "screencapture",
            Self::Custom => "custom",
        }
    }

    /// Backend candidates for a session, in preference order.
    fn candidates(session: crate::display::SessionKind) -> &'static [BackendKind] {
        use crate::display::SessionKind;
        use BackendKind::*;
        match session {
            SessionKind::Wayland => &[Grim, Spectacle, GnomeScreenshot],
            SessionKind::X11 => &[Import, Scrot, GnomeScreenshot],
            SessionKind::MacOs => &[Screencapture],
            SessionKind::Unknown => &[
                Grim,
                Spectacle,
                GnomeScreenshot,
                Import,
                Scrot,
                Screencapture,
            ],
        }
    }

    /// Map a configured backend name or binary path to a known backend.
    fn from_preferred(preferred: &str) -> Option<Self> {
        let basename = Path::new(preferred)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(preferred);
        let stem = basename.split('.').next().unwrap_or(basename);
        match stem {
            "grim" => Some(Self::Grim),
            "spectacle" => Some(Self::Spectacle),
            "gnome-screenshot" => Some(Self::GnomeScreenshot),
            "import" => Some(Self::Import),
            "scrot" => Some(Self::Scrot),
            "screencapture" => Some(Self::Screencapture),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedBackend {
    kind: BackendKind,
    /// Stable name reported in frames/capabilities/errors.
    name: String,
    program: PathBuf,
}

/// Resolve the configured backend, or auto-detect one for the session.
///
/// `capture.preferred_backend` may be a bare command name (looked up on `PATH`,
/// verified executable) or an absolute/relative path (used verbatim in tests).
fn resolve_backend(cfg: &CaptureConfig) -> std::result::Result<ResolvedBackend, String> {
    if let Some(preferred) = cfg
        .preferred_backend
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let kind = BackendKind::from_preferred(preferred);
        let is_path = preferred.contains('/') || preferred.contains('\\');
        let program = if is_path {
            let path = PathBuf::from(preferred);
            if !is_executable(&path) {
                return Err(format!(
                    "preferred capture backend `{preferred}` does not exist or is not executable"
                ));
            }
            path
        } else {
            find_in_path(preferred).ok_or_else(|| {
                format!("preferred capture backend `{preferred}` was not found on PATH")
            })?
        };
        let kind = kind.unwrap_or(BackendKind::Custom);
        let name = match kind {
            BackendKind::Custom => Path::new(preferred)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| preferred.to_string()),
            known => known.canonical_name().to_string(),
        };
        return Ok(ResolvedBackend {
            kind,
            name,
            program,
        });
    }

    let session = crate::display::SessionKind::detect();
    let candidates = BackendKind::candidates(session);
    for kind in candidates {
        if let Some(program) = find_in_path(kind.canonical_name()) {
            return Ok(ResolvedBackend {
                kind: *kind,
                name: kind.canonical_name().to_string(),
                program,
            });
        }
    }
    let tried = candidates
        .iter()
        .map(|kind| kind.canonical_name())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "no supported screenshot backend is installed for the detected {session:?} session \
         (tried: {tried}); install one of them or set capture.preferred_backend"
    ))
}

fn run_backend(backend: &ResolvedBackend, cfg: &CaptureConfig) -> Result<Vec<u8>> {
    let timeout = Duration::from_millis(cfg.timeout_ms.clamp(200, 60_000));
    match backend.kind {
        BackendKind::Grim | BackendKind::Import | BackendKind::Custom => {
            run_stdout_backend(backend, timeout)
        }
        BackendKind::Spectacle
        | BackendKind::GnomeScreenshot
        | BackendKind::Scrot
        | BackendKind::Screencapture => run_file_backend(backend, timeout),
    }
}

/// Arguments for backends that write the image to stdout.
fn stdout_args(kind: BackendKind) -> &'static [&'static str] {
    match kind {
        BackendKind::Grim => &["-"],
        BackendKind::Import => &["-window", "root", "png:-"],
        _ => &[],
    }
}

/// Arguments for backends that only write a file; `path` is a unique temp path.
fn file_args(kind: BackendKind, path: &str) -> Vec<String> {
    match kind {
        BackendKind::Spectacle => vec![
            "-b".to_string(),
            "-n".to_string(),
            "-f".to_string(),
            "-o".to_string(),
            path.to_string(),
        ],
        BackendKind::GnomeScreenshot => vec!["-f".to_string(), path.to_string()],
        BackendKind::Scrot => vec![path.to_string()],
        BackendKind::Screencapture => vec!["-x".to_string(), path.to_string()],
        _ => vec![path.to_string()],
    }
}

fn run_stdout_backend(backend: &ResolvedBackend, timeout: Duration) -> Result<Vec<u8>> {
    let mut command = Command::new(&backend.program);
    command.args(stdout_args(backend.kind));
    let output = run_command(command, timeout)?;
    if !output.status.success() {
        return Err(backend_failure(backend, &output));
    }
    if output.stdout.is_empty() {
        return Err(DeviceError::Backend(format!(
            "capture backend `{}` produced no image data",
            backend.name
        )));
    }
    Ok(output.stdout)
}

fn run_file_backend(backend: &ResolvedBackend, timeout: Duration) -> Result<Vec<u8>> {
    // Backends such as spectacle/gnome-screenshot/screencapture cannot stream to
    // stdout, so they write a PNG into a unique temp path that is deleted after.
    let file = TempCaptureFile::new("png");
    let mut command = Command::new(&backend.program);
    command.args(file_args(backend.kind, &file.path().to_string_lossy()));
    let output = run_command(command, timeout)?;
    if !output.status.success() {
        return Err(backend_failure(backend, &output));
    }
    let data = fs::read(file.path()).map_err(|error| {
        DeviceError::Backend(format!(
            "capture backend `{}` finished but did not write `{}`: {error}",
            backend.name,
            file.path().display()
        ))
    })?;
    if data.is_empty() {
        return Err(DeviceError::Backend(format!(
            "capture backend `{}` wrote an empty image file",
            backend.name
        )));
    }
    Ok(data)
}

fn backend_failure(backend: &ResolvedBackend, output: &CommandOutput) -> DeviceError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        let mut detail: String = stderr.chars().take(500).collect();
        if stderr.chars().count() > 500 {
            detail.push('…');
        }
        format!(": {detail}")
    };
    DeviceError::Backend(format!(
        "capture backend `{}` failed with {}{detail}",
        backend.name, output.status
    ))
}

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempCaptureFile {
    path: PathBuf,
}

impl TempCaptureFile {
    fn new(extension: &str) -> Self {
        let unique = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ale-capture-{}-{unique}-{}.{extension}",
            std::process::id(),
            now_unix_ms()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempCaptureFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Image decoding, cropping and encoding
// ---------------------------------------------------------------------------

// `image::Limits` is #[non_exhaustive], so it cannot be built with struct-update
// syntax; the allow keeps clippy from suggesting the impossible rewrite.
#[allow(clippy::field_reassign_with_default)]
fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    limits
}

fn guessed_image_reader(data: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>> {
    if data.is_empty() {
        return Err(DeviceError::Backend(
            "cannot decode image: empty data".to_string(),
        ));
    }
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|error| {
            DeviceError::Backend(format!("could not detect the image format: {error}"))
        })?;
    reader.limits(decode_limits());
    Ok(reader)
}

fn decode_image(data: &[u8]) -> Result<DynamicImage> {
    let reader = guessed_image_reader(data)?;
    reader.decode().map_err(|error| {
        DeviceError::Backend(format!("could not decode screenshot image: {error}"))
    })
}

enum DisplayCrop {
    /// The captured image is already exactly the requested display.
    AlreadyExact,
    /// Portion of the virtual desktop that belongs to the requested display.
    Cropped(DynamicImage),
    /// Geometry/scale did not line up; keep the whole image.
    Unsupported,
}

fn crop_to_display(image: &DynamicImage, display: &DisplayInfo, set: &DisplaySet) -> DisplayCrop {
    let Some((min_x, min_y, total_width, total_height)) = set.total_bounds() else {
        return DisplayCrop::Unsupported;
    };
    if total_width == 0 || total_height == 0 || image.width() == 0 || image.height() == 0 {
        return DisplayCrop::Unsupported;
    }
    // Map virtual-desktop coordinates into image pixels. Scale factors != 1
    // (HiDPI compositors) show up as a uniform image/desktop ratio.
    let scale_x = f64::from(image.width()) / f64::from(total_width);
    let scale_y = f64::from(image.height()) / f64::from(total_height);
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return DisplayCrop::Unsupported;
    }
    if (scale_x - scale_y).abs() > 0.01 {
        return DisplayCrop::Unsupported;
    }

    let image_width = f64::from(image.width());
    let image_height = f64::from(image.height());
    let offset_x = (f64::from(display.x.saturating_sub(min_x)) * scale_x)
        .round()
        .max(0.0);
    let offset_y = (f64::from(display.y.saturating_sub(min_y)) * scale_y)
        .round()
        .max(0.0);
    if offset_x >= image_width || offset_y >= image_height {
        return DisplayCrop::Unsupported;
    }
    let x = (offset_x as u32).min(image.width() - 1);
    let y = (offset_y as u32).min(image.height() - 1);
    let width = (f64::from(display.width) * scale_x)
        .round()
        .max(1.0)
        .min(image_width - offset_x) as u32;
    let height = (f64::from(display.height) * scale_y)
        .round()
        .max(1.0)
        .min(image_height - offset_y) as u32;
    let width = width.min(image.width() - x).max(1);
    let height = height.min(image.height() - y).max(1);

    if x == 0 && y == 0 && width == image.width() && height == image.height() {
        return DisplayCrop::AlreadyExact;
    }
    let cropped = image::imageops::crop_imm(image, x, y, width, height).to_image();
    DisplayCrop::Cropped(DynamicImage::ImageRgba8(cropped))
}

fn resize_to_width(image: DynamicImage, max_width: u32) -> DynamicImage {
    if max_width == 0 || image.width() <= max_width {
        return image;
    }
    let new_height = ((u64::from(image.height()) * u64::from(max_width)) / u64::from(image.width()))
        .max(1) as u32;
    image.resize_exact(max_width, new_height, image::imageops::FilterType::Triangle)
}

fn encode_image(image: &DynamicImage, mime: ImageMime, jpeg_quality: u8) -> Result<Vec<u8>> {
    match mime {
        ImageMime::Jpeg => {
            let rgb = image.to_rgb8();
            let quality = jpeg_quality.clamp(1, 100);
            let mut data = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, quality);
            rgb.write_with_encoder(encoder).map_err(|error| {
                DeviceError::Backend(format!("could not encode screenshot as JPEG: {error}"))
            })?;
            Ok(data)
        }
        ImageMime::Png => {
            let mut data = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
                .map_err(|error| {
                    DeviceError::Backend(format!("could not encode screenshot as PNG: {error}"))
                })?;
            Ok(data)
        }
    }
}

fn enforce_frame_limit(data: &[u8]) -> Result<()> {
    if data.len() > MAX_FRAME_BYTES {
        return Err(DeviceError::Backend(format!(
            "encoded screenshot is {} KiB which exceeds the {} KiB frame limit; \
             lower capture.max_width or capture.jpeg_quality",
            data.len().div_ceil(1024),
            MAX_FRAME_BYTES / 1024
        )));
    }
    Ok(())
}

fn displays_for_frame(
    discovered: &std::result::Result<DisplaySet, String>,
    source_width: u32,
    source_height: u32,
    backend: &str,
) -> Vec<DisplayInfo> {
    if let Ok(set) = discovered {
        if !set.displays.is_empty() {
            return set.displays.clone();
        }
    }
    // No display tool available: report the captured desktop so automation can
    // still map image coordinates back to the screen.
    vec![DisplayInfo {
        id: 0,
        name: "virtual".to_string(),
        x: 0,
        y: 0,
        width: source_width.max(1),
        height: source_height.max(1),
        scale_factor: 1.0,
        primary: true,
        backend: backend.to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, x: i32, y: i32, width: u32, height: u32, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id,
            name: format!("display-{id}"),
            x,
            y,
            width,
            height,
            scale_factor: 1.0,
            primary,
            backend: "xrandr".to_string(),
        }
    }

    #[test]
    fn synthetic_display_uses_captured_geometry() {
        let displays = displays_for_frame(&Err("no tools".to_string()), 1920, 1080, "grim");
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].width, 1920);
        assert_eq!(displays[0].height, 1080);
        assert!(displays[0].primary);
        assert_eq!(displays[0].backend, "grim");
    }

    #[test]
    fn crops_requested_display_from_virtual_desktop() {
        let set = DisplaySet {
            displays: vec![
                display(0, 0, 0, 1920, 1080, true),
                display(1, 1920, 0, 1280, 720, false),
            ],
            backend: "xrandr".to_string(),
        };
        let image = DynamicImage::ImageRgb8(image::RgbImage::new(3200, 1080));
        match crop_to_display(&image, &set.displays[1], &set) {
            DisplayCrop::Cropped(cropped) => {
                assert_eq!(cropped.width(), 1280);
                assert_eq!(cropped.height(), 720);
            }
            _ => panic!("expected a cropped image"),
        }
    }

    #[test]
    fn crop_respects_hidpi_image_scale() {
        let set = DisplaySet {
            displays: vec![display(0, 0, 0, 1280, 720, true)],
            backend: "wlr-randr".to_string(),
        };
        let image = DynamicImage::ImageRgb8(image::RgbImage::new(2560, 1440));
        assert!(matches!(
            crop_to_display(&image, &set.displays[0], &set),
            DisplayCrop::AlreadyExact
        ));
    }

    #[test]
    fn cache_key_differs_by_format_and_width() {
        let base = CacheKey {
            display_id: None,
            max_width: 1600,
            mime: ImageMime::Jpeg,
        };
        assert_ne!(
            base,
            CacheKey {
                mime: ImageMime::Png,
                ..base.clone()
            }
        );
        assert_ne!(
            base,
            CacheKey {
                max_width: 800,
                ..base.clone()
            }
        );
    }

    #[test]
    fn resize_keeps_aspect_ratio_and_never_upscales() {
        let image = DynamicImage::ImageRgb8(image::RgbImage::new(2000, 1000));
        let scaled = resize_to_width(image, 800);
        assert_eq!((scaled.width(), scaled.height()), (800, 400));

        let image = DynamicImage::ImageRgb8(image::RgbImage::new(320, 240));
        let untouched = resize_to_width(image, 1600);
        assert_eq!((untouched.width(), untouched.height()), (320, 240));
    }
}
