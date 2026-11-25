use anyhow::Result;
use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MESH_WICK_HOME") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".mesh-wick")
}

pub fn storage_path() -> Result<PathBuf> {
    Ok(state_dir().join("data"))
}

pub fn clear() -> Result<()> {
    let path = storage_path()?;
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    Ok(())
}
