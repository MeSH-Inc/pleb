use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::signature::Ed25519KeyPair;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::atomic::{compiler_fence, Ordering};

use crate::context::{acquire_private_lock, ensure_private_dir, write_private_file_atomic};

pub const MAX_SECRET_BYTES: usize = 4096;
const MAX_WEB_BOT_AUTH_AGENT_BYTES: usize = 512;

#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "pleb";
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = 44;

const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const CREDENTIALS_LOCK_NAME: &str = "credentials.lock";
const NO_KEYCHAIN_ENV: &str = "PLEB_NO_KEYCHAIN";

fn zeroize_bytes(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

fn zeroize_string(s: &mut str) {
    // SAFETY: Writing 0x00 bytes to a str's buffer keeps it valid UTF-8.
    let bytes = unsafe { s.as_bytes_mut() };
    zeroize_bytes(bytes);
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
        zeroize_string(&mut self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialProvider {
    Brave,
    Tavily,
    Serper,
    Exa,
    Searxng,
    WebBotAuthJwk,
    WebBotAuthAgent,
    CdpAccessClientId,
    CdpAccessClientSecret,
    CdpAccessHost,
}

impl CredentialProvider {
    pub const ALL: &'static [Self] = &[
        Self::Brave,
        Self::Tavily,
        Self::Serper,
        Self::Exa,
        Self::Searxng,
        Self::WebBotAuthJwk,
        Self::WebBotAuthAgent,
        Self::CdpAccessClientId,
        Self::CdpAccessClientSecret,
        Self::CdpAccessHost,
    ];

    pub const SEARCH_PROVIDERS: &'static [Self] = &[
        Self::Brave,
        Self::Tavily,
        Self::Serper,
        Self::Exa,
        Self::Searxng,
    ];

    pub const fn is_search_provider(self) -> bool {
        matches!(
            self,
            Self::Brave | Self::Tavily | Self::Serper | Self::Exa | Self::Searxng
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Tavily => "tavily",
            Self::Serper => "serper",
            Self::Exa => "exa",
            Self::Searxng => "searxng",
            Self::WebBotAuthJwk => "web-bot-auth-jwk",
            Self::WebBotAuthAgent => "web-bot-auth-agent",
            Self::CdpAccessClientId => "cdp-access-client-id",
            Self::CdpAccessClientSecret => "cdp-access-client-secret",
            Self::CdpAccessHost => "cdp-access-host",
        }
    }

    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Brave => "BRAVE_API_KEY",
            Self::Tavily => "TAVILY_API_KEY",
            Self::Serper => "SERPER_API_KEY",
            Self::Exa => "EXA_API_KEY",
            Self::Searxng => "SEARXNG_URL",
            Self::WebBotAuthJwk => "PLEB_WEB_BOT_AUTH_JWK",
            Self::WebBotAuthAgent => "PLEB_WEB_BOT_AUTH_AGENT",
            Self::CdpAccessClientId => "PLEB_CDP_ACCESS_CLIENT_ID",
            Self::CdpAccessClientSecret => "PLEB_CDP_ACCESS_CLIENT_SECRET",
            Self::CdpAccessHost => "PLEB_CDP_ACCESS_HOST",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentIdentityStatus {
    pub web_bot_auth_enabled: bool,
    pub jwk_configured: bool,
    pub jwk_source: Option<CredentialSource>,
    pub agent_configured: bool,
    pub agent_source: Option<CredentialSource>,
    pub keyid: Option<String>,
    pub status_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CdpAccessStatus {
    pub configured: bool,
    pub client_id_configured: bool,
    pub client_id_source: Option<CredentialSource>,
    pub client_secret_configured: bool,
    pub client_secret_source: Option<CredentialSource>,
    pub host_configured: bool,
    pub host_source: Option<CredentialSource>,
    pub access_host: Option<String>,
    pub status_error: Option<String>,
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

pub fn scrub_sensitive_env(cmd: &mut std::process::Command) {
    for provider in CredentialProvider::ALL {
        cmd.env_remove(provider.env_var());
    }
    cmd.env_remove("PLEB_CDP_URL");
}

fn trim_secret_edges(raw: &str) -> &str {
    raw.trim_matches([' ', '\r', '\n'])
}

pub(crate) struct ValidatedEd25519Jwk {
    pub(crate) canonical_json: SecretString,
    pub(crate) key_id: String,
    pub(crate) keypair: Ed25519KeyPair,
}

#[derive(serde::Deserialize)]
struct RawEd25519Jwk {
    kty: String,
    crv: String,
    x: String,
    d: String,
}

impl Drop for RawEd25519Jwk {
    fn drop(&mut self) {
        zeroize_string(&mut self.kty);
        zeroize_string(&mut self.crv);
        zeroize_string(&mut self.x);
        zeroize_string(&mut self.d);
    }
}

pub(crate) fn parse_ed25519_jwk(raw: &str) -> Result<ValidatedEd25519Jwk> {
    let trimmed = raw.trim_matches([' ', '\t', '\r', '\n']);
    if trimmed.is_empty() {
        bail!("credential for web-bot-auth-jwk must not be empty");
    }
    if trimmed.len() > MAX_SECRET_BYTES {
        bail!(
            "credential for web-bot-auth-jwk exceeds maximum length of {} bytes",
            MAX_SECRET_BYTES
        );
    }
    if trimmed
        .bytes()
        .any(|b| !b.is_ascii() || (b.is_ascii_control() && !matches!(b, b'\n' | b'\r' | b'\t')))
    {
        bail!("credential for web-bot-auth-jwk contains invalid control or non-ASCII characters");
    }

    let parsed: RawEd25519Jwk = serde_json::from_str(trimmed).map_err(|_| {
        anyhow::anyhow!("invalid web-bot-auth-jwk: malformed or incomplete Ed25519 JWK JSON")
    })?;
    if parsed.kty != "OKP" || parsed.crv != "Ed25519" {
        bail!("invalid web-bot-auth-jwk: expected kty=\"OKP\" and crv=\"Ed25519\"");
    }

    let mut d_bytes = ZeroizingBytes(vec![0u8; 32]);
    let d_len = URL_SAFE_NO_PAD
        .decode_slice(parsed.d.as_bytes(), &mut d_bytes.0)
        .map_err(|_| anyhow::anyhow!("invalid web-bot-auth-jwk: d must be valid base64url"))?;
    if d_len != 32 {
        bail!("invalid web-bot-auth-jwk: d must decode to 32 bytes");
    }

    let mut x_bytes = [0u8; 32];
    let x_len = URL_SAFE_NO_PAD
        .decode_slice(parsed.x.as_bytes(), &mut x_bytes)
        .map_err(|_| anyhow::anyhow!("invalid web-bot-auth-jwk: x must be valid base64url"))?;
    if x_len != 32 {
        bail!("invalid web-bot-auth-jwk: x must decode to 32 bytes");
    }

    let keypair = Ed25519KeyPair::from_seed_and_public_key(&d_bytes.0, &x_bytes).map_err(|_| {
        anyhow::anyhow!("invalid web-bot-auth-jwk: Ed25519 keypair verification failed")
    })?;

    let d_canon = SecretString::new(URL_SAFE_NO_PAD.encode(&d_bytes.0));
    let x_canon = URL_SAFE_NO_PAD.encode(x_bytes);
    let canonical_json = SecretString::new(format!(
        r#"{{"crv":"Ed25519","d":"{}","kty":"OKP","x":"{}"}}"#,
        d_canon.as_str(),
        x_canon
    ));
    let key_id = crate::transport::web_bot_auth::ed25519_jwk_thumbprint(&x_bytes);

    Ok(ValidatedEd25519Jwk {
        canonical_json,
        key_id,
        keypair,
    })
}

fn validate_web_bot_auth_agent(trimmed: &str) -> Result<SecretString> {
    if trimmed.len() > MAX_WEB_BOT_AUTH_AGENT_BYTES {
        bail!(
            "credential for web-bot-auth-agent exceeds maximum length of {} bytes",
            MAX_WEB_BOT_AUTH_AGENT_BYTES
        );
    }
    if trimmed
        .bytes()
        .any(|b| b.is_ascii_whitespace() || matches!(b, b'"' | b'\\'))
    {
        bail!("invalid web-bot-auth-agent: URL contains disallowed whitespace or quote characters");
    }
    let parsed = url::Url::parse(trimmed)
        .map_err(|_| anyhow::anyhow!("invalid web-bot-auth-agent: must be a valid https URL"))?;
    if parsed.scheme() != "https" {
        bail!("invalid web-bot-auth-agent: scheme must be https");
    }
    let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
        bail!("invalid web-bot-auth-agent: URL must include a host");
    };
    if crate::policy::is_local_or_private_host(host) {
        bail!("invalid web-bot-auth-agent: local or private hosts are not allowed");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("invalid web-bot-auth-agent: embedded userinfo credentials are not allowed");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("invalid web-bot-auth-agent: query strings and fragments are not allowed");
    }
    Ok(SecretString::new(trimmed.to_string()))
}

fn parse_nonzero_port(raw: &str) -> Result<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid cdp-access-host: port must be a valid TCP port");
    }
    let port: u16 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid cdp-access-host: port out of range"))?;
    if port == 0 {
        bail!("invalid cdp-access-host: port must be non-zero");
    }
    Ok(port)
}

fn validate_cdp_access_host(trimmed: &str) -> Result<SecretString> {
    if trimmed.bytes().any(|b| {
        b.is_ascii_whitespace() || matches!(b, b'"' | b'\\' | b'/' | b'?' | b'#' | b'@' | b'*')
    }) {
        bail!("invalid cdp-access-host: must be a hostname or host:port without scheme, path, userinfo, or wildcard");
    }

    let (canonical_host, port) = if let Ok(ip) = trimmed.parse::<IpAddr>() {
        let host = match ip {
            IpAddr::V6(v6) => format!("[{v6}]"),
            IpAddr::V4(v4) => v4.to_string(),
        };
        (host, None)
    } else if let Some(rest_after_open) = trimmed.strip_prefix('[') {
        let Some((inside, rest)) = rest_after_open.split_once(']') else {
            bail!("invalid cdp-access-host: malformed IPv6 literal");
        };
        let v6: Ipv6Addr = inside
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid cdp-access-host: invalid IPv6 address"))?;
        let port = if rest.is_empty() {
            None
        } else if let Some(p_str) = rest.strip_prefix(':') {
            Some(parse_nonzero_port(p_str)?)
        } else {
            bail!("invalid cdp-access-host: unexpected characters after IPv6 literal");
        };
        (format!("[{v6}]"), port)
    } else {
        let (host_raw, port) = if let Some((h, p)) = trimmed.split_once(':') {
            if h.is_empty() || p.contains(':') {
                bail!("invalid cdp-access-host: malformed host:port");
            }
            (h, Some(parse_nonzero_port(p)?))
        } else {
            (trimmed, None)
        };
        let parsed = url::Url::parse(&format!("https://{host_raw}"))
            .map_err(|_| anyhow::anyhow!("invalid cdp-access-host: invalid hostname"))?;
        if parsed.path() != "/"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || parsed.port().is_some()
        {
            bail!("invalid cdp-access-host: invalid hostname");
        }
        let host = match parsed.host() {
            Some(url::Host::Domain(d)) => {
                if d.is_empty() || d.starts_with('.') || d.ends_with('.') || d.contains("..") {
                    bail!("invalid cdp-access-host: invalid hostname");
                }
                d.to_ascii_lowercase()
            }
            Some(url::Host::Ipv4(ip)) => ip.to_string(),
            Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
            None => bail!("invalid cdp-access-host: missing hostname"),
        };
        (host, port)
    };

    if crate::policy::is_local_or_private_host(&canonical_host) {
        bail!("invalid cdp-access-host: local or private hosts are not allowed");
    }

    let canonical = match port {
        Some(p) => format!("{canonical_host}:{p}"),
        None => canonical_host,
    };
    Ok(SecretString::new(canonical))
}

pub fn validate_credential(provider: CredentialProvider, raw: &str) -> Result<SecretString> {
    if provider == CredentialProvider::WebBotAuthJwk {
        return parse_ed25519_jwk(raw).map(|parsed| parsed.canonical_json);
    }

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

    match provider {
        CredentialProvider::Searxng => {
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
            Ok(SecretString::new(trimmed.to_string()))
        }
        CredentialProvider::WebBotAuthAgent => validate_web_bot_auth_agent(trimmed),
        CredentialProvider::CdpAccessHost => validate_cdp_access_host(trimmed),
        CredentialProvider::CdpAccessClientId | CredentialProvider::CdpAccessClientSecret => {
            if trimmed.bytes().any(|b| b.is_ascii_whitespace()) {
                bail!(
                    "credential for {} must not contain whitespace",
                    provider.as_str()
                );
            }
            Ok(SecretString::new(trimmed.to_string()))
        }
        CredentialProvider::WebBotAuthJwk => unreachable!("handled above"),
        CredentialProvider::Brave
        | CredentialProvider::Tavily
        | CredentialProvider::Serper
        | CredentialProvider::Exa => Ok(SecretString::new(trimmed.to_string())),
    }
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
            zeroize_string(value);
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
        let is_blank = if provider == CredentialProvider::WebBotAuthJwk {
            raw.as_str()
                .trim_matches([' ', '\t', '\r', '\n'])
                .is_empty()
        } else {
            trim_secret_edges(raw.as_str()).is_empty()
        };
        if !is_blank {
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

fn resolve_provider_results(
    state_dir: &Path,
    providers: &[CredentialProvider],
) -> Result<Vec<(CredentialProvider, Result<Option<ResolvedCredential>>)>> {
    if !keychain_enabled() {
        let mut out = Vec::with_capacity(providers.len());
        for &provider in providers {
            out.push((provider, resolve_credential(state_dir, provider)));
        }
        return Ok(out);
    }

    std::thread::scope(|scope| {
        let handles: Vec<_> = providers
            .iter()
            .copied()
            .map(|provider| {
                scope.spawn(move || (provider, resolve_credential(state_dir, provider)))
            })
            .collect();

        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            let pair = handle
                .join()
                .map_err(|_| anyhow::anyhow!("credential resolution worker panicked"))?;
            out.push(pair);
        }
        Ok(out)
    })
}

pub fn resolve_credentials(
    state_dir: &Path,
    providers: &[CredentialProvider],
) -> Result<BTreeMap<CredentialProvider, ResolvedCredential>> {
    let entries = resolve_provider_results(state_dir, providers)?;
    let mut map = BTreeMap::new();
    for (provider, res) in entries {
        if let Some(cred) = res? {
            map.insert(provider, cred);
        }
    }
    Ok(map)
}

pub fn resolve_all_credentials(
    state_dir: &Path,
) -> Result<Vec<(CredentialProvider, Option<ResolvedCredential>)>> {
    let resolved = resolve_credentials(state_dir, CredentialProvider::ALL)?;
    let mut map = resolved;
    Ok(CredentialProvider::ALL
        .iter()
        .copied()
        .map(|provider| (provider, map.remove(&provider)))
        .collect())
}

pub fn all_credential_statuses(state_dir: &Path) -> Result<Vec<CredentialStatus>> {
    let resolved = resolve_all_credentials(state_dir)?;
    Ok(resolved
        .into_iter()
        .map(|(provider, cred)| CredentialStatus {
            provider,
            configured: cred.is_some(),
            source: cred.map(|r| r.source),
        })
        .collect())
}

pub fn resolve_agent_identity_status(
    state_dir: &Path,
    no_web_bot_auth: bool,
) -> AgentIdentityStatus {
    let mut status = AgentIdentityStatus {
        web_bot_auth_enabled: !no_web_bot_auth,
        ..Default::default()
    };
    let mut errors = Vec::new();
    let results = match resolve_provider_results(
        state_dir,
        &[
            CredentialProvider::WebBotAuthJwk,
            CredentialProvider::WebBotAuthAgent,
        ],
    ) {
        Ok(res) => res,
        Err(err) => {
            status.status_error = Some(err.to_string());
            return status;
        }
    };

    for (provider, res) in results {
        match (provider, res) {
            (CredentialProvider::WebBotAuthJwk, Ok(Some(cred))) => {
                status.jwk_configured = true;
                status.jwk_source = Some(cred.source);
                status.keyid = parse_ed25519_jwk(cred.value.as_str())
                    .ok()
                    .map(|jwk| jwk.key_id);
            }
            (CredentialProvider::WebBotAuthAgent, Ok(Some(cred))) => {
                status.agent_configured = true;
                status.agent_source = Some(cred.source);
            }
            (_, Ok(_)) => {}
            (_, Err(err)) => errors.push(err.to_string()),
        }
    }

    if !errors.is_empty() {
        status.status_error = Some(errors.join("; "));
    } else if status.jwk_configured && !status.agent_configured {
        status.status_error = Some(
            "incomplete Web Bot Auth configuration: run `pleb auth set web-bot-auth-agent`"
                .to_string(),
        );
    } else if !status.jwk_configured && status.agent_configured {
        status.status_error = Some(
            "incomplete Web Bot Auth configuration: run `pleb auth set web-bot-auth-jwk`"
                .to_string(),
        );
    }

    status
}

pub fn resolve_cdp_access_status(state_dir: &Path) -> CdpAccessStatus {
    let mut status = CdpAccessStatus::default();
    let mut errors = Vec::new();
    let results = match resolve_provider_results(
        state_dir,
        &[
            CredentialProvider::CdpAccessClientId,
            CredentialProvider::CdpAccessClientSecret,
            CredentialProvider::CdpAccessHost,
        ],
    ) {
        Ok(res) => res,
        Err(err) => {
            status.status_error = Some(err.to_string());
            return status;
        }
    };

    for (provider, res) in results {
        match (provider, res) {
            (CredentialProvider::CdpAccessClientId, Ok(Some(cred))) => {
                status.client_id_configured = true;
                status.client_id_source = Some(cred.source);
            }
            (CredentialProvider::CdpAccessClientSecret, Ok(Some(cred))) => {
                status.client_secret_configured = true;
                status.client_secret_source = Some(cred.source);
            }
            (CredentialProvider::CdpAccessHost, Ok(Some(cred))) => {
                status.host_configured = true;
                status.host_source = Some(cred.source);
                status.access_host = Some(cred.value.as_str().to_string());
            }
            (_, Ok(_)) => {}
            (_, Err(err)) => errors.push(err.to_string()),
        }
    }

    if !errors.is_empty() {
        status.status_error = Some(errors.join("; "));
    } else if status.client_id_configured
        && status.client_secret_configured
        && status.host_configured
    {
        status.configured = true;
    } else if status.client_id_configured
        || status.client_secret_configured
        || status.host_configured
    {
        let mut missing = Vec::new();
        if !status.client_id_configured {
            missing.push(CredentialProvider::CdpAccessClientId.as_str());
        }
        if !status.client_secret_configured {
            missing.push(CredentialProvider::CdpAccessClientSecret.as_str());
        }
        if !status.host_configured {
            missing.push(CredentialProvider::CdpAccessHost.as_str());
        }
        status.status_error = Some(format!(
            "incomplete CDP Access configuration: missing {}",
            missing.join(", ")
        ));
    }

    status
}
