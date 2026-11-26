use anyhow::{bail, Result};
use serde::Serialize;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{compiler_fence, Ordering};

pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // SAFETY: Writing 0x00 bytes to a String's buffer keeps it valid UTF-8.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for byte in bytes {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CredentialProvider {
    Brave,
    Tavily,
    Serper,
    Exa,
    Searxng,
}

impl CredentialProvider {
    pub const ALL: &'static [Self] = &[
        Self::Brave,
        Self::Tavily,
        Self::Serper,
        Self::Exa,
        Self::Searxng,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Tavily => "tavily",
            Self::Serper => "serper",
            Self::Exa => "exa",
            Self::Searxng => "searxng",
        }
    }

    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Brave => "BRAVE_API_KEY",
            Self::Tavily => "TAVILY_API_KEY",
            Self::Serper => "SERPER_API_KEY",
            Self::Exa => "EXA_API_KEY",
            Self::Searxng => "SEARXNG_URL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    Environment,
    Keychain,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum CredentialStorage {
    #[default]
    Auto,
    Keychain,
    File,
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialStatus {
    pub provider: CredentialProvider,
    pub configured: bool,
    pub source: Option<CredentialSource>,
}

pub struct ResolvedCredential {
    pub provider: CredentialProvider,
    pub value: SecretString,
    pub source: CredentialSource,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("provider", &self.provider)
            .field("value", &self.value)
            .field("source", &self.source)
            .finish()
    }
}

pub fn resolve_credential(
    state_dir: &Path,
    provider: CredentialProvider,
) -> Result<Option<ResolvedCredential>> {
    let _ = state_dir;
    if let Ok(raw) = std::env::var(provider.env_var()) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Ok(Some(ResolvedCredential {
                provider,
                value: SecretString::new(trimmed.to_string()),
                source: CredentialSource::Environment,
            }));
        }
    }
    Ok(None)
}

pub fn set_credential(
    state_dir: &Path,
    provider: CredentialProvider,
    secret: &str,
    storage: CredentialStorage,
) -> Result<CredentialSource> {
    let _ = (state_dir, provider, secret, storage);
    bail!("credential storage is not yet implemented")
}

pub fn remove_credential(
    state_dir: &Path,
    provider: CredentialProvider,
    storage: CredentialStorage,
) -> Result<Vec<CredentialSource>> {
    let _ = (state_dir, provider, storage);
    bail!("credential storage is not yet implemented")
}

pub fn all_credential_statuses(state_dir: &Path) -> Result<Vec<CredentialStatus>> {
    let mut statuses = Vec::with_capacity(CredentialProvider::ALL.len());
    for &provider in CredentialProvider::ALL {
        let resolved = resolve_credential(state_dir, provider)?;
        statuses.push(CredentialStatus {
            provider,
            configured: resolved.is_some(),
            source: resolved.map(|r| r.source),
        });
    }
    Ok(statuses)
}
