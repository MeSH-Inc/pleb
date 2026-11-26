use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::rules::parent_domain;

const STRATEGY_TTL_SECS: u64 = 7 * 24 * 3600;
const TIMEOUT_TTL_SECS: u64 = 24 * 3600;
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SiteCacheEntry {
    pub strategy: String,
    pub avg_time_ms: u64,
    pub successes: u32,
    pub updated_at_epoch_secs: u64,
}

pub struct SiteCache {
    path: PathBuf,
    entries: Mutex<HashMap<String, SiteCacheEntry>>,
}

impl SiteCache {
    pub fn load(path: PathBuf) -> Self {
        let entries = match read_cache_map(&path) {
            Ok(map) => map,
            Err(CacheReadError::Corrupt(err)) => {
                tracing::warn!(
                    "site cache {} contains corrupted JSON, starting empty: {}",
                    path.display(),
                    err
                );
                HashMap::new()
            }
            Err(CacheReadError::Io(err)) => {
                tracing::warn!("failed to read site cache {}: {}", path.display(), err);
                HashMap::new()
            }
        };
        Self {
            path,
            entries: Mutex::new(entries),
        }
    }

    pub fn get(&self, host: &str) -> Option<SiteCacheEntry> {
        self.get_at(host, now_epoch_secs())
    }

    pub fn get_at(&self, host: &str, now_secs: u64) -> Option<SiteCacheEntry> {
        let guard = self.entries.lock().ok()?;
        let entry = guard
            .get(host)
            .or_else(|| parent_domain(host).and_then(|p| guard.get(p)))?;
        if is_expired(entry, now_secs) {
            None
        } else {
            Some(entry.clone())
        }
    }

    pub fn record(&self, host: &str, strategy: &str, time_ms: u64) {
        self.record_at(host, strategy, time_ms, now_epoch_secs());
    }

    pub fn record_at(&self, host: &str, strategy: &str, time_ms: u64, now_secs: u64) {
        let lock_file = match acquire_cache_lock(&self.path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!("failed to lock site cache {}: {}", self.path.display(), err);
                return;
            }
        };

        let mut merged = match read_cache_map(&self.path) {
            Ok(map) => map,
            Err(CacheReadError::Corrupt(err)) => {
                tracing::warn!(
                    "site cache {} contains corrupted JSON, rebuilding: {}",
                    self.path.display(),
                    err
                );
                HashMap::new()
            }
            Err(CacheReadError::Io(err)) => {
                tracing::warn!(
                    "failed to read site cache {} before update, refusing to overwrite: {}",
                    self.path.display(),
                    err
                );
                drop(lock_file);
                return;
            }
        };

        let entry = merged
            .entry(host.to_string())
            .or_insert_with(|| SiteCacheEntry {
                strategy: strategy.to_string(),
                avg_time_ms: time_ms,
                successes: 0,
                updated_at_epoch_secs: now_secs,
            });

        entry.successes = entry.successes.saturating_add(1);
        entry.avg_time_ms = (entry.avg_time_ms * 3 + time_ms) / 4;
        entry.updated_at_epoch_secs = now_secs;
        entry.strategy = strategy.to_string();

        if let Err(err) = save_atomic(&self.path, &merged) {
            tracing::warn!("failed to save site cache {}: {}", self.path.display(), err);
        }

        if let Ok(mut guard) = self.entries.lock() {
            *guard = merged;
        }
        drop(lock_file);
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        let lock_file = acquire_cache_lock(&self.path)?;
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        drop(lock_file);
        Ok(())
    }
}

enum CacheReadError {
    Io(std::io::Error),
    Corrupt(serde_json::Error),
}

fn read_cache_map(path: &Path) -> Result<HashMap<String, SiteCacheEntry>, CacheReadError> {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).map_err(CacheReadError::Corrupt),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => Err(CacheReadError::Io(err)),
    }
}

fn acquire_cache_lock(cache_path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = cache_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let lock_path = cache_path.with_file_name("site-cache.lock");
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(&lock_path)?;
    file.lock()?;
    Ok(file)
}

fn is_expired(entry: &SiteCacheEntry, now_secs: u64) -> bool {
    let ttl = match entry.strategy.as_str() {
        "cef_timeout" => TIMEOUT_TTL_SECS,
        _ => STRATEGY_TTL_SECS,
    };
    now_secs.saturating_sub(entry.updated_at_epoch_secs) > ttl
}

fn save_atomic(path: &Path, entries: &HashMap<String, SiteCacheEntry>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let data = serde_json::to_string_pretty(entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("site-cache.json");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{}.tmp.{}.{}", file_name, std::process::id(), seq));

    let write_res = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(data.as_bytes())?;
        file.flush()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
