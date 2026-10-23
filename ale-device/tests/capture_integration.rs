//! End-to-end capture tests using fake backend scripts.
//!
//! Each test generates a real PNG fixture with the `image` crate and a shell
//! script that stands in for a screenshot tool. The script's absolute path is
//! passed through the frozen `capture.preferred_backend` setting, so these
//! tests need no display server and never mutate the process-global `PATH`.

#![cfg(unix)]

use ale_device::capture::{
    decode_dimensions, now_unix_ms, CaptureRequest, CaptureService, ImageMime,
};
use ale_device::config::CaptureConfig;
use ale_device::error::DeviceError;
use image::RgbImage;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Write a real PNG fixture generated at test time (no checked-in binary files).
fn write_fixture_png(directory: &Path, name: &str, width: u32, height: u32) -> PathBuf {
    let path = directory.join(name);
    let mut image = RgbImage::new(width, height);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([
            (x % 256) as u8,
            (y % 256) as u8,
            ((x.wrapping_add(y)) % 256) as u8,
        ]);
    }
    image.save(&path).expect("write PNG fixture");
    path
}

/// Create an executable `#!/bin/sh` script inside `directory`.
fn write_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    let mut file = fs::File::create(&path).expect("create fake backend");
    writeln!(file, "#!/bin/sh").expect("write shebang");
    file.write_all(body.as_bytes())
        .expect("write fake backend body");
    drop(file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake backend");
    path
}

/// Fake backend named `grim` that streams the fixture from its own directory.
fn fake_stdout_backend(directory: &Path, name: &str) -> PathBuf {
    write_script(
        directory,
        name,
        "cat \"$(dirname \"$0\")/frame.png\"\n\
         echo run >> \"$(dirname \"$0\")/runs.log\"\n",
    )
}

fn service_config(backend: &Path) -> CaptureConfig {
    CaptureConfig {
        preferred_backend: Some(backend.display().to_string()),
        max_width: 1_600,
        jpeg_quality: 80,
        timeout_ms: 5_000,
        cache_ttl_ms: 0,
    }
}

fn run_count(directory: &Path) -> usize {
    fs::read_to_string(directory.join("runs.log"))
        .map(|log| log.lines().count())
        .unwrap_or(0)
}

#[test]
fn captures_png_and_fills_frame_metadata() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 800, 400);
    let backend = fake_stdout_backend(dir.path(), "grim");
    let service = CaptureService::new(service_config(&backend));

    assert_eq!(service.backend_name().as_deref(), Some("grim"));
    let capabilities = service.capabilities();
    assert!(capabilities.available);
    assert_eq!(capabilities.backend.as_deref(), Some("grim"));
    assert!(capabilities.error.is_none());

    let request = CaptureRequest {
        format: ImageMime::Png,
        ..CaptureRequest::default()
    };
    let frame = service.capture_now(&request).expect("capture");

    assert_eq!((frame.width, frame.height), (800, 400));
    assert_eq!(frame.mime, ImageMime::Png);
    assert_eq!(frame.backend, "grim");
    assert_eq!(frame.display_id, None);
    assert_eq!(decode_dimensions(&frame.data).unwrap(), (800, 400));
    assert_eq!(&frame.data[..8], b"\x89PNG\r\n\x1a\n");
    assert!(!frame.frame_id.is_empty());
    assert!(frame.captured_at_unix_ms > 0);
    assert!(frame.captured_at_unix_ms <= now_unix_ms());
    assert!(
        !frame.displays.is_empty(),
        "frames always report at least a synthetic display"
    );

    let decoded = image::load_from_memory(&frame.data).expect("decode captured PNG");
    assert_eq!((decoded.width(), decoded.height()), (800, 400));

    assert_eq!(
        service.last_frame_id().as_deref(),
        Some(frame.frame_id.as_str())
    );
    let cached_meta = service.last_frame_meta().expect("last frame meta");
    assert_eq!(cached_meta.frame_id, frame.frame_id);
    assert_eq!(cached_meta.width, 800);
}

#[test]
fn downscales_to_requested_width_and_uses_config_default() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 2000, 1000);
    let backend = fake_stdout_backend(dir.path(), "grim");
    let mut config = service_config(&backend);
    config.max_width = 1_600;
    let service = CaptureService::new(config);

    let jpeg = service
        .capture_now(&CaptureRequest {
            max_width: Some(1_000),
            format: ImageMime::Jpeg,
            ..CaptureRequest::default()
        })
        .expect("downscaled capture");
    assert_eq!((jpeg.width, jpeg.height), (1_000, 500));
    assert_eq!(decode_dimensions(&jpeg.data).unwrap(), (1_000, 500));
    assert_eq!(&jpeg.data[..2], &[0xFF, 0xD8], "JPEG SOI marker expected");
    let decoded = image::load_from_memory(&jpeg.data).expect("decode captured JPEG");
    assert_eq!((decoded.width(), decoded.height()), (1_000, 500));

    // No per-request max_width: the service-wide config value applies.
    let default_width = service
        .capture_now(&CaptureRequest {
            format: ImageMime::Png,
            ..CaptureRequest::default()
        })
        .expect("capture with config max_width");
    assert_eq!((default_width.width, default_width.height), (1_600, 800));
}

#[test]
fn cache_ttl_reuses_frames_and_fresh_bypasses_it() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 320, 200);
    let backend = fake_stdout_backend(dir.path(), "grim");
    let mut config = service_config(&backend);
    config.cache_ttl_ms = 60_000;
    let service = CaptureService::new(config);
    let request = CaptureRequest::default();

    let first = service.capture(&request).expect("first capture");
    let reused = service.capture(&request).expect("cached capture");
    assert_eq!(first.frame_id, reused.frame_id);
    assert_eq!(
        run_count(dir.path()),
        1,
        "cache hit must not rerun the tool"
    );

    let fresh = service
        .capture(&CaptureRequest {
            fresh: true,
            ..request.clone()
        })
        .expect("fresh capture");
    assert_ne!(fresh.frame_id, first.frame_id);
    assert_eq!(run_count(dir.path()), 2);

    let forced = service.capture_now(&request).expect("forced capture");
    assert_ne!(forced.frame_id, fresh.frame_id);
    assert_eq!(run_count(dir.path()), 3);
    assert_eq!(
        service.last_frame_id().as_deref(),
        Some(forced.frame_id.as_str())
    );
}

#[test]
fn zero_cache_ttl_always_recaptures() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 320, 200);
    let backend = fake_stdout_backend(dir.path(), "grim");
    let service = CaptureService::new(service_config(&backend));
    let request = CaptureRequest::default();

    let first = service.capture(&request).expect("first capture");
    let second = service.capture(&request).expect("second capture");
    assert_ne!(first.frame_id, second.frame_id);
    assert_eq!(run_count(dir.path()), 2);
}

#[test]
fn cache_is_keyed_by_request_parameters() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 320, 200);
    let backend = fake_stdout_backend(dir.path(), "grim");
    let mut config = service_config(&backend);
    config.cache_ttl_ms = 60_000;
    let service = CaptureService::new(config);

    let jpeg = service
        .capture(&CaptureRequest {
            format: ImageMime::Jpeg,
            ..CaptureRequest::default()
        })
        .expect("jpeg capture");
    let png = service
        .capture(&CaptureRequest {
            format: ImageMime::Png,
            ..CaptureRequest::default()
        })
        .expect("png capture");
    assert_ne!(jpeg.frame_id, png.frame_id);
    assert_eq!(jpeg.mime, ImageMime::Jpeg);
    assert_eq!(png.mime, ImageMime::Png);
    assert_eq!(run_count(dir.path()), 2);
}

#[test]
fn capture_now_times_out_and_kills_a_slow_backend() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 320, 200);
    let backend = write_script(
        dir.path(),
        "grim",
        "sleep 5\ncat \"$(dirname \"$0\")/frame.png\"\n",
    );
    let mut config = service_config(&backend);
    config.timeout_ms = 200; // minimum allowed by CaptureConfig::validate
    let service = CaptureService::new(config);

    let started = Instant::now();
    let error = service
        .capture_now(&CaptureRequest::default())
        .expect_err("slow backend must time out");
    let elapsed = started.elapsed();
    assert!(matches!(error, DeviceError::Timeout), "got {error:?}");
    assert!(
        elapsed < Duration::from_secs(4),
        "capture must be killed near the 200 ms timeout, took {elapsed:?}"
    );
}

#[test]
fn file_based_backend_writes_temp_file_that_is_cleaned_up() {
    let dir = TempDir::new().unwrap();
    write_fixture_png(dir.path(), "frame.png", 640, 480);
    let backend = write_script(
        dir.path(),
        "scrot",
        "printf '%s' \"$1\" > \"$(dirname \"$0\")/temp-path.txt\"\n\
         cp \"$(dirname \"$0\")/frame.png\" \"$1\"\n",
    );
    let service = CaptureService::new(service_config(&backend));

    let frame = service
        .capture_now(&CaptureRequest {
            format: ImageMime::Png,
            ..CaptureRequest::default()
        })
        .expect("file-backed capture");
    assert_eq!((frame.width, frame.height), (640, 480));
    assert_eq!(frame.backend, "scrot");
    assert_eq!(decode_dimensions(&frame.data).unwrap(), (640, 480));

    let temp_path = fs::read_to_string(dir.path().join("temp-path.txt")).unwrap();
    assert!(
        !Path::new(&temp_path).exists(),
        "capture temp file {temp_path} must be deleted"
    );
}

#[test]
fn backend_failure_is_reported_with_stderr() {
    let dir = TempDir::new().unwrap();
    let backend = write_script(dir.path(), "grim", "echo boom >&2\nexit 3\n");
    let service = CaptureService::new(service_config(&backend));

    // The binary exists, so the capability is advertised; execution still fails.
    assert!(service.capabilities().available);
    let error = service
        .capture_now(&CaptureRequest::default())
        .expect_err("failing backend must surface an error");
    match error {
        DeviceError::Backend(message) => {
            assert!(message.contains("grim"), "{message}");
            assert!(message.contains("boom"), "{message}");
        }
        other => panic!("expected a backend error, got {other:?}"),
    }
}

#[test]
fn missing_backend_path_reports_unavailable_capability() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("missing-backend");
    let service = CaptureService::new(CaptureConfig {
        preferred_backend: Some(missing.display().to_string()),
        ..CaptureConfig::default()
    });

    assert!(service.backend_name().is_none());
    let capabilities = service.capabilities();
    assert!(!capabilities.available);
    assert!(capabilities.backend.is_none());
    let message = capabilities.error.expect("error message");
    assert!(message.contains("missing-backend"), "{message}");

    let error = service
        .capture_now(&CaptureRequest::default())
        .expect_err("capture without a backend must fail");
    assert!(
        matches!(error, DeviceError::Unavailable(_)),
        "got {error:?}"
    );
}

#[test]
fn missing_named_backend_reports_not_found_on_path() {
    let service = CaptureService::new(CaptureConfig {
        preferred_backend: Some("ale-test-no-such-capture-backend".to_string()),
        ..CaptureConfig::default()
    });
    let capabilities = service.capabilities();
    assert!(!capabilities.available);
    let message = capabilities.error.expect("error message");
    assert!(message.contains("not found on PATH"), "{message}");
}

#[test]
fn decode_dimensions_rejects_non_image_data() {
    assert!(decode_dimensions(b"").is_err());
    assert!(decode_dimensions(b"this is definitely not an image").is_err());
}
