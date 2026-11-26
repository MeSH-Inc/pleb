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
    pub cef_timeout: Duration,
    pub cef_idle_timeout: Duration,
    pub cef_daemon: bool,
    pub verbose: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            proxy: None,
            timeout: Duration::from_secs(30),
            cef_timeout: Duration::from_secs(60),
            cef_idle_timeout: Duration::from_secs(300),
            cef_daemon: false,
            verbose: false,
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
}

impl RuntimeContext {
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        ensure_private_dir(&config.state_dir)?;
        let storage_path = config.state_dir.join("data");
        let client = TransportClient::new(
            &storage_path,
            config.proxy.as_deref(),
            config.timeout,
        )?;
        let cef = CefRenderer::new(
            config.state_dir.clone(),
            config.proxy.clone(),
            config.cef_idle_timeout,
            config.cef_daemon,
            config.verbose,
        );
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
        })
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
    let cache_file = state_dir.join("site-cache.json");
    if cache_file.exists() {
        std::fs::remove_file(&cache_file)?;
    }
    Ok(())
}
