use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{compiler_fence, Ordering};

use crate::context::{acquire_private_lock, ensure_private_dir, write_private_file_atomic};

pub const MAX_SECRET_BYTES: usize = 4096;

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "mesh-wick";
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = 44;

const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const CREDENTIALS_LOCK_NAME: &str = "credentials.lock";
const NO_KEYCHAIN_ENV: &str = "WICK_NO_KEYCHAIN";

fn zeroize_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

struct ZeroizingBytes(Vec<u8>);

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        zeroize_bytes(&mut self.0);
    }
}

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
        zeroize_bytes(bytes);
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

fn trim_secret_edges(raw: &str) -> &str {
    raw.trim_matches([' ', '\r', '\n'])
}

pub fn validate_credential(provider: CredentialProvider, raw: &str) -> Result<SecretString> {
    let trimmed = trim_secret_edges(raw);
    if trimmed.is_empty() {
        bail!("credential for {} must not be empty", provider.as_str());
    }
    if trimmed.len() > MAX_SECRET_BYTES {
        bail!(
            "credential for {} exceeds maximum length of {} bytes",
            provider.as_str(),
            MAX_SECRET_BYTES
        );
    }
    if trimmed
        .bytes()
        .any(|b| !b.is_ascii() || b.is_ascii_control())
    {
        bail!(
            "credential for {} must contain printable ASCII characters only",
            provider.as_str()
        );
    }
    if provider == CredentialProvider::Searxng {
        let parsed = url::Url::parse(trimmed).map_err(|_| {
            anyhow::anyhow!("invalid SEARXNG_URL: must be a valid http or https URL")
        })?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            bail!("invalid SEARXNG_URL: scheme must be http or https");
        }
        if parsed.host_str().is_none() {
            bail!("invalid SEARXNG_URL: URL must include a host");
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            bail!("invalid SEARXNG_URL: embedded userinfo credentials are not allowed");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("invalid SEARXNG_URL: query strings and fragments are not allowed");
        }
    }
    Ok(SecretString::new(trimmed.to_string()))
}

fn keychain_disabled() -> bool {
    std::env::var(NO_KEYCHAIN_ENV).ok().as_deref() == Some("1")
}

fn keychain_enabled() -> bool {
    cfg!(target_os = "macos") && !keychain_disabled()
}

fn require_keychain_available() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("keychain credential storage is only supported on macOS");
    }
    if keychain_disabled() {
        bail!("keychain credential storage is disabled by {NO_KEYCHAIN_ENV}=1");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn encode_lowercase_hex(bytes: &[u8]) -> ZeroizingBytes {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0f) as usize]);
    }
    ZeroizingBytes(out)
}

#[cfg(target_os = "macos")]
fn keychain_error(
    action: &str,
    provider: CredentialProvider,
    output: &std::process::Output,
) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        anyhow::anyhow!(
            "failed to {action} {} credential in macOS Keychain (exit {:?})",
            provider.as_str(),
            output.status.code()
        )
    } else {
        anyhow::anyhow!(
            "failed to {action} {} credential in macOS Keychain (exit {:?}): {detail}",
            provider.as_str(),
            output.status.code()
        )
    }
}

#[cfg(target_os = "macos")]
fn keychain_set(provider: CredentialProvider, secret: &SecretString) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let hex = encode_lowercase_hex(secret.as_str().as_bytes());
    let mut cmd_buf = Vec::with_capacity(64 + hex.0.len());
    cmd_buf.extend_from_slice(b"add-generic-password -U -a ");
    cmd_buf.extend_from_slice(provider.as_str().as_bytes());
    cmd_buf.extend_from_slice(b" -s ");
    cmd_buf.extend_from_slice(KEYCHAIN_SERVICE.as_bytes());
    cmd_buf.extend_from_slice(b" -X ");
    cmd_buf.extend_from_slice(&hex.0);
    cmd_buf.push(b'\n');
    let cmd_buf = ZeroizingBytes(cmd_buf);
    drop(hex);

    let mut child = Command::new("/usr/bin/security")
        .args(["-q", "-i"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to spawn /usr/bin/security: {err}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&cmd_buf.0)?;
        stdin.flush()?;
    }
    drop(cmd_buf);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(keychain_error("store", provider, &output));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_get(provider: CredentialProvider) -> Result<Option<SecretString>> {
    use std::process::{Command, Stdio};

    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-a",
            provider.as_str(),
            "-s",
            KEYCHAIN_SERVICE,
            "-w",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| anyhow::anyhow!("failed to run /usr/bin/security: {err}"))?;

    if output.status.success() {
        let stdout_buf = ZeroizingBytes(output.stdout);
        let raw = std::str::from_utf8(&stdout_buf.0).map_err(|_| {
            anyhow::anyhow!(
                "macOS Keychain entry for {} is not valid UTF-8",
                provider.as_str()
            )
        })?;
        return Ok(Some(validate_credential(provider, raw)?));
    }

    if output.status.code() == Some(ERR_SEC_ITEM_NOT_FOUND) {
        return Ok(None);
    }

    Err(keychain_error("read", provider, &output))
}

#[cfg(target_os = "macos")]
fn keychain_delete(provider: CredentialProvider) -> Result<bool> {
    use std::process::{Command, Stdio};

    let output = Command::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-a",
            provider.as_str(),
            "-s",
            KEYCHAIN_SERVICE,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| anyhow::anyhow!("failed to run /usr/bin/security: {err}"))?;

    if output.status.success() {
        return Ok(true);
    }
    if output.status.code() == Some(ERR_SEC_ITEM_NOT_FOUND) {
        return Ok(false);
    }

    Err(keychain_error("delete", provider, &output))
}

#[cfg(not(target_os = "macos"))]
fn keychain_set(_provider: CredentialProvider, _secret: &SecretString) -> Result<()> {
    require_keychain_available()
}

#[cfg(not(target_os = "macos"))]
fn keychain_get(_provider: CredentialProvider) -> Result<Option<SecretString>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn keychain_delete(_provider: CredentialProvider) -> Result<bool> {
    require_keychain_available()?;
    Ok(false)
}

struct ZeroizingCredentialMap(BTreeMap<String, String>);

impl Drop for ZeroizingCredentialMap {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            // SAFETY: Writing 0x00 bytes to a String's buffer keeps it valid UTF-8.
            let bytes = unsafe { value.as_bytes_mut() };
            zeroize_bytes(bytes);
        }
    }
}

fn read_credentials_map_locked(creds_path: &Path) -> Result<(ZeroizingCredentialMap, bool)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match opts.open(creds_path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((ZeroizingCredentialMap(BTreeMap::new()), false));
        }
        Err(err) => {
            bail!(
                "failed to open credentials file {}: {}",
                creds_path.display(),
                err
            );
        }
    };

    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!(
            "refusing to read credentials from non-regular file {}",
            creds_path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current_uid = unsafe { libc::geteuid() };
        if meta.uid() != current_uid {
            bail!(
                "refusing to read {} owned by uid {} (expected uid {})",
                creds_path.display(),
                meta.uid(),
                current_uid
            );
        }
        if meta.mode() & 0o077 != 0 {
            bail!(
                "refusing to read {} with insecure permissions {:04o} (expected 0600)",
                creds_path.display(),
                meta.mode() & 0o7777
            );
        }
    }

    let mut raw_bytes = Vec::new();
    file.read_to_end(&mut raw_bytes)?;
    let raw_bytes = ZeroizingBytes(raw_bytes);
    let parsed: BTreeMap<String, String> = serde_json::from_slice(&raw_bytes.0).map_err(|err| {
        anyhow::anyhow!(
            "corrupt credentials file {}: invalid JSON ({err})",
            creds_path.display()
        )
    })?;
    Ok((ZeroizingCredentialMap(parsed), true))
}

fn write_credentials_map_locked(creds_path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    let mut json_bytes = serde_json::to_vec_pretty(map)?;
    json_bytes.push(b'\n');
    let json_bytes = ZeroizingBytes(json_bytes);
    write_private_file_atomic(creds_path, &json_bytes.0)?;
    Ok(())
}

fn file_get(state_dir: &Path, provider: CredentialProvider) -> Result<Option<SecretString>> {
    match std::fs::symlink_metadata(state_dir) {
        Ok(_) => ensure_private_dir(state_dir)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
    let (mut map, _) = read_credentials_map_locked(&state_dir.join(CREDENTIALS_FILE_NAME))?;
    let Some(raw) = map.0.remove(provider.as_str()) else {
        return Ok(None);
    };
    let raw = SecretString::new(raw);
    let validated = validate_credential(provider, raw.as_str())?;
    Ok(Some(validated))
}

fn file_set(state_dir: &Path, provider: CredentialProvider, secret: &SecretString) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
    let creds_path = state_dir.join(CREDENTIALS_FILE_NAME);
    let (mut map, _) = read_credentials_map_locked(&creds_path)?;
    map.0
        .insert(provider.as_str().to_string(), secret.as_str().to_string());
    write_credentials_map_locked(&creds_path, &map.0)
}

fn file_delete(state_dir: &Path, provider: CredentialProvider) -> Result<bool> {
    match std::fs::symlink_metadata(state_dir) {
        Ok(_) => ensure_private_dir(state_dir)?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    }
    let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
    let creds_path = state_dir.join(CREDENTIALS_FILE_NAME);
    let (mut map, existed) = read_credentials_map_locked(&creds_path)?;
    if !existed {
        return Ok(false);
    }
    let Some(removed_val) = map.0.remove(provider.as_str()) else {
        return Ok(false);
    };
    drop(SecretString::new(removed_val));
    write_credentials_map_locked(&creds_path, &map.0)?;
    Ok(true)
}

pub fn resolve_credential(
    state_dir: &Path,
    provider: CredentialProvider,
) -> Result<Option<ResolvedCredential>> {
    if let Ok(raw) = std::env::var(provider.env_var()) {
        let raw = SecretString::new(raw);
        if !trim_secret_edges(raw.as_str()).is_empty() {
            let value = validate_credential(provider, raw.as_str())?;
            return Ok(Some(ResolvedCredential {
                provider,
                value,
                source: CredentialSource::Environment,
            }));
        }
    }

    if keychain_enabled() {
        if let Some(value) = keychain_get(provider)? {
            return Ok(Some(ResolvedCredential {
                provider,
                value,
                source: CredentialSource::Keychain,
            }));
        }
    }

    if let Some(value) = file_get(state_dir, provider)? {
        return Ok(Some(ResolvedCredential {
            provider,
            value,
            source: CredentialSource::File,
        }));
    }

    Ok(None)
}

pub fn set_credential(
    state_dir: &Path,
    provider: CredentialProvider,
    secret: &str,
    storage: CredentialStorage,
) -> Result<CredentialSource> {
    let validated = validate_credential(provider, secret)?;
    match storage {
        CredentialStorage::Keychain => {
            require_keychain_available()?;
            keychain_set(provider, &validated)?;
            Ok(CredentialSource::Keychain)
        }
        CredentialStorage::File => {
            file_set(state_dir, provider, &validated)?;
            Ok(CredentialSource::File)
        }
        CredentialStorage::Auto => {
            if keychain_enabled() {
                keychain_set(provider, &validated)?;
                Ok(CredentialSource::Keychain)
            } else {
                file_set(state_dir, provider, &validated)?;
                Ok(CredentialSource::File)
            }
        }
    }
}

pub fn remove_credential(
    state_dir: &Path,
    provider: CredentialProvider,
    storage: CredentialStorage,
) -> Result<Vec<CredentialSource>> {
    match storage {
        CredentialStorage::Keychain => {
            require_keychain_available()?;
            if keychain_delete(provider)? {
                Ok(vec![CredentialSource::Keychain])
            } else {
                Ok(vec![])
            }
        }
        CredentialStorage::File => {
            if file_delete(state_dir, provider)? {
                Ok(vec![CredentialSource::File])
            } else {
                Ok(vec![])
            }
        }
        CredentialStorage::Auto => {
            let mut removed = Vec::new();
            if keychain_enabled() && keychain_delete(provider)? {
                removed.push(CredentialSource::Keychain);
            }
            if file_delete(state_dir, provider)? {
                removed.push(CredentialSource::File);
            }
            Ok(removed)
        }
    }
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
