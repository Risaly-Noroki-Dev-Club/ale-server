//! `ale-server` — headless Ale, My Eyes! backend service.
//!
//! Responsibilities:
//!
//! * load + validate `ale-server.toml` (with `ALE_SERVER__*` environment
//!   overrides) and reproduce the summary with `--check-config`;
//! * initialize tracing (stderr, journald friendly, optional JSON);
//! * serve the HTTP API (inference, status, egress) and, when enabled, the
//!   encrypted device terminal channel (WebSocket) concurrently;
//! * log the pairing code + expiry at startup and on rotation;
//! * shut down gracefully on `SIGINT`/`SIGTERM`: stop accepting HTTP requests,
//!   stop the device listener, drain with bounded timeouts;
//! * `--print-pairing` issues a fresh pairing code without starting any listener.
//!
//! Configuration errors always exit non-zero. Runtime engine initialization
//! failures never crash the process: they are reported through `/health`
//! (`degraded`) and `503 ENGINE_UNAVAILABLE` on inference routes.

use ale_server::{
    app,
    config::{Config, ConfigSource, LoadedConfig, LoggingConfig},
    device::{self, DeviceAccess, DeviceRuntime},
    egress::EgressState,
};
use anyhow::{bail, Context};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{oneshot, watch},
};
use tracing_subscriber::EnvFilter;

/// Maximum time the HTTP server may take to drain in-flight requests after a
/// shutdown signal before the process closes the listener anyway.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time the device channel may take to stop after the HTTP server.
const DEVICE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(
    name = "ale-server",
    version,
    about = "Headless Ale, My Eyes! backend: HTTP API, encrypted device channel and allow-listed network egress",
    long_about = None,
)]
struct Cli {
    /// Path to the TOML configuration file
    /// (default: /etc/ale/ale-server.toml).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Validate the configuration and exit without binding any port.
    #[arg(long)]
    check_config: bool,

    /// Print a fresh device pairing code and its lifetime, then exit.
    #[arg(long, conflicts_with = "check_config")]
    print_pairing: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Configuration/runtime failures must be visible even when tracing
            // has not been initialized yet.
            eprintln!("ale-server: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let loaded = Config::load(cli.config.as_deref()).context("failed to load configuration")?;
    loaded
        .config
        .validate()
        .context("invalid configuration (refusing to start)")?;

    init_tracing(&loaded.config.logging);
    for notice in &loaded.notices {
        tracing::warn!("{notice}");
    }
    tracing::info!(
        path = %loaded.path.display(),
        source = config_source_name(loaded.source),
        version = env!("CARGO_PKG_VERSION"),
        "configuration loaded"
    );

    if cli.check_config {
        let summary = serde_json::to_string_pretty(&loaded.config.to_redacted_json())
            .context("failed to render the configuration summary")?;
        println!("{summary}");
        tracing::info!(path = %loaded.path.display(), "configuration OK");
        return Ok(());
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?;
    runtime.block_on(run_async(cli, loaded))
}

async fn run_async(cli: Cli, loaded: LoadedConfig) -> anyhow::Result<()> {
    if cli.print_pairing {
        return print_pairing(&loaded.config);
    }
    // Delete expired browser-task artifacts before serving traffic: a
    // long-running instance must not fill its disk (SRV-03).
    loaded.config.browser_tasks.prune_artifacts();
    run_server(loaded).await
}

fn config_source_name(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::File => "file",
        ConfigSource::Defaults => "defaults",
    }
}

/// Build the tracing subscriber from `[logging]`.
///
/// `RUST_LOG` wins when it parses (standard Rust behaviour, also handy for
/// `journalctl -u ale-server` debugging); otherwise `[logging].level` is used.
fn init_tracing(config: &LoggingConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&config.level).unwrap_or_else(|_| EnvFilter::new("info"))
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true);

    if config
        .format
        .eq_ignore_ascii_case(ale_server::config::LOG_FORMAT_JSON)
    {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// `--print-pairing`: build the device runtime from the loaded `[device]`
/// configuration, force a fresh code and print it together with its lifetime.
///
/// No socket is bound and (importantly) the HTTP API is not started, so this can
/// be used from an admin terminal while the systemd unit owns the real ports.
fn print_pairing(config: &Config) -> anyhow::Result<()> {
    if !config.device.enabled {
        bail!(
            "the device terminal channel is disabled ([device].enabled = false); \
             no pairing code can be issued"
        );
    }
    let runtime = DeviceRuntime::new(config.device.clone());
    let pairing = runtime.rotate_pairing();
    println!("pairing code: {}", pairing.code);
    println!("expires in: {}s", pairing.expires_in_s);
    tracing::info!(
        expires_in_s = pairing.expires_in_s,
        "issued a fresh device pairing code"
    );
    Ok(())
}

async fn run_server(loaded: LoadedConfig) -> anyhow::Result<()> {
    let config = Arc::new(loaded.config.clone());
    let (device_shutdown_tx, device_shutdown_rx) = watch::channel(false);
    let mut device_tasks = Vec::new();
    let mut device_runtime: Option<Arc<DeviceRuntime>> = None;

    // --- Device terminal channel (encrypted WebSocket) ---------------------
    if config.device.enabled {
        let runtime = Arc::new(DeviceRuntime::new(config.device.clone()));
        let bind = config
            .device
            .socket_addr()
            .context("invalid [device].bind address")?;
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("failed to bind the device channel at {bind}"))?;
        tracing::info!(%bind, "device terminal channel listening");

        // Startup pairing code + expiry (the operator needs it to pair a phone).
        device::log_pairing(runtime.as_ref(), "startup");

        let session = runtime.session_service().clone();
        let serve_shutdown = device_shutdown_rx.clone();
        device_tasks.push(tokio::spawn(async move {
            if let Err(error) = session.serve(listener, serve_shutdown).await {
                tracing::error!(error = %error, "device terminal channel stopped with an error");
            }
        }));
        device_tasks.push(device::spawn_pairing_watcher(
            runtime.clone(),
            device_shutdown_rx.clone(),
        ));
        device_runtime = Some(runtime);
    } else {
        tracing::info!("device terminal channel disabled");
    }

    // --- Network egress client --------------------------------------------
    let egress = if config.egress.enabled {
        Some(
            EgressState::new(&config.egress)
                .map_err(anyhow::Error::msg)
                .context("failed to initialize the egress HTTP client")?,
        )
    } else {
        tracing::info!("network egress disabled");
        None
    };

    // --- HTTP API ----------------------------------------------------------
    let device: Option<Arc<dyn DeviceAccess>> =
        device_runtime.map(|runtime| runtime as Arc<dyn DeviceAccess>);
    let state = app::AppState::from_loaded(&loaded, egress, device);
    let router = app::build_router(state);

    let bind = config.server.bind;
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind the HTTP listener at {bind}"))?;
    tracing::info!(
        %bind,
        version = env!("CARGO_PKG_VERSION"),
        "HTTP API listening"
    );

    // `shutdown_signal()` resolves on SIGINT/SIGTERM; the oneshot lets the
    // supervisor enforce a bounded drain time without killing active requests.
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let graceful = async move {
        shutdown_signal().await;
        let _ = stop_tx.send(());
    };
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(graceful);

    tokio::select! {
        result = server => {
            result.context("HTTP server failed")?;
        }
        _ = async move {
            let _ = stop_rx.await;
            tokio::time::sleep(GRACEFUL_SHUTDOWN_TIMEOUT).await;
        } => {
            tracing::warn!(
                timeout_s = GRACEFUL_SHUTDOWN_TIMEOUT.as_secs(),
                "graceful HTTP shutdown timed out; closing the listener"
            );
        }
    }
    tracing::info!("HTTP API stopped; shutting down the device channel");

    // Stop accepting device connections and cancel session tasks.
    let _ = device_shutdown_tx.send(true);
    for task in device_tasks {
        match tokio::time::timeout(DEVICE_SHUTDOWN_TIMEOUT, task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(error = %error, "device task panicked"),
            Err(_) => tracing::warn!(
                timeout_s = DEVICE_SHUTDOWN_TIMEOUT.as_secs(),
                "device task did not stop within the shutdown timeout"
            ),
        }
    }
    tracing::info!("shutdown complete");
    Ok(())
}

/// Resolve on `SIGINT` or (on unix) `SIGTERM`.
///
/// Signal-registration failures are logged and then parked forever rather than
/// panicking: losing the handler must not take the service down immediately.
async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(error = %error, "failed to install the SIGINT handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::error!(error = %error, "failed to install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT; shutting down"),
        _ = terminate => tracing::info!("received SIGTERM; shutting down"),
    }
}
