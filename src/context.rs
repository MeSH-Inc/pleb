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
    pub verbose: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            state_dir: default_state_dir(),
            proxy: None,
            timeout: Duration::from_secs(30),
            cef_timeout: Duration::from_secs(60),
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
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".mesh-wick")
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
        let storage_path = config.state_dir.join("data");
        let client = TransportClient::new(
            &storage_path,
            config.proxy.as_deref(),
            config.timeout,
        )?;
        let cef = CefRenderer::new(
            config.state_dir.clone(),
            config.proxy.clone(),
            config.cef_timeout,
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

    pub fn clear_session(&self) -> Result<()> {
        clear_state_dir(&self.config.state_dir)?;
        self.site_cache.clear()?;
        Ok(())
    }
}

pub fn clear_state_dir(state_dir: &Path) -> Result<()> {
    let data_dir = state_dir.join("data");
    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)?;
    }
    std::fs::create_dir_all(&data_dir)?;
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
