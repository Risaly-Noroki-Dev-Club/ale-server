//! TOML configuration for the headless `ale-server` service.
//!
//! # Sources (highest priority last)
//!
//! 1. built-in defaults ([`Config::default`]);
//! 2. the TOML file (`/etc/ale/ale-server.toml` by default, `--config` override);
//! 3. `ALE_SERVER__SECTION__KEY` environment variables (double underscore selects
//!    a nested key, e.g. `ALE_SERVER__DEVICE__PAIRING__CODE_TTL_S=600`).
//!
//! Environment overrides are applied after the file, so they can be used by
//! systemd units or containers without editing the file. List values are
//! comma-separated (`ALE_SERVER__EGRESS__ALLOWED_HOSTS=api.openai.com,api.example.com`).
//!
//! The whole schema is validated by [`Config::validate`] before any listener is
//! bound; `--check-config` performs the same validation and exits.
//!
//! [`Config::to_redacted_json`] produces the summary served by `GET /status`. It
//! intentionally never contains credential values (only the *name* of the
//! environment variable that holds the egress credential).

use ale_device::DeviceConfig;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tracing_subscriber::EnvFilter;

/// Default configuration file path (see `docs/CONTRACTS.md`).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/ale/ale-server.toml";
/// Default HTTP bind address.
pub const DEFAULT_SERVER_BIND: &str = "0.0.0.0:8080";
/// Prefix of environment overrides.
pub const ENV_PREFIX: &str = "ALE_SERVER__";

/// Log format `"text"` (journald friendly single-line key=value).
pub const LOG_FORMAT_TEXT: &str = "text";
/// Log format `"json"` (one JSON object per line).
pub const LOG_FORMAT_JSON: &str = "json";

/// Root configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub engine: EngineConfig,
    pub device: DeviceConfig,
    pub egress: EgressConfig,
    pub logging: LoggingConfig,
    pub browser_tasks: crate::browser_tasks::BrowserTasksConfig,
}

/// `[server]` — HTTP listener settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// HTTP bind address.
    pub bind: SocketAddr,
    /// Optional CORS origin allow-list. Empty means "no CORS layer at all";
    /// `*` is rejected because the API is never permissive by default.
    pub cors_origin: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_SERVER_BIND
                .parse()
                .expect("DEFAULT_SERVER_BIND must be a valid socket address"),
            cors_origin: Vec::new(),
        }
    }
}

/// `[engine]` — local/cloud inference engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// Enable the inference routes. When `false` they answer `503 ENGINE_UNAVAILABLE`.
    pub enabled: bool,
    /// Path to the `ale-core` JSON application configuration. `None` lets the
    /// engine resolve its platform default (`~/.config/ale-my-eyes/config.json`).
    pub config_path: Option<PathBuf>,
    /// Credential backend. `auto` probes the system keyring and falls back to the
    /// file store on headless hosts (see [`crate::secrets`]).
    pub secret_store: crate::secrets::SecretStoreKind,
    /// Override for the file credential store location.
    /// Defaults to `$STATE_DIRECTORY/secrets.json` (systemd `StateDirectory=ale`
    /// → `/var/lib/ale/secrets.json`), then `~/.local/state/ale/secrets.json`.
    pub secret_store_path: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            config_path: None,
            secret_store: crate::secrets::SecretStoreKind::Auto,
            secret_store_path: None,
        }
    }
}

impl EngineConfig {
    /// Effective engine configuration path (explicit value or platform default).
    pub fn resolved_config_path(&self) -> PathBuf {
        self.config_path
            .clone()
            .unwrap_or_else(default_engine_config_path)
    }
}

/// Platform default for the vendored `ale-core` configuration file.
///
/// Mirrors `ale_core::config::ConfigFactory::create_default()` (the `dirs`
/// crate's config directory + `ale-my-eyes/config.json`).
pub fn default_engine_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ale-my-eyes")
        .join("config.json")
}

/// `[egress]` — allow-listed network egress proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EgressConfig {
    /// Enable `/v1/egress/*`. When `false` those routes answer `403 EGRESS_DISABLED`.
    pub enabled: bool,
    /// OpenAI-compatible upstream base URL, without the `/v1/...` path
    /// (e.g. `https://api.openai.com`).
    pub upstream: String,
    /// Exact upstream host allow-list. Host names only, no schemes or paths.
    pub allowed_hosts: Vec<String>,
    /// Environment variable that carries the upstream API key. The key value is
    /// read per request and is never serialized or logged.
    pub api_key_env: String,
    /// Upstream request timeout.
    pub timeout_s: u64,
    /// Largest accepted request body for the egress routes.
    pub max_body_bytes: usize,
    /// Largest accepted upstream response body.
    pub max_response_bytes: usize,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            upstream: "https://api.openai.com".to_string(),
            allowed_hosts: vec!["api.openai.com".to_string()],
            api_key_env: "OPENAI_API_KEY".to_string(),
            timeout_s: 60,
            max_body_bytes: 1024 * 1024,
            max_response_bytes: 16 * 1024 * 1024,
        }
    }
}

impl EgressConfig {
    /// Upstream credential, read fresh from the environment (never cached, never
    /// logged). Returns `None` when the variable is unset or blank.
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.api_key_env)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Whether the configured credential environment variable is currently set.
    pub fn api_key_configured(&self) -> bool {
        self.api_key().is_some()
    }

    /// Whether `host` is in the allow-list (case-insensitive exact match).
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(host))
    }

    fn validate(&self) -> Result<()> {
        if self.timeout_s == 0 || self.timeout_s > 600 {
            return Err(ConfigError::Invalid(
                "egress.timeout_s must be within 1..=600".to_string(),
            ));
        }
        if !(1024..=64 * 1024 * 1024).contains(&self.max_body_bytes) {
            return Err(ConfigError::Invalid(
                "egress.max_body_bytes must be within 1 KiB..=64 MiB".to_string(),
            ));
        }
        if !(1024..=256 * 1024 * 1024).contains(&self.max_response_bytes) {
            return Err(ConfigError::Invalid(
                "egress.max_response_bytes must be within 1 KiB..=256 MiB".to_string(),
            ));
        }
        if !self.enabled {
            return Ok(());
        }

        let url = reqwest::Url::parse(self.upstream.trim()).map_err(|error| {
            ConfigError::Invalid(format!("egress.upstream is not a valid URL: {error}"))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ConfigError::Invalid(
                "egress.upstream must use http or https".to_string(),
            ));
        }
        let host = url.host_str().ok_or_else(|| {
            ConfigError::Invalid("egress.upstream must contain a host name".to_string())
        })?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ConfigError::Invalid(
                "egress.upstream must not embed credentials; use egress.api_key_env".to_string(),
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ConfigError::Invalid(
                "egress.upstream must be a bare base URL without query or fragment".to_string(),
            ));
        }
        if self.allowed_hosts.is_empty() {
            return Err(ConfigError::Invalid(
                "egress.allowed_hosts must not be empty when egress is enabled".to_string(),
            ));
        }
        for allowed in &self.allowed_hosts {
            validate_host_entry(allowed)?;
        }
        if !self.host_allowed(host) {
            return Err(ConfigError::Invalid(format!(
                "egress.upstream host `{host}` is not listed in egress.allowed_hosts"
            )));
        }
        if self.api_key_env.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "egress.api_key_env must name an environment variable when egress is enabled"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// `[logging]` — tracing subscriber settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing` filter directive, e.g. `info` or `ale_server=debug,tower_http=warn`.
    pub level: String,
    /// `text` or `json`.
    pub format: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: LOG_FORMAT_TEXT.to_string(),
        }
    }
}

impl LoggingConfig {
    fn validate(&self) -> Result<()> {
        if EnvFilter::try_new(&self.level).is_err() {
            return Err(ConfigError::Invalid(format!(
                "logging.level is not a valid tracing filter: `{}`",
                self.level
            )));
        }
        if !self.format.eq_ignore_ascii_case(LOG_FORMAT_TEXT)
            && !self.format.eq_ignore_ascii_case(LOG_FORMAT_JSON)
        {
            return Err(ConfigError::Invalid(format!(
                "logging.format must be `{LOG_FORMAT_TEXT}` or `{LOG_FORMAT_JSON}` (got `{}`)",
                self.format
            )));
        }
        Ok(())
    }
}

/// Where the effective configuration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    /// Loaded from a TOML file.
    File,
    /// No configuration file was present; built-in defaults were used.
    Defaults,
}

/// Result of loading configuration, including provenance for logs and `/status`.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    /// The path that was considered (explicit `--config` or the default path).
    pub path: PathBuf,
    pub source: ConfigSource,
    /// Operator-facing notes to log once tracing is initialized.
    pub notices: Vec<String>,
}

/// Configuration errors. All are fatal at startup and make `--check-config`
/// exit non-zero.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("invalid environment override {var}: {message}")]
    Env { var: String, message: String },
}

/// Shorthand result used throughout this module.
pub type Result<T> = std::result::Result<T, ConfigError>;

impl Config {
    /// Load configuration using the process environment.
    ///
    /// * `path = Some(file)` — explicit `--config`; the file must exist.
    /// * `path = None` — `/etc/ale/ale-server.toml`; when missing, built-in
    ///   defaults are used and a notice is recorded (the service still starts).
    pub fn load(path: Option<&Path>) -> Result<LoadedConfig> {
        Self::load_with_env(path, std::env::vars())
    }

    /// Testable variant of [`Config::load`] with an explicit environment
    /// iterator. Only variables starting with `ALE_SERVER__` are considered.
    pub fn load_with_env<I>(path: Option<&Path>, env: I) -> Result<LoadedConfig>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let (mut config, path, source, mut notices) = match path {
            Some(explicit) => {
                if !explicit.exists() {
                    return Err(ConfigError::Read {
                        path: explicit.to_path_buf(),
                        source: io::Error::new(
                            io::ErrorKind::NotFound,
                            "configuration file does not exist",
                        ),
                    });
                }
                (
                    Self::load_file(explicit)?,
                    explicit.to_path_buf(),
                    ConfigSource::File,
                    Vec::new(),
                )
            }
            None => {
                let default_path = PathBuf::from(DEFAULT_CONFIG_PATH);
                if default_path.exists() {
                    (
                        Self::load_file(&default_path)?,
                        default_path,
                        ConfigSource::File,
                        Vec::new(),
                    )
                } else {
                    let notice = format!(
                        "configuration file {} does not exist; using built-in defaults \
                         (environment overrides are still applied)",
                        default_path.display()
                    );
                    (
                        Config::default(),
                        default_path,
                        ConfigSource::Defaults,
                        vec![notice],
                    )
                }
            }
        };

        let applied = config.apply_env(env)?;
        if !applied.is_empty() {
            notices.push(format!(
                "applied {} environment override(s): {}",
                applied.len(),
                applied.join(", ")
            ));
        }

        Ok(LoadedConfig {
            config,
            path,
            source,
            notices,
        })
    }

    /// Parse a TOML document into a [`Config`]. Missing sections/keys fall back
    /// to their defaults; unknown keys are rejected.
    pub fn load_from_toml(text: &str) -> Result<Config> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })
    }

    fn load_file(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Apply `ALE_SERVER__...` overrides to this configuration.
    ///
    /// Returns the sorted list of environment variable names that were applied.
    /// Unknown keys and unparsable values are hard errors so that a typo in a
    /// systemd unit cannot silently disable a security control.
    pub fn apply_env<I>(&mut self, env: I) -> Result<Vec<String>>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut applied = Vec::new();
        for (var, raw) in env {
            let Some(rest) = var.strip_prefix(ENV_PREFIX) else {
                continue;
            };
            let segments: Vec<String> = rest
                .split("__")
                .map(|segment| segment.trim().to_ascii_lowercase())
                .collect();
            if segments.iter().any(|segment| segment.is_empty()) {
                return Err(ConfigError::Env {
                    var,
                    message: "malformed override path".to_string(),
                });
            }
            let field = segments.join(".");
            self.apply_env_value(&var, &field, raw.trim())?;
            applied.push(var);
        }
        applied.sort();
        Ok(applied)
    }

    fn apply_env_value(&mut self, var: &str, field: &str, raw: &str) -> Result<()> {
        match field {
            "server.bind" => self.server.bind = parse_field(var, raw)?,
            "server.cors_origin" => self.server.cors_origin = parse_list(raw),
            "engine.enabled" => self.engine.enabled = parse_bool(var, raw)?,
            "engine.config_path" => {
                self.engine.config_path = if raw.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(raw))
                }
            }
            "engine.secret_store" => self.engine.secret_store = parse_field(var, raw)?,
            "engine.secret_store_path" => {
                self.engine.secret_store_path = if raw.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(raw))
                }
            }
            "device.enabled" => self.device.enabled = parse_bool(var, raw)?,
            "device.bind" => self.device.bind = raw.to_string(),
            "device.pairing.enabled" => self.device.pairing.enabled = parse_bool(var, raw)?,
            "device.pairing.code_ttl_s" => self.device.pairing.code_ttl_s = parse_field(var, raw)?,
            "device.pairing.max_attempts" => {
                self.device.pairing.max_attempts = parse_field(var, raw)?
            }
            "device.pairing.token_ttl_s" => {
                self.device.pairing.token_ttl_s = parse_field(var, raw)?
            }
            "device.capture.preferred_backend" => {
                self.device.capture.preferred_backend = parse_optional(raw)
            }
            "device.capture.max_width" => self.device.capture.max_width = parse_field(var, raw)?,
            "device.capture.jpeg_quality" => {
                self.device.capture.jpeg_quality = parse_field(var, raw)?
            }
            "device.capture.timeout_ms" => self.device.capture.timeout_ms = parse_field(var, raw)?,
            "device.capture.cache_ttl_ms" => {
                self.device.capture.cache_ttl_ms = parse_field(var, raw)?
            }
            "device.automation.preferred_backend" => {
                self.device.automation.preferred_backend = parse_optional(raw)
            }
            "device.automation.allow_high_risk" => {
                self.device.automation.allow_high_risk = parse_bool(var, raw)?
            }
            "device.automation.allow_process_actions" => {
                self.device.automation.allow_process_actions = parse_bool(var, raw)?
            }
            "device.automation.dry_run" => self.device.automation.dry_run = parse_bool(var, raw)?,
            "device.automation.timeout_ms" => {
                self.device.automation.timeout_ms = parse_field(var, raw)?
            }
            "device.automation.max_actions" => {
                self.device.automation.max_actions = parse_field(var, raw)?
            }
            "device.automation.frame_max_age_ms" => {
                self.device.automation.frame_max_age_ms = parse_field(var, raw)?
            }
            "device.limits.max_connections" => {
                self.device.limits.max_connections = parse_field(var, raw)?
            }
            "device.limits.max_connections_per_ip" => {
                self.device.limits.max_connections_per_ip = parse_field(var, raw)?
            }
            "device.limits.max_concurrent_requests" => {
                self.device.limits.max_concurrent_requests = parse_field(var, raw)?
            }
            "device.limits.max_message_bytes" => {
                self.device.limits.max_message_bytes = parse_field(var, raw)?
            }
            "device.limits.idle_timeout_s" => {
                self.device.limits.idle_timeout_s = parse_field(var, raw)?
            }
            "device.limits.request_timeout_s" => {
                self.device.limits.request_timeout_s = parse_field(var, raw)?
            }
            "egress.enabled" => self.egress.enabled = parse_bool(var, raw)?,
            "egress.upstream" => self.egress.upstream = raw.to_string(),
            "egress.allowed_hosts" => self.egress.allowed_hosts = parse_list(raw),
            "egress.api_key_env" => self.egress.api_key_env = raw.to_string(),
            "egress.timeout_s" => self.egress.timeout_s = parse_field(var, raw)?,
            "egress.max_body_bytes" => self.egress.max_body_bytes = parse_field(var, raw)?,
            "egress.max_response_bytes" => self.egress.max_response_bytes = parse_field(var, raw)?,
            "browser_tasks.enabled" => self.browser_tasks.enabled = parse_bool(var, raw)?,
            "browser_tasks.webdriver_url" => self.browser_tasks.webdriver_url = raw.to_string(),
            "browser_tasks.model_url" => self.browser_tasks.model_url = raw.to_string(),
            "browser_tasks.model" => self.browser_tasks.model = raw.to_string(),
            "browser_tasks.api_key_env" => self.browser_tasks.api_key_env = raw.to_string(),
            "browser_tasks.allowed_hosts" => self.browser_tasks.allowed_hosts = parse_list(raw),
            "browser_tasks.output_dir" => self.browser_tasks.output_dir = PathBuf::from(raw),
            "browser_tasks.max_steps" => self.browser_tasks.max_steps = parse_field(var, raw)?,
            "browser_tasks.max_concurrent" => {
                self.browser_tasks.max_concurrent = parse_field(var, raw)?
            }
            "browser_tasks.task_timeout_s" => {
                self.browser_tasks.task_timeout_s = parse_field(var, raw)?
            }
            "browser_tasks.call_timeout_s" => {
                self.browser_tasks.call_timeout_s = parse_field(var, raw)?
            }
            "browser_tasks.max_request_bytes" => {
                self.browser_tasks.max_request_bytes = parse_field(var, raw)?
            }
            "browser_tasks.max_response_bytes" => {
                self.browser_tasks.max_response_bytes = parse_field(var, raw)?
            }
            "browser_tasks.max_page_chars" => {
                self.browser_tasks.max_page_chars = parse_field(var, raw)?
            }
            "browser_tasks.max_markdown_bytes" => {
                self.browser_tasks.max_markdown_bytes = parse_field(var, raw)?
            }
            "browser_tasks.max_output_tokens" => {
                self.browser_tasks.max_output_tokens = parse_field(var, raw)?
            }
            "browser_tasks.artifact_retention_s" => {
                self.browser_tasks.artifact_retention_s = parse_field(var, raw)?
            }
            "logging.level" => self.logging.level = raw.to_string(),
            "logging.format" => self.logging.format = raw.to_string(),
            other => {
                return Err(ConfigError::Env {
                    var: var.to_string(),
                    message: format!("unknown configuration key `{other}`"),
                });
            }
        }
        Ok(())
    }

    /// Validate every section. Called before binding any listener and by
    /// `--check-config`.
    pub fn validate(&self) -> Result<()> {
        for origin in &self.server.cors_origin {
            validate_origin(origin)?;
        }

        if self.engine.enabled {
            if let Some(path) = &self.engine.config_path {
                if path.as_os_str().is_empty() {
                    return Err(ConfigError::Invalid(
                        "engine.config_path must not be empty".to_string(),
                    ));
                }
            }
            if let Some(path) = &self.engine.secret_store_path {
                if path.as_os_str().is_empty() {
                    return Err(ConfigError::Invalid(
                        "engine.secret_store_path must not be empty".to_string(),
                    ));
                }
            }
        }

        self.device.validate().map_err(|error| {
            ConfigError::Invalid(format!("invalid [device] configuration: {error}"))
        })?;

        if self.device.enabled && self.device.socket_addr().ok() == Some(self.server.bind) {
            return Err(ConfigError::Invalid(format!(
                "server.bind and device.bind must not be the same address ({})",
                self.server.bind
            )));
        }

        self.egress.validate()?;
        self.browser_tasks
            .validate(self.server.bind)
            .map_err(ConfigError::Invalid)?;
        self.logging.validate()?;
        Ok(())
    }

    /// Redacted configuration summary for `GET /status`. Credential *values* are
    /// never included — only the name of the environment variable and whether it
    /// is currently populated.
    pub fn to_redacted_json(&self) -> serde_json::Value {
        let engine_config_path = self
            .engine
            .config_path
            .as_ref()
            .map(|path| path.display().to_string());
        let device = serde_json::to_value(&self.device).unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "server": {
                "bind": self.server.bind.to_string(),
                "cors_origin": self.server.cors_origin,
            },
            "engine": {
                "enabled": self.engine.enabled,
                "config_path": engine_config_path,
                "resolved_config_path": self.engine.resolved_config_path().display().to_string(),
                "secret_store": self.engine.secret_store.as_str(),
                "secret_store_path": crate::secrets::resolved_secret_store_path(&self.engine)
                    .display()
                    .to_string(),
            },
            "device": device,
            "browser_tasks": self.browser_tasks,
            "egress": {
                "enabled": self.egress.enabled,
                "upstream": self.egress.upstream,
                "allowed_hosts": self.egress.allowed_hosts,
                "api_key_env": self.egress.api_key_env,
                "api_key_configured": self.egress.api_key_configured(),
                "timeout_s": self.egress.timeout_s,
                "max_body_bytes": self.egress.max_body_bytes,
                "max_response_bytes": self.egress.max_response_bytes,
            },
            "logging": {
                "level": self.logging.level,
                "format": self.logging.format,
            },
        })
    }
}

fn validate_origin(origin: &str) -> Result<()> {
    let trimmed = origin.trim();
    if trimmed.is_empty() {
        return Err(ConfigError::Invalid(
            "server.cors_origin entries must not be empty".to_string(),
        ));
    }
    if trimmed != origin {
        return Err(ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` has leading/trailing whitespace"
        )));
    }
    if origin == "*" || origin.eq_ignore_ascii_case("null") {
        return Err(ConfigError::Invalid(
            "server.cors_origin must list explicit origins; wildcard permissive CORS is not allowed"
                .to_string(),
        ));
    }
    let url = reqwest::Url::parse(origin).map_err(|error| {
        ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` is not a URL: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` must use http or https"
        )));
    }
    if url.host_str().is_none() {
        return Err(ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` must contain a host"
        )));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` must be a bare origin (scheme://host[:port])"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError::Invalid(format!(
            "server.cors_origin entry `{origin}` must not embed credentials"
        )));
    }
    Ok(())
}

fn validate_host_entry(host: &str) -> Result<()> {
    if host.trim().is_empty() {
        return Err(ConfigError::Invalid(
            "egress.allowed_hosts entries must not be empty".to_string(),
        ));
    }
    if host.trim() != host {
        return Err(ConfigError::Invalid(format!(
            "egress.allowed_hosts entry `{host}` has leading/trailing whitespace"
        )));
    }
    if host.contains("://") || host.contains('/') || host.contains('*') {
        return Err(ConfigError::Invalid(format!(
            "egress.allowed_hosts entry `{host}` must be a bare host name (no scheme, path or wildcard)"
        )));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(ConfigError::Invalid(format!(
            "egress.allowed_hosts entry `{host}` must not contain whitespace"
        )));
    }
    Ok(())
}

fn parse_field<T>(var: &str, raw: &str) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    raw.parse::<T>().map_err(|error| ConfigError::Env {
        var: var.to_string(),
        message: format!("invalid value `{raw}`: {error}"),
    })
}

fn parse_bool(var: &str, raw: &str) -> Result<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Env {
            var: var.to_string(),
            message: format!("invalid boolean `{raw}` (expected true/false)"),
        }),
    }
}

fn parse_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_optional(raw: &str) -> Option<String> {
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL_TOML: &str = r#"
[server]
bind = "127.0.0.1:18080"
cors_origin = ["https://admin.example.com"]

[engine]
enabled = false
config_path = "/var/lib/ale/config.json"
secret_store = "file"
secret_store_path = "/var/lib/ale/secrets.json"

[device]
enabled = false
bind = "127.0.0.1:37655"

[device.pairing]
code_ttl_s = 120

[device.capture]
max_width = 1280

[egress]
enabled = true
upstream = "https://api.example.com"
allowed_hosts = ["api.example.com"]
api_key_env = "ALE_TEST_KEY"
timeout_s = 30
max_body_bytes = 4096
max_response_bytes = 65536

[logging]
level = "debug"
format = "json"
"#;

    fn empty_env() -> Vec<(String, String)> {
        Vec::new()
    }

    #[test]
    fn defaults_are_sane() {
        let config = Config::default();
        assert_eq!(config.server.bind.to_string(), "0.0.0.0:8080");
        assert!(config.server.cors_origin.is_empty());
        assert!(config.engine.enabled);
        assert!(config.engine.config_path.is_none());
        assert_eq!(
            config.engine.secret_store,
            crate::secrets::SecretStoreKind::Auto
        );
        assert!(config.engine.secret_store_path.is_none());
        assert!(config.device.enabled);
        assert_eq!(config.device.bind, "0.0.0.0:37655");
        assert!(!config.egress.enabled);
        assert_eq!(config.egress.max_body_bytes, 1024 * 1024);
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.format, "text");
        config.validate().expect("defaults must validate");
    }

    #[test]
    fn parses_full_toml_document() {
        let config = Config::load_from_toml(FULL_TOML).expect("valid toml");
        assert_eq!(config.server.bind.to_string(), "127.0.0.1:18080");
        assert_eq!(config.server.cors_origin, vec!["https://admin.example.com"]);
        assert!(!config.engine.enabled);
        assert_eq!(
            config.engine.config_path.as_deref(),
            Some(Path::new("/var/lib/ale/config.json"))
        );
        assert_eq!(
            config.engine.secret_store,
            crate::secrets::SecretStoreKind::File
        );
        assert_eq!(
            config.engine.secret_store_path.as_deref(),
            Some(Path::new("/var/lib/ale/secrets.json"))
        );
        assert!(!config.device.enabled);
        assert_eq!(config.device.pairing.code_ttl_s, 120);
        assert_eq!(config.device.capture.max_width, 1280);
        assert!(config.egress.enabled);
        assert_eq!(config.egress.allowed_hosts, vec!["api.example.com"]);
        assert_eq!(config.egress.max_body_bytes, 4096);
        assert_eq!(config.logging.format, "json");
        config.validate().expect("full document must validate");
    }

    #[test]
    fn missing_sections_fall_back_to_defaults() {
        let config =
            Config::load_from_toml("[server]\nbind = \"127.0.0.1:9000\"\n").expect("partial toml");
        assert_eq!(config.server.bind.to_string(), "127.0.0.1:9000");
        assert!(config.engine.enabled);
        assert!(config.device.pairing.enabled);
        assert_eq!(config.egress.timeout_s, 60);
    }

    #[test]
    fn unknown_toml_key_is_rejected() {
        let error = Config::load_from_toml("[server]\nbiind = \"127.0.0.1:9000\"\n")
            .expect_err("typo must fail");
        assert!(error.to_string().contains("failed to parse"), "{error}");
    }

    #[test]
    fn env_overrides_are_applied_after_defaults() {
        let mut config = Config::default();
        let env = vec![
            (
                "ALE_SERVER__SERVER__BIND".to_string(),
                "127.0.0.1:9999".to_string(),
            ),
            (
                "ALE_SERVER__SERVER__CORS_ORIGIN".to_string(),
                "https://a.example.com, https://b.example.com".to_string(),
            ),
            (
                "ALE_SERVER__DEVICE__ENABLED".to_string(),
                "false".to_string(),
            ),
            (
                "ALE_SERVER__DEVICE__PAIRING__CODE_TTL_S".to_string(),
                "600".to_string(),
            ),
            ("ALE_SERVER__ENGINE__ENABLED".to_string(), "0".to_string()),
            (
                "ALE_SERVER__ENGINE__SECRET_STORE".to_string(),
                "file".to_string(),
            ),
            (
                "ALE_SERVER__EGRESS__ALLOWED_HOSTS".to_string(),
                "api.openai.com,api.example.com".to_string(),
            ),
            (
                "ALE_SERVER__EGRESS__MAX_BODY_BYTES".to_string(),
                "2048".to_string(),
            ),
            (
                "ALE_SERVER__LOGGING__FORMAT".to_string(),
                "json".to_string(),
            ),
            ("RUST_LOG".to_string(), "debug".to_string()),
        ];
        let applied = config.apply_env(env).expect("overrides must apply");
        assert_eq!(config.server.bind.to_string(), "127.0.0.1:9999");
        assert_eq!(
            config.server.cors_origin,
            vec!["https://a.example.com", "https://b.example.com"]
        );
        assert!(!config.device.enabled);
        assert_eq!(config.device.pairing.code_ttl_s, 600);
        assert!(!config.engine.enabled);
        assert_eq!(
            config.engine.secret_store,
            crate::secrets::SecretStoreKind::File
        );
        assert_eq!(config.egress.allowed_hosts.len(), 2);
        assert_eq!(config.egress.max_body_bytes, 2048);
        assert_eq!(config.logging.format, "json");
        assert_eq!(applied.len(), 9);
        assert!(!applied.iter().any(|var| var == "RUST_LOG"));
        assert!(applied.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn env_override_rejects_unknown_key_and_bad_value() {
        let mut config = Config::default();
        let unknown = config
            .apply_env(vec![(
                "ALE_SERVER__SERVER__BIND_ADDR".to_string(),
                "x".to_string(),
            )])
            .expect_err("unknown key must fail");
        assert!(unknown.to_string().contains("unknown configuration key"));

        let mut config = Config::default();
        let bad_value = config
            .apply_env(vec![(
                "ALE_SERVER__SERVER__BIND".to_string(),
                "nope".to_string(),
            )])
            .expect_err("bad value must fail");
        assert!(bad_value.to_string().contains("invalid value"));

        let mut config = Config::default();
        let malformed = config
            .apply_env(vec![("ALE_SERVER____X".to_string(), "1".to_string())])
            .expect_err("malformed path must fail");
        assert!(malformed.to_string().contains("malformed"));
    }

    #[test]
    fn env_override_clears_optional_paths() {
        let mut config = Config::default();
        config.engine.config_path = Some(PathBuf::from("/tmp/x.json"));
        config
            .apply_env(vec![(
                "ALE_SERVER__ENGINE__CONFIG_PATH".to_string(),
                String::new(),
            )])
            .unwrap();
        assert!(config.engine.config_path.is_none());
    }

    #[test]
    fn validation_rejects_permissive_or_malformed_cors() {
        for bad in [
            "*",
            "null",
            "admin.example.com",
            "https://admin.example.com/path",
        ] {
            let mut config = Config::default();
            config.server.cors_origin = vec![bad.to_string()];
            assert!(
                config.validate().is_err(),
                "origin `{bad}` should have been rejected"
            );
        }

        let mut config = Config::default();
        config.server.cors_origin = vec!["https://admin.example.com".to_string()];
        config.validate().expect("explicit origin is valid");
    }

    #[test]
    fn validation_rejects_unallowlisted_upstream_and_embedded_credentials() {
        let mut config = Config::default();
        config.egress.enabled = true;
        config.egress.upstream = "https://evil.example.com".to_string();
        config.egress.allowed_hosts = vec!["api.openai.com".to_string()];
        let error = config
            .validate()
            .expect_err("upstream host must be allow-listed");
        assert!(error.to_string().contains("not listed"));

        let mut config = Config::default();
        config.egress.enabled = true;
        config.egress.upstream = "https://user:secret@api.openai.com".to_string();
        config.egress.allowed_hosts = vec!["api.openai.com".to_string()];
        let error = config.validate().expect_err("credentials in URL must fail");
        assert!(error.to_string().contains("must not embed credentials"));

        let mut config = Config::default();
        config.egress.enabled = true;
        config.egress.allowed_hosts = vec!["*".to_string()];
        let error = config.validate().expect_err("wildcard host must fail");
        assert!(error.to_string().contains("bare host name"));
    }

    #[test]
    fn validation_rejects_same_http_and_device_bind() {
        let mut config = Config::default();
        config.server.bind = "127.0.0.1:37655".parse().unwrap();
        config.device.bind = "127.0.0.1:37655".to_string();
        let error = config.validate().expect_err("port clash must fail");
        assert!(error.to_string().contains("must not be the same address"));
    }

    #[test]
    fn validation_rejects_bad_log_format_and_timeout() {
        let mut config = Config::default();
        config.logging.format = "xml".to_string();
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.egress.timeout_s = 0;
        assert!(config.validate().is_err());

        let mut config = Config::default();
        config.egress.max_body_bytes = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn redacted_json_never_exposes_credential_values() {
        let env_name = "ALE_SERVER_TEST_EGRESS_SECRET_FOR_REDACTION";
        std::env::set_var(env_name, "sk-super-secret-value");
        let mut config = Config::default();
        config.server.cors_origin = vec!["https://admin.example.com".to_string()];
        config.egress.enabled = true;
        config.egress.upstream = "https://api.openai.com".to_string();
        config.egress.allowed_hosts = vec!["api.openai.com".to_string()];
        config.egress.api_key_env = env_name.to_string();

        let value = config.to_redacted_json();
        let rendered = value.to_string();
        assert!(!rendered.contains("sk-super-secret-value"));
        assert_eq!(value["egress"]["api_key_env"], env_name);
        assert_eq!(value["egress"]["api_key_configured"], true);
        assert_eq!(value["server"]["bind"], "0.0.0.0:8080");
        std::env::remove_var(env_name);
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let path = PathBuf::from("/definitely/not/a/real/ale-server-test-config.toml");
        let error = Config::load_with_env(Some(&path), empty_env())
            .expect_err("explicit missing config must fail");
        assert!(error.to_string().contains("failed to read"));
    }

    #[test]
    fn config_file_plus_env_overrides_compose() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ale-server.toml");
        std::fs::write(
            &path,
            "[server]\nbind = \"127.0.0.1:18080\"\n\n[logging]\nformat = \"json\"\n",
        )
        .unwrap();
        let loaded = Config::load_with_env(
            Some(&path),
            vec![("ALE_SERVER__LOGGING__LEVEL".to_string(), "warn".to_string())],
        )
        .unwrap();
        assert_eq!(loaded.source, ConfigSource::File);
        assert_eq!(loaded.config.server.bind.to_string(), "127.0.0.1:18080");
        assert_eq!(loaded.config.logging.level, "warn");
        assert_eq!(loaded.config.logging.format, "json");
        assert!(loaded
            .notices
            .iter()
            .any(|notice| notice.contains("override")));
    }

    #[test]
    fn default_path_missing_uses_defaults_with_notice() {
        if Path::new(DEFAULT_CONFIG_PATH).exists() {
            // A real deployment artifact is present on this machine; skip.
            return;
        }
        let loaded = Config::load_with_env(None, empty_env()).unwrap();
        assert_eq!(loaded.source, ConfigSource::Defaults);
        assert!(loaded.path.ends_with("ale-server.toml"));
        assert!(loaded
            .notices
            .iter()
            .any(|notice| notice.contains("defaults")));
        loaded.config.validate().unwrap();
    }
}
