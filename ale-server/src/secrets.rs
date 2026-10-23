//! Headless-safe credential stores for the inference engine.
//!
//! The vendored `ale-core` defaults to [`ale_core::secret_store::SystemSecretStore`],
//! which talks to a D-Bus Secret Service (gnome-keyring, KWallet, …). A headless
//! Debian arm64 unit usually has no Secret Service, and `ConfigManager::load`
//! propagates keyring errors. Because the engine is created lazily and failures
//! are non-fatal (the server keeps serving `/health` and the device channel),
//! that would still leave inference permanently unavailable.
//!
//! This module provides two credential backends and a resolver:
//!
//! * [`FileSecretStore`] — a JSON file (`0600`) at
//!   `[engine].secret_store_path`, defaulting to `$STATE_DIRECTORY/secrets.json`
//!   (systemd `StateDirectory=ale` → `/var/lib/ale/secrets.json`);
//! * [`EnvSecretStore`] — read-only, backed by `ALE_SECRET__*` environment
//!   variables, ideal for containers and secret managers;
//! * [`SecretStoreKind::Auto`] — probe the system keyring and transparently fall
//!   back to the file store with a warning when no Secret Service is reachable.
//!
//! Credential values are never logged; the resolver only logs the *kind* and the
//! file path of the backend it selected.

use crate::config::EngineConfig;
use ale_core::secret_store::{SecretStore, SystemSecretStore};
use ale_core::AleError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Environment variable for the primary cloud API key (env backend).
pub const ENV_API_KEY: &str = "ALE_SECRET__API_KEY";
/// Environment variable for the backup cloud API key (env backend).
pub const ENV_BACKUP_API_KEY: &str = "ALE_SECRET__BACKUP_API_KEY";
/// Environment variable for the independent transcription API key (env backend).
pub const ENV_TRANSCRIPTION_API_KEY: &str = "ALE_SECRET__TRANSCRIPTION_API_KEY";

/// Maximum time the `auto` probe may spend inside the platform keyring.
const SYSTEM_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Selects the credential backend used by the inference engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SecretStoreKind {
    /// Probe the system keyring, fall back to the file store on failure.
    #[default]
    Auto,
    /// Platform keyring (D-Bus Secret Service). May fail on headless hosts.
    System,
    /// JSON file with `0600` permissions.
    File,
    /// Read-only `ALE_SECRET__*` environment variables.
    Env,
}

impl SecretStoreKind {
    /// Lowercase wire name, used by config, `/status` and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::System => "system",
            Self::File => "file",
            Self::Env => "env",
        }
    }
}

impl std::str::FromStr for SecretStoreKind {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "system" => Ok(Self::System),
            "file" => Ok(Self::File),
            "env" => Ok(Self::Env),
            other => Err(format!(
                "unknown secret store `{other}` (expected auto, system, file or env)"
            )),
        }
    }
}

/// Default location of the file credential store.
///
/// * `ALE_SERVER_STATE_DIR` / systemd `STATE_DIRECTORY` → `<dir>/secrets.json`
///   (`StateDirectory=ale` sets it to `/var/lib/ale`);
/// * `XDG_STATE_HOME/ale/secrets.json`;
/// * `$HOME/.local/state/ale/secrets.json`;
/// * `/var/lib/ale/secrets.json`.
pub fn default_secret_store_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("ALE_SERVER_STATE_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("secrets.json");
    }
    if let Some(dir) = std::env::var_os("STATE_DIRECTORY").filter(|value| !value.is_empty()) {
        // systemd may pass a colon-separated list; use the first entry.
        let first = dir.to_string_lossy();
        let first = first.split(':').next().unwrap_or_default();
        if !first.is_empty() {
            return PathBuf::from(first).join("secrets.json");
        }
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(state).join("ale").join("secrets.json");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("ale")
            .join("secrets.json");
    }
    PathBuf::from("/var/lib/ale/secrets.json")
}

/// Resolved location for [`FileSecretStore`].
pub fn resolved_secret_store_path(config: &EngineConfig) -> PathBuf {
    config
        .secret_store_path
        .clone()
        .unwrap_or_else(default_secret_store_path)
}

/// Resolve the configured secret store. Never fails: `auto` falls back to the
/// file store and `system` is returned as-is (the engine turns keyring errors
/// into an `ENGINE_UNAVAILABLE` response without killing the process).
pub async fn resolve_secret_store(config: &EngineConfig) -> Arc<dyn SecretStore> {
    resolve_secret_store_with(config, probe_system_store)
        .await
        .0
}

/// Testable resolver returning the effective backend kind as well as the store.
///
/// `probe` is the platform-keyring probe; tests inject a deterministic result to
/// verify the `auto` fallback without touching a real keyring.
pub(crate) async fn resolve_secret_store_with<F, Fut>(
    config: &EngineConfig,
    probe: F,
) -> (Arc<dyn SecretStore>, SecretStoreKind)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), String>>,
{
    match config.secret_store {
        SecretStoreKind::System => {
            tracing::info!("engine credential store: system keyring (explicitly configured)");
            (Arc::new(SystemSecretStore), SecretStoreKind::System)
        }
        SecretStoreKind::File => {
            let path = resolved_secret_store_path(config);
            tracing::info!(
                path = %path.display(),
                "engine credential store: file (explicitly configured)"
            );
            (Arc::new(FileSecretStore::new(path)), SecretStoreKind::File)
        }
        SecretStoreKind::Env => {
            tracing::info!("engine credential store: environment (ALE_SECRET__*)");
            (Arc::new(EnvSecretStore), SecretStoreKind::Env)
        }
        SecretStoreKind::Auto => match probe().await {
            Ok(()) => {
                tracing::info!("engine credential store: system keyring (auto-detected)");
                (Arc::new(SystemSecretStore), SecretStoreKind::System)
            }
            Err(error) => {
                let path = resolved_secret_store_path(config);
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "system credential store unavailable; falling back to the file secret store"
                );
                (Arc::new(FileSecretStore::new(path)), SecretStoreKind::File)
            }
        },
    }
}

/// Probe the platform keyring. `Ok(())` means it is usable (a missing entry is
/// fine); `Err` means there is no reachable credential service.
pub async fn probe_system_store() -> std::result::Result<(), String> {
    let probe = tokio::task::spawn_blocking(|| {
        SystemSecretStore
            .get_api_key()
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
    match tokio::time::timeout(SYSTEM_PROBE_TIMEOUT, probe).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(format!("credential store probe failed: {join_error}")),
        Err(_) => Err("credential store probe timed out".to_string()),
    }
}

/// JSON document that backs [`FileSecretStore`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SecretsFile {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub backup_api_key: Option<String>,
    #[serde(default)]
    pub transcription_api_key: Option<String>,
}

/// File-backed credential store.
///
/// The file is written atomically and only ever readable/writable by the service
/// user (`0600`). Missing file means "no credentials configured".
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> ale_core::Result<SecretsFile> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                AleError::ConfigError(format!(
                    "cannot parse credential file {}: {error}",
                    self.path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(SecretsFile::default())
            }
            Err(error) => Err(AleError::ConfigError(format!(
                "cannot read credential file {}: {error}",
                self.path.display()
            ))),
        }
    }

    fn write(&self, secrets: &SecretsFile) -> ale_core::Result<()> {
        let contents = serde_json::to_vec_pretty(secrets).map_err(|error| {
            AleError::ConfigError(format!("cannot encode credentials: {error}"))
        })?;
        write_private(&self.path, &contents).map_err(|error| {
            AleError::ConfigError(format!(
                "cannot write credential file {}: {error}",
                self.path.display()
            ))
        })
    }
}

impl SecretStore for FileSecretStore {
    fn get_transcription_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(self
            .read()?
            .transcription_api_key
            .filter(|key| !key.is_empty()))
    }

    fn set_transcription_api_key(&self, key: &str) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.transcription_api_key = Some(key.to_string());
        self.write(&secrets)
    }

    fn delete_transcription_api_key(&self) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.transcription_api_key = None;
        self.write(&secrets)
    }

    fn get_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(self.read()?.api_key.filter(|key| !key.is_empty()))
    }

    fn set_api_key(&self, api_key: &str) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.api_key = Some(api_key.to_string());
        self.write(&secrets)
    }

    fn delete_api_key(&self) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.api_key = None;
        self.write(&secrets)
    }

    fn get_backup_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(self.read()?.backup_api_key.filter(|key| !key.is_empty()))
    }

    fn set_backup_api_key(&self, api_key: &str) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.backup_api_key = Some(api_key.to_string());
        self.write(&secrets)
    }

    fn delete_backup_api_key(&self) -> ale_core::Result<()> {
        let mut secrets = self.read()?;
        secrets.backup_api_key = None;
        self.write(&secrets)
    }
}

/// Read-only credential store backed by environment variables.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvSecretStore;

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl SecretStore for EnvSecretStore {
    fn get_transcription_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(env_value(ENV_TRANSCRIPTION_API_KEY))
    }

    fn set_transcription_api_key(&self, _key: &str) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }

    fn delete_transcription_api_key(&self) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }

    fn get_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(env_value(ENV_API_KEY))
    }

    fn set_api_key(&self, _api_key: &str) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }

    fn delete_api_key(&self) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }

    fn get_backup_api_key(&self) -> ale_core::Result<Option<String>> {
        Ok(env_value(ENV_BACKUP_API_KEY))
    }

    fn set_backup_api_key(&self, _api_key: &str) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }

    fn delete_backup_api_key(&self) -> ale_core::Result<()> {
        Err(AleError::ConfigError(
            "the environment credential store is read-only".into(),
        ))
    }
}

/// Atomically write a private (`0600`) file, creating its parent directory.
fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "secrets.json".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_store() -> (tempfile::TempDir, FileSecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path().join("state").join("secrets.json"));
        (dir, store)
    }

    #[test]
    fn file_store_roundtrips_all_credential_kinds() {
        let (_dir, store) = file_store();
        assert_eq!(store.get_api_key().unwrap(), None);
        store.set_api_key("sk-primary").unwrap();
        store.set_backup_api_key("sk-backup").unwrap();
        store.set_transcription_api_key("sk-transcription").unwrap();

        assert_eq!(store.get_api_key().unwrap().as_deref(), Some("sk-primary"));
        assert_eq!(
            store.get_backup_api_key().unwrap().as_deref(),
            Some("sk-backup")
        );
        assert_eq!(
            store.get_transcription_api_key().unwrap().as_deref(),
            Some("sk-transcription")
        );

        store.delete_api_key().unwrap();
        assert_eq!(store.get_api_key().unwrap(), None);
        assert_eq!(
            store.get_backup_api_key().unwrap().as_deref(),
            Some("sk-backup")
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = file_store();
        store.set_api_key("sk-private").unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn file_store_ignores_empty_credentials() {
        let (_dir, store) = file_store();
        store.set_api_key("").unwrap();
        assert_eq!(store.get_api_key().unwrap(), None);
    }

    #[test]
    fn env_store_reads_ale_secret_variables() {
        std::env::set_var(ENV_API_KEY, "env-key");
        std::env::set_var(ENV_BACKUP_API_KEY, "env-backup");
        let store = EnvSecretStore;
        assert_eq!(store.get_api_key().unwrap().as_deref(), Some("env-key"));
        assert_eq!(
            store.get_backup_api_key().unwrap().as_deref(),
            Some("env-backup")
        );
        assert!(store.set_api_key("other").is_err());
        std::env::remove_var(ENV_API_KEY);
        std::env::remove_var(ENV_BACKUP_API_KEY);
    }

    #[test]
    fn secret_store_kind_parses_and_lists_wire_names() {
        for (name, kind) in [
            ("auto", SecretStoreKind::Auto),
            ("system", SecretStoreKind::System),
            ("file", SecretStoreKind::File),
            ("env", SecretStoreKind::Env),
        ] {
            assert_eq!(name.parse::<SecretStoreKind>().unwrap(), kind);
            assert_eq!(kind.as_str(), name);
        }
        assert!("keyring".parse::<SecretStoreKind>().is_err());
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    #[test]
    fn explicit_secret_path_wins_over_defaults() {
        let config = EngineConfig {
            secret_store_path: Some(PathBuf::from("/tmp/ale-test-secrets.json")),
            ..Default::default()
        };
        assert_eq!(
            resolved_secret_store_path(&config),
            PathBuf::from("/tmp/ale-test-secrets.json")
        );

        let config = EngineConfig::default();
        assert!(resolved_secret_store_path(&config).ends_with("secrets.json"));
    }

    #[tokio::test]
    async fn explicit_file_store_resolves_and_reads_missing_file_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            secret_store: SecretStoreKind::File,
            secret_store_path: Some(dir.path().join("ale").join("secrets.json")),
            ..Default::default()
        };
        let store = resolve_secret_store(&config).await;
        assert_eq!(store.get_api_key().unwrap(), None);
        assert_eq!(store.get_backup_api_key().unwrap(), None);
    }

    #[tokio::test]
    async fn auto_falls_back_to_file_store_when_keyring_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.json");
        let config = EngineConfig {
            secret_store: SecretStoreKind::Auto,
            secret_store_path: Some(path.clone()),
            ..Default::default()
        };

        let (store, kind) = resolve_secret_store_with(&config, || async {
            Err("no D-Bus Secret Service on this host".to_string())
        })
        .await;

        assert_eq!(kind, SecretStoreKind::File);
        store.set_api_key("fallback-key").unwrap();
        assert!(
            path.exists(),
            "file fallback store must have written the secret"
        );
        assert_eq!(
            store.get_api_key().unwrap().as_deref(),
            Some("fallback-key")
        );
    }

    #[tokio::test]
    async fn auto_uses_system_store_when_probe_succeeds() {
        let config = EngineConfig::default();
        let (_store, kind) = resolve_secret_store_with(&config, || async { Ok(()) }).await;
        assert_eq!(kind, SecretStoreKind::System);
    }

    #[tokio::test]
    async fn explicit_env_store_resolves() {
        std::env::set_var(ENV_API_KEY, "resolved-env-key");
        let config = EngineConfig {
            secret_store: SecretStoreKind::Env,
            ..Default::default()
        };
        let store = resolve_secret_store(&config).await;
        assert_eq!(
            store.get_api_key().unwrap().as_deref(),
            Some("resolved-env-key")
        );
        std::env::remove_var(ENV_API_KEY);
    }
}
