use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::policy::{robots::RobotsCache, SiteCache, SiteRules};
use crate::transport::{cef::CefRenderer, TransportClient};

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub state_dir: PathBuf,
    pub proxy: Option<String>,
    pub timeout: Duration,
    pub max_response_bytes: Option<usize>,
    pub cef_timeout: Duration,
    pub daemon_idle_timeout: Duration,
    pub use_daemon: bool,
    pub verbose: bool,
    pub daemon_executable: Option<PathBuf>,
    pub cef_renderer_executable: Option<PathBuf>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            proxy: None,
            timeout: Duration::from_secs(30),
            max_response_bytes: None,
            cef_timeout: Duration::from_secs(60),
            daemon_idle_timeout: Duration::from_secs(300),
            use_daemon: false,
            verbose: false,
            daemon_executable: None,
            cef_renderer_executable: None,
        }
    }
}

pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MESH_WICK_HOME") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home);
        if !path.as_os_str().is_empty() {
            return path.join(".mesh-wick");
        }
    }
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::process::id().to_string());
    std::env::temp_dir().join(format!("mesh-wick-{user}"))
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub struct RuntimeContext {
    pub config: RuntimeConfig,
    pub client: TransportClient,
    pub cef: CefRenderer,
    pub rules: SiteRules,
    pub site_cache: SiteCache,
    pub robots: RobotsCache,
    brave_client: std::sync::Mutex<Option<reqwest::Client>>,
}

impl RuntimeContext {
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        ensure_private_dir(&config.state_dir)?;

        #[cfg(unix)]
        if !config.use_daemon && crate::daemon::is_daemon_running(&config.state_dir) {
            anyhow::bail!(
                "a background wick daemon owns {} (run `wick daemon stop` or pass an isolated `--state-dir`)",
                config.state_dir.display()
            );
        }

        #[cfg(unix)]
        let client = if config.use_daemon {
            TransportClient::new_daemon(
                config.state_dir.clone(),
                config.proxy.clone(),
                config.timeout,
                config.daemon_idle_timeout,
                config.verbose,
            )
            .with_daemon_executable(config.daemon_executable.clone())
        } else {
            TransportClient::new_direct(
                &config.state_dir.join("data"),
                config.proxy.as_deref(),
                config.timeout,
            )?
        };

        #[cfg(not(unix))]
        let client = TransportClient::new_direct(
            &config.state_dir.join("data"),
            config.proxy.as_deref(),
            config.timeout,
        )?;

        let cef = CefRenderer::new(
            config.state_dir.clone(),
            config.proxy.clone(),
            config.daemon_idle_timeout,
            config.use_daemon,
            config.verbose,
        )
        .with_daemon_executable(config.daemon_executable.clone())
        .with_renderer_executable(config.cef_renderer_executable.clone());
        let rules = SiteRules::load(&config.state_dir);
        let site_cache = SiteCache::load(config.state_dir.join("site-cache.json"));
        let robots = RobotsCache::new();

        Ok(Self {
            config,
            client,
            cef,
            rules,
            site_cache,
            robots,
            brave_client: std::sync::Mutex::new(None),
        })
    }

    pub fn brave_client(&self) -> Result<reqwest::Client> {
        let mut guard = self
            .brave_client
            .lock()
            .map_err(|_| anyhow::anyhow!("Brave HTTP client lock poisoned"))?;
        if let Some(ref client) = *guard {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder();
        if let Some(proxy_url) = self.config.proxy.as_deref() {
            let trimmed = proxy_url.trim();
            if !trimmed.is_empty() {
                builder = builder.proxy(reqwest::Proxy::all(trimmed)?);
            }
        }
        let client = builder.build()?;
        *guard = Some(client.clone());
        Ok(client)
    }
}

pub fn clear_state_dir(state_dir: &Path) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let data_dir = state_dir.join("data");
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)?;
    }
    ensure_private_dir(&data_dir)?;
    let cef_cache_dir = state_dir.join("cef-cache");
    if cef_cache_dir.exists() {
        std::fs::remove_dir_all(&cef_cache_dir)?;
    }
    let lock_path = state_dir.join("site-cache.lock");
    let mut lock_opts = std::fs::OpenOptions::new();
    lock_opts
        .create(true)
        .truncate(false)
        .read(true)
        .write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_opts.mode(0o600);
    }
    let lock_file = lock_opts.open(&lock_path)?;
    lock_file.lock()?;

    let cache_file = state_dir.join("site-cache.json");
    match std::fs::remove_file(&cache_file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    drop(lock_file);
    Ok(())
}
