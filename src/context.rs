use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::policy::{robots::RobotsCache, SiteCache, SiteRules};
use crate::transport::{
    cdp::CdpRenderer, cef::CefRenderer, web_bot_auth::WebBotAuthSigner, TransportClient,
};

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
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
    pub no_web_bot_auth: bool,
    pub cdp_url: Option<String>,
    pub cdp_timeout: Duration,
    pub prefer_cdp: bool,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_cdp = self
            .cdp_url
            .as_deref()
            .map(crate::transport::cdp::sanitize_cdp_url_for_display);
        f.debug_struct("RuntimeConfig")
            .field("state_dir", &self.state_dir)
            .field("proxy", &self.proxy)
            .field("timeout", &self.timeout)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("cef_timeout", &self.cef_timeout)
            .field("daemon_idle_timeout", &self.daemon_idle_timeout)
            .field("use_daemon", &self.use_daemon)
            .field("verbose", &self.verbose)
            .field("daemon_executable", &self.daemon_executable)
            .field("cef_renderer_executable", &self.cef_renderer_executable)
            .field("no_web_bot_auth", &self.no_web_bot_auth)
            .field("cdp_url", &redacted_cdp)
            .field("cdp_timeout", &self.cdp_timeout)
            .field("prefer_cdp", &self.prefer_cdp)
            .finish()
    }
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
            no_web_bot_auth: false,
            cdp_url: None,
            cdp_timeout: Duration::from_secs(60),
            prefer_cdp: false,
        }
    }
}

pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PLEB_HOME") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let path = PathBuf::from(home);
        if !path.as_os_str().is_empty() {
            return path.join(".pleb");
        }
    }
    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::process::id().to_string());
    std::env::temp_dir().join(format!("pleb-{user}"))
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)?;
            std::fs::symlink_metadata(path)?
        }
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "refusing to use symlink as private directory: {}",
            path.display()
        );
    }
    if !meta.is_dir() {
        anyhow::bail!(
            "expected private directory, found non-directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let current_uid = unsafe { libc::geteuid() };
        if meta.uid() != current_uid {
            anyhow::bail!(
                "refusing to use private directory {} owned by uid {} (expected uid {})",
                path.display(),
                meta.uid(),
                current_uid
            );
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn acquire_private_lock(lock_path: &Path) -> std::io::Result<std::fs::File> {
    let mut lock_opts = std::fs::OpenOptions::new();
    lock_opts
        .create(true)
        .truncate(false)
        .read(true)
        .write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        lock_opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock_file = lock_opts.open(lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = lock_file.metadata()?;
        let current_uid = unsafe { libc::geteuid() };
        if !meta.is_file() || meta.uid() != current_uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing to use lock file {}", lock_path.display()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
    }
    lock_file.lock()?;
    Ok(lock_file)
}

pub(crate) fn write_private_file_atomic(path: &Path, payload: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path has no parent directory: {}", path.display()),
        )
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let pid = std::process::id();

    let (temp_path, mut temp_file) = (0..16)
        .find_map(|_| {
            let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!("{file_name}.tmp.{pid}.{seq}"));
            let mut opts = std::fs::OpenOptions::new();
            opts.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            match opts.open(&candidate) {
                Ok(file) => Some(Ok((candidate, file))),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(err) => Some(Err(err)),
            }
        })
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("failed to allocate temporary file in {}", parent.display()),
            )
        })??;

    let write_result = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temp_file.write_all(payload)?;
        temp_file.sync_all()
    })();
    drop(temp_file);

    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
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
    http_client: Mutex<Option<reqwest::Client>>,
    web_bot_auth_signer: Mutex<Option<Option<Arc<WebBotAuthSigner>>>>,
    cdp_renderer: Mutex<Option<Option<Arc<CdpRenderer>>>>,
}

impl RuntimeContext {
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        ensure_private_dir(&config.state_dir)?;

        #[cfg(unix)]
        if !config.use_daemon && crate::daemon::is_daemon_running(&config.state_dir) {
            anyhow::bail!(
                "a background pleb daemon owns {} (run `pleb daemon stop` or pass an isolated `--state-dir`)",
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
            http_client: Mutex::new(None),
            web_bot_auth_signer: Mutex::new(None),
            cdp_renderer: Mutex::new(None),
        })
    }

    pub fn web_bot_auth_signer_for_url(
        &self,
        url: &url::Url,
    ) -> Result<Option<Arc<WebBotAuthSigner>>> {
        if self.config.no_web_bot_auth || url.scheme() != "https" {
            return Ok(None);
        }
        let Some(host) = url.host_str() else {
            return Ok(None);
        };
        if crate::policy::is_local_or_private_host(host) {
            return Ok(None);
        }
        let mut guard = self
            .web_bot_auth_signer
            .lock()
            .map_err(|_| anyhow::anyhow!("Web Bot Auth signer lock poisoned"))?;
        if let Some(ref cached) = *guard {
            return Ok(cached.clone());
        }
        let resolved = WebBotAuthSigner::from_state_dir(&self.config.state_dir)?.map(Arc::new);
        *guard = Some(resolved.clone());
        Ok(resolved)
    }

    pub fn configured_cdp_url(&self) -> Option<String> {
        self.config
            .cdp_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var("PLEB_CDP_URL")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    }

    pub fn cdp_renderer(&self) -> Option<Arc<CdpRenderer>> {
        if self.config.proxy.is_some() {
            return None;
        }
        let mut guard = self.cdp_renderer.lock().ok()?;
        if let Some(ref cached) = *guard {
            return cached.clone();
        }
        let renderer = self.configured_cdp_url().map(|cdp_url| {
            Arc::new(CdpRenderer::new(
                cdp_url,
                self.config.state_dir.clone(),
                self.config.proxy.clone(),
            ))
        });
        *guard = Some(renderer.clone());
        renderer
    }

    pub fn http_client(&self) -> Result<reqwest::Client> {
        let mut guard = self
            .http_client
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP client lock poisoned"))?;
        if let Some(ref client) = *guard {
            return Ok(client.clone());
        }
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("pleb/", env!("CARGO_PKG_VERSION")))
            .timeout(self.config.timeout);
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
    let artifacts_dir = state_dir.join("artifacts");
    if artifacts_dir.exists() {
        std::fs::remove_dir_all(&artifacts_dir)?;
    }
    let lock_file = acquire_private_lock(&state_dir.join("site-cache.lock"))?;

    let cache_file = state_dir.join("site-cache.json");
    match std::fs::remove_file(&cache_file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    drop(lock_file);

    crate::search::clear_search_state(state_dir)?;
    Ok(())
}
