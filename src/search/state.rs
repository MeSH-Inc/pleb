use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::auth::CredentialProvider;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderHealthEntry {
    pub last_used_epoch_ms: u64,
    pub cooldown_until_epoch_secs: u64,
    pub month_key: String,
    pub month_requests: u32,
    pub last_error_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_fingerprint: Option<String>,
}

pub fn provider_health_summary(
    state_dir: &Path,
) -> Result<BTreeMap<&'static str, ProviderHealthEntry>> {
    let _ = state_dir;
    Ok(BTreeMap::new())
}

pub fn clear_search_state(state_dir: &Path) -> Result<()> {
    let _ = state_dir;
    Ok(())
}

pub fn clear_provider_cooldown(state_dir: &Path, provider: CredentialProvider) -> Result<()> {
    let _ = (state_dir, provider);
    Ok(())
}
