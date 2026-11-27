use anyhow::{bail, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::Ed25519KeyPair;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::atomic::{compiler_fence, Ordering};

use crate::context::{acquire_private_lock, ensure_private_dir, write_private_file_atomic};

pub const MAX_SECRET_BYTES: usize = 4096;
const MAX_WEB_BOT_AUTH_AGENT_BYTES: usize = 512;

const KEYCHAIN_SERVICE: &str = "pleb";
const KEYCHAIN_VAULT_ACCOUNT: &str = "vault-key";
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = 44;

const CREDENTIALS_FILE_NAME: &str = "credentials.json";
const VAULT_FILE_NAME: &str = "credentials.enc";
const CREDENTIALS_LOCK_NAME: &str = "credentials.lock";
const NO_KEYCHAIN_ENV: &str = "PLEB_NO_KEYCHAIN";
const VAULT_MAGIC: &[u8; 8] = b"PLEBVLT1";
const VAULT_NONCE_LEN: usize = 12;
const VAULT_KEY_LEN: usize = 32;
const VAULT_AAD: &[u8] = b"pleb-vault-v1";

/// Overwrites sensitive bytes in place with a compiler fence.
pub fn zeroize_bytes(bytes: &mut [u8]) {
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

fn is_truthy_env_flag(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
}

fn keychain_disabled() -> bool {
    std::env::var(NO_KEYCHAIN_ENV)
        .ok()
        .is_some_and(|v| is_truthy_env_flag(&v))
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

fn encode_lowercase_hex(bytes: &[u8]) -> ZeroizingBytes {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0f) as usize]);
    }
    ZeroizingBytes(out)
}

#[cfg(any(target_os = "macos", test))]
fn decode_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(any(target_os = "macos", test))]
fn decode_hex_vault_key(raw: &str) -> Result<ZeroizingBytes> {
    let trimmed = trim_secret_edges(raw);
    if trimmed.len() != VAULT_KEY_LEN * 2 {
        bail!("invalid credentials vault key in macOS Keychain: unexpected length");
    }
    let mut out = ZeroizingBytes(Vec::with_capacity(VAULT_KEY_LEN));
    for &[hi_b, lo_b] in trimmed.as_bytes().as_chunks::<2>().0 {
        let (Some(hi), Some(lo)) = (decode_hex_nibble(hi_b), decode_hex_nibble(lo_b)) else {
            bail!("invalid credentials vault key in macOS Keychain: malformed key encoding");
        };
        out.0.push((hi << 4) | lo);
    }
    Ok(out)
}

#[cfg(target_os = "macos")]
fn keychain_error(action: &str, account: &str, output: &std::process::Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        anyhow::anyhow!(
            "failed to {action} {account} in macOS Keychain (exit {:?})",
            output.status.code()
        )
    } else {
        anyhow::anyhow!(
            "failed to {action} {account} in macOS Keychain (exit {:?}): {detail}",
            output.status.code()
        )
    }
}

#[cfg(target_os = "macos")]
fn keychain_set_vault_key(key: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if key.len() != VAULT_KEY_LEN {
        bail!("invalid credentials vault key length");
    }
    // Store the 32-byte key as 64 printable ASCII hex characters encoded into
    // `security -X` so `find-generic-password -w` always emits valid UTF-8.
    let key_ascii_hex = encode_lowercase_hex(key);
    let cmd_hex = encode_lowercase_hex(&key_ascii_hex.0);
    drop(key_ascii_hex);

    let mut cmd_buf = Vec::with_capacity(64 + cmd_hex.0.len());
    cmd_buf.extend_from_slice(b"add-generic-password -U -a ");
    cmd_buf.extend_from_slice(KEYCHAIN_VAULT_ACCOUNT.as_bytes());
    cmd_buf.extend_from_slice(b" -s ");
    cmd_buf.extend_from_slice(KEYCHAIN_SERVICE.as_bytes());
    cmd_buf.extend_from_slice(b" -X ");
    cmd_buf.extend_from_slice(&cmd_hex.0);
    cmd_buf.push(b'\n');
    let cmd_buf = ZeroizingBytes(cmd_buf);
    drop(cmd_hex);

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
        return Err(keychain_error("store", KEYCHAIN_VAULT_ACCOUNT, &output));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn keychain_get_vault_key() -> Result<Option<ZeroizingBytes>> {
    use std::process::{Command, Stdio};

    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-a",
            KEYCHAIN_VAULT_ACCOUNT,
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
            anyhow::anyhow!("macOS Keychain entry for {KEYCHAIN_VAULT_ACCOUNT} is not valid UTF-8")
        })?;
        return decode_hex_vault_key(raw).map(Some);
    }

    if output.status.code() == Some(ERR_SEC_ITEM_NOT_FOUND) {
        return Ok(None);
    }

    Err(keychain_error("read", KEYCHAIN_VAULT_ACCOUNT, &output))
}

#[cfg(target_os = "macos")]
fn keychain_get_or_create_vault_key() -> Result<ZeroizingBytes> {
    if let Some(existing) = keychain_get_vault_key()? {
        return Ok(existing);
    }
    let mut key = ZeroizingBytes(vec![0u8; VAULT_KEY_LEN]);
    SystemRandom::new()
        .fill(&mut key.0)
        .map_err(|_| anyhow::anyhow!("failed to generate credentials vault master key"))?;
    keychain_set_vault_key(&key.0)?;
    Ok(key)
}

#[cfg(not(target_os = "macos"))]
fn keychain_get_vault_key() -> Result<Option<ZeroizingBytes>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn keychain_get_or_create_vault_key() -> Result<ZeroizingBytes> {
    require_keychain_available()?;
    unreachable!("require_keychain_available always errors off macOS")
}

trait VaultKeyStore {
    fn get_key(&self) -> Result<Option<ZeroizingBytes>>;
    fn get_or_create_key(&self) -> Result<ZeroizingBytes>;
}

struct SystemKeychainStore;

impl VaultKeyStore for SystemKeychainStore {
    fn get_key(&self) -> Result<Option<ZeroizingBytes>> {
        keychain_get_vault_key()
    }

    fn get_or_create_key(&self) -> Result<ZeroizingBytes> {
        keychain_get_or_create_vault_key()
    }
}

struct ZeroizingCredentialMap(BTreeMap<String, String>);

impl Drop for ZeroizingCredentialMap {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            zeroize_string(value);
        }
    }
}

fn read_private_file_bytes_locked(path: &Path) -> Result<Option<ZeroizingBytes>> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match opts.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            bail!("failed to open credentials file {}: {err}", path.display());
        }
    };

    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!(
            "refusing to read credentials from non-regular file {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current_uid = unsafe { libc::geteuid() };
        if meta.uid() != current_uid {
            bail!(
                "refusing to read {} owned by uid {} (expected uid {})",
                path.display(),
                meta.uid(),
                current_uid
            );
        }
        if meta.mode() & 0o077 != 0 {
            bail!(
                "refusing to read {} with insecure permissions {:04o} (expected 0600)",
                path.display(),
                meta.mode() & 0o7777
            );
        }
    }

    let mut raw_bytes = Vec::new();
    file.read_to_end(&mut raw_bytes)?;
    Ok(Some(ZeroizingBytes(raw_bytes)))
}

fn read_credentials_map_locked(creds_path: &Path) -> Result<(ZeroizingCredentialMap, bool)> {
    let Some(raw_bytes) = read_private_file_bytes_locked(creds_path)? else {
        return Ok((ZeroizingCredentialMap(BTreeMap::new()), false));
    };
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

fn seal_vault_map(map: &BTreeMap<String, String>, key: &[u8]) -> Result<ZeroizingBytes> {
    if key.len() != VAULT_KEY_LEN {
        bail!("invalid credentials vault key length");
    }
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| anyhow::anyhow!("failed to initialize credentials vault cipher"))?;
    let aead_key = LessSafeKey::new(unbound);

    let mut nonce_bytes = [0u8; VAULT_NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate credentials vault nonce"))?;

    let serialized = ZeroizingBytes(
        serde_json::to_vec(map)
            .map_err(|_| anyhow::anyhow!("failed to serialize credentials vault"))?,
    );
    let body_start = VAULT_MAGIC.len() + VAULT_NONCE_LEN;
    let mut out = ZeroizingBytes(Vec::with_capacity(
        body_start + serialized.0.len() + AES_256_GCM.tag_len(),
    ));
    out.0.extend_from_slice(VAULT_MAGIC);
    out.0.extend_from_slice(&nonce_bytes);
    let mut ciphertext = ZeroizingBytes(serialized.0.clone());
    drop(serialized);

    aead_key
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(VAULT_AAD),
            &mut ciphertext.0,
        )
        .map_err(|_| anyhow::anyhow!("failed to encrypt credentials vault"))?;
    out.0.extend_from_slice(&ciphertext.0);
    Ok(out)
}

fn open_vault_map(vault_path: &Path, raw: &[u8], key: &[u8]) -> Result<ZeroizingCredentialMap> {
    if key.len() != VAULT_KEY_LEN {
        bail!("invalid credentials vault key length");
    }
    let min_len = VAULT_MAGIC.len() + VAULT_NONCE_LEN + AES_256_GCM.tag_len();
    if raw.len() < min_len || &raw[..VAULT_MAGIC.len()] != VAULT_MAGIC {
        bail!(
            "corrupt credentials vault {}: unrecognized format",
            vault_path.display()
        );
    }
    let mut nonce_bytes = [0u8; VAULT_NONCE_LEN];
    nonce_bytes.copy_from_slice(&raw[VAULT_MAGIC.len()..VAULT_MAGIC.len() + VAULT_NONCE_LEN]);
    let mut buf = ZeroizingBytes(raw[VAULT_MAGIC.len() + VAULT_NONCE_LEN..].to_vec());

    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| anyhow::anyhow!("failed to initialize credentials vault cipher"))?;
    let aead_key = LessSafeKey::new(unbound);
    let plaintext = aead_key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::from(VAULT_AAD),
            &mut buf.0,
        )
        .map_err(|_| {
            anyhow::anyhow!(
                "failed to decrypt credentials vault {}: authentication error or mismatched vault key",
                vault_path.display()
            )
        })?;

    let parsed: BTreeMap<String, String> = serde_json::from_slice(plaintext).map_err(|_| {
        anyhow::anyhow!(
            "corrupt credentials vault {}: invalid JSON payload",
            vault_path.display()
        )
    })?;
    Ok(ZeroizingCredentialMap(parsed))
}

fn ensure_existing_private_dir(state_dir: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(state_dir) {
        Ok(_) => {
            ensure_private_dir(state_dir)?;
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn require_vault_key(vault_path: &Path, store: &dyn VaultKeyStore) -> Result<ZeroizingBytes> {
    store.get_key()?.ok_or_else(|| {
        anyhow::anyhow!(
            "credentials vault {} exists but macOS Keychain master key \"{KEYCHAIN_VAULT_ACCOUNT}\" (service \"{KEYCHAIN_SERVICE}\") is missing",
            vault_path.display()
        )
    })
}

fn read_vault_map_locked(
    vault_path: &Path,
    store: &dyn VaultKeyStore,
) -> Result<Option<ZeroizingCredentialMap>> {
    let Some(raw_bytes) = read_private_file_bytes_locked(vault_path)? else {
        return Ok(None);
    };
    let key = require_vault_key(vault_path, store)?;
    open_vault_map(vault_path, &raw_bytes.0, &key.0).map(Some)
}

fn vault_set_with_store(
    state_dir: &Path,
    provider: CredentialProvider,
    secret: &SecretString,
    store: &dyn VaultKeyStore,
) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
    let vault_path = state_dir.join(VAULT_FILE_NAME);
    let (key, mut map) = match read_private_file_bytes_locked(&vault_path)? {
        Some(raw_bytes) => {
            let key = require_vault_key(&vault_path, store)?;
            let map = open_vault_map(&vault_path, &raw_bytes.0, &key.0)?;
            (key, map)
        }
        None => (
            store.get_or_create_key()?,
            ZeroizingCredentialMap(BTreeMap::new()),
        ),
    };
    map.0
        .insert(provider.as_str().to_string(), secret.as_str().to_string());
    let sealed = seal_vault_map(&map.0, &key.0)?;
    write_private_file_atomic(&vault_path, &sealed.0)?;
    Ok(())
}

fn vault_delete_with_store(
    state_dir: &Path,
    provider: CredentialProvider,
    store: &dyn VaultKeyStore,
) -> Result<bool> {
    if !ensure_existing_private_dir(state_dir)? {
        return Ok(false);
    }
    let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
    let vault_path = state_dir.join(VAULT_FILE_NAME);
    let Some(raw_bytes) = read_private_file_bytes_locked(&vault_path)? else {
        return Ok(false);
    };
    let key = require_vault_key(&vault_path, store)?;
    let mut map = open_vault_map(&vault_path, &raw_bytes.0, &key.0)?;
    let Some(removed_val) = map.0.remove(provider.as_str()) else {
        return Ok(false);
    };
    drop(SecretString::new(removed_val));
    if map.0.is_empty() {
        match std::fs::remove_file(&vault_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    } else {
        let sealed = seal_vault_map(&map.0, &key.0)?;
        write_private_file_atomic(&vault_path, &sealed.0)?;
    }
    Ok(true)
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
    if !ensure_existing_private_dir(state_dir)? {
        return Ok(false);
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

fn resolve_env_provider(provider: CredentialProvider) -> Option<Result<ResolvedCredential>> {
    let raw = std::env::var(provider.env_var()).ok()?;
    let raw = SecretString::new(raw);
    let is_blank = if provider == CredentialProvider::WebBotAuthJwk {
        raw.as_str()
            .trim_matches([' ', '\t', '\r', '\n'])
            .is_empty()
    } else {
        trim_secret_edges(raw.as_str()).is_empty()
    };
    if is_blank {
        return None;
    }
    Some(
        validate_credential(provider, raw.as_str()).map(|value| ResolvedCredential {
            provider,
            value,
            source: CredentialSource::Environment,
        }),
    )
}

pub type ProviderResolution = (CredentialProvider, Result<Option<ResolvedCredential>>);

fn populate_from_map(
    slots: &mut [ProviderResolution],
    unresolved_indices: &mut Vec<usize>,
    map: &mut BTreeMap<String, String>,
    source: CredentialSource,
) {
    let mut still_unresolved = Vec::with_capacity(unresolved_indices.len());
    for &idx in unresolved_indices.iter() {
        let provider = slots[idx].0;
        if let Some(raw) = map.remove(provider.as_str()) {
            let raw = SecretString::new(raw);
            let res = validate_credential(provider, raw.as_str()).map(|value| {
                Some(ResolvedCredential {
                    provider,
                    value,
                    source,
                })
            });
            slots[idx] = (provider, res);
        } else {
            still_unresolved.push(idx);
        }
    }
    *unresolved_indices = still_unresolved;
}

fn resolve_provider_results_with_store(
    state_dir: &Path,
    providers: &[CredentialProvider],
    vault_store: Option<&dyn VaultKeyStore>,
) -> Result<Vec<ProviderResolution>> {
    let mut slots: Vec<ProviderResolution> = Vec::with_capacity(providers.len());
    let mut unresolved_indices = Vec::new();
    let mut seen = HashSet::with_capacity(providers.len());

    for &provider in providers {
        if !seen.insert(provider) {
            continue;
        }
        let idx = slots.len();
        if let Some(env_res) = resolve_env_provider(provider) {
            slots.push((provider, env_res.map(Some)));
        } else {
            slots.push((provider, Ok(None)));
            unresolved_indices.push(idx);
        }
    }

    if !unresolved_indices.is_empty() && ensure_existing_private_dir(state_dir)? {
        let _lock = acquire_private_lock(&state_dir.join(CREDENTIALS_LOCK_NAME))?;
        if let Some(store) = vault_store {
            if let Some(mut vault_map) =
                read_vault_map_locked(&state_dir.join(VAULT_FILE_NAME), store)?
            {
                populate_from_map(
                    &mut slots,
                    &mut unresolved_indices,
                    &mut vault_map.0,
                    CredentialSource::Keychain,
                );
            }
        }
        if !unresolved_indices.is_empty() {
            let (mut file_map, existed) =
                read_credentials_map_locked(&state_dir.join(CREDENTIALS_FILE_NAME))?;
            if existed {
                populate_from_map(
                    &mut slots,
                    &mut unresolved_indices,
                    &mut file_map.0,
                    CredentialSource::File,
                );
            }
        }
    }

    Ok(slots)
}

fn resolve_with_system_store(
    state_dir: &Path,
    providers: &[CredentialProvider],
) -> Result<Vec<ProviderResolution>> {
    let sys_store = SystemKeychainStore;
    let vault_store: Option<&dyn VaultKeyStore> = keychain_enabled().then_some(&sys_store);
    resolve_provider_results_with_store(state_dir, providers, vault_store)
}

pub fn resolve_credential(
    state_dir: &Path,
    provider: CredentialProvider,
) -> Result<Option<ResolvedCredential>> {
    let mut results = resolve_with_system_store(state_dir, &[provider])?;
    match results.pop() {
        Some((_, res)) => res,
        None => Ok(None),
    }
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
            vault_set_with_store(state_dir, provider, &validated, &SystemKeychainStore)?;
            Ok(CredentialSource::Keychain)
        }
        CredentialStorage::File => {
            file_set(state_dir, provider, &validated)?;
            Ok(CredentialSource::File)
        }
        CredentialStorage::Auto => {
            if keychain_enabled() {
                vault_set_with_store(state_dir, provider, &validated, &SystemKeychainStore)?;
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
            if vault_delete_with_store(state_dir, provider, &SystemKeychainStore)? {
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
            if keychain_enabled()
                && vault_delete_with_store(state_dir, provider, &SystemKeychainStore)?
            {
                removed.push(CredentialSource::Keychain);
            }
            if file_delete(state_dir, provider)? {
                removed.push(CredentialSource::File);
            }
            Ok(removed)
        }
    }
}

pub fn resolve_all_provider_results(
    state_dir: &Path,
) -> Result<Vec<(CredentialProvider, Result<Option<ResolvedCredential>>)>> {
    resolve_with_system_store(state_dir, CredentialProvider::ALL)
}

pub fn resolve_credentials(
    state_dir: &Path,
    providers: &[CredentialProvider],
) -> Result<BTreeMap<CredentialProvider, ResolvedCredential>> {
    let entries = resolve_with_system_store(state_dir, providers)?;
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
    let mut map = resolve_credentials(state_dir, CredentialProvider::ALL)?;
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

pub fn agent_identity_status_from_results(
    results: &[(CredentialProvider, Result<Option<ResolvedCredential>>)],
    no_web_bot_auth: bool,
) -> AgentIdentityStatus {
    let mut status = AgentIdentityStatus {
        web_bot_auth_enabled: !no_web_bot_auth,
        ..Default::default()
    };
    let mut errors = Vec::new();

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
            (CredentialProvider::WebBotAuthJwk | CredentialProvider::WebBotAuthAgent, Ok(_)) => {}
            (CredentialProvider::WebBotAuthJwk | CredentialProvider::WebBotAuthAgent, Err(err)) => {
                errors.push(err.to_string())
            }
            _ => {}
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

pub fn resolve_agent_identity_status(
    state_dir: &Path,
    no_web_bot_auth: bool,
) -> AgentIdentityStatus {
    let results = match resolve_with_system_store(
        state_dir,
        &[
            CredentialProvider::WebBotAuthJwk,
            CredentialProvider::WebBotAuthAgent,
        ],
    ) {
        Ok(res) => res,
        Err(err) => {
            return AgentIdentityStatus {
                web_bot_auth_enabled: !no_web_bot_auth,
                status_error: Some(err.to_string()),
                ..Default::default()
            };
        }
    };
    agent_identity_status_from_results(&results, no_web_bot_auth)
}

pub fn cdp_access_status_from_results(
    results: &[(CredentialProvider, Result<Option<ResolvedCredential>>)],
) -> CdpAccessStatus {
    let mut status = CdpAccessStatus::default();
    let mut errors = Vec::new();

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
            (
                CredentialProvider::CdpAccessClientId
                | CredentialProvider::CdpAccessClientSecret
                | CredentialProvider::CdpAccessHost,
                Ok(_),
            ) => {}
            (
                CredentialProvider::CdpAccessClientId
                | CredentialProvider::CdpAccessClientSecret
                | CredentialProvider::CdpAccessHost,
                Err(err),
            ) => errors.push(err.to_string()),
            _ => {}
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

pub fn resolve_cdp_access_status(state_dir: &Path) -> CdpAccessStatus {
    let results = match resolve_with_system_store(
        state_dir,
        &[
            CredentialProvider::CdpAccessClientId,
            CredentialProvider::CdpAccessClientSecret,
            CredentialProvider::CdpAccessHost,
        ],
    ) {
        Ok(res) => res,
        Err(err) => {
            return CdpAccessStatus {
                status_error: Some(err.to_string()),
                ..Default::default()
            };
        }
    };
    cdp_access_status_from_results(&results)
}

#[cfg(test)]
mod vault_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, MutexGuard};

    static VAULT_ENV_LOCK: Mutex<()> = Mutex::new(());
    static VAULT_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    struct ClearEnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl ClearEnvGuard {
        fn clear_all_providers(overrides: &[(&'static str, &str)]) -> Self {
            let lock = VAULT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for provider in CredentialProvider::ALL {
                let key = provider.env_var();
                saved.push((key, std::env::var(key).ok()));
                std::env::remove_var(key);
            }
            for &(key, val) in overrides {
                saved.push((key, std::env::var(key).ok()));
                std::env::set_var(key, val);
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for ClearEnvGuard {
        fn drop(&mut self) {
            for (key, prev) in self.saved.iter().rev() {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    struct TempVaultDir(std::path::PathBuf);

    impl TempVaultDir {
        fn new() -> Self {
            let seq = VAULT_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::path::PathBuf::from(format!("/tmp/pleb-v-{}-{}", std::process::id(), seq));
            let _ = std::fs::remove_dir_all(&dir);
            ensure_private_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempVaultDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct MockVaultKeyStore {
        key: Mutex<Option<[u8; VAULT_KEY_LEN]>>,
        calls: AtomicUsize,
    }

    impl MockVaultKeyStore {
        fn new() -> Self {
            Self {
                key: Mutex::new(None),
                calls: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn reset_calls(&self) {
            self.calls.store(0, Ordering::SeqCst);
        }

        fn clear_key(&self) {
            *self.key.lock().unwrap() = None;
        }
    }

    impl VaultKeyStore for MockVaultKeyStore {
        fn get_key(&self) -> Result<Option<ZeroizingBytes>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .key
                .lock()
                .unwrap()
                .map(|bytes| ZeroizingBytes(bytes.to_vec())))
        }

        fn get_or_create_key(&self) -> Result<ZeroizingBytes> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut guard = self.key.lock().unwrap();
            if let Some(existing) = *guard {
                return Ok(ZeroizingBytes(existing.to_vec()));
            }
            let mut created = [0u8; VAULT_KEY_LEN];
            SystemRandom::new().fill(&mut created).unwrap();
            *guard = Some(created);
            Ok(ZeroizingBytes(created.to_vec()))
        }
    }

    #[test]
    fn missing_or_env_satisfied_vault_makes_zero_keychain_calls_and_multi_provider_makes_one() {
        let _env = ClearEnvGuard::clear_all_providers(&[]);
        let dir = TempVaultDir::new();
        let store = MockVaultKeyStore::new();

        // Missing credentials.enc: 0 Keychain lookups across all 10 providers.
        let initial =
            resolve_provider_results_with_store(&dir.0, CredentialProvider::ALL, Some(&store))
                .unwrap();
        assert_eq!(initial.len(), CredentialProvider::ALL.len());
        assert_eq!(store.call_count(), 0);
        assert!(!vault_delete_with_store(&dir.0, CredentialProvider::Brave, &store).unwrap());
        assert_eq!(store.call_count(), 0);

        // Non-existent state_dir also performs 0 Keychain lookups.
        let missing_dir = dir.0.join("nonexistent-state-dir");
        let missing_res = resolve_provider_results_with_store(
            &missing_dir,
            CredentialProvider::ALL,
            Some(&store),
        )
        .unwrap();
        assert_eq!(missing_res.len(), CredentialProvider::ALL.len());
        assert_eq!(store.call_count(), 0);

        // Store Brave and Tavily in the vault, verify ciphertext is non-plaintext & nonce-random.
        let brave_secret = SecretString::new("bsa-vault-secret-12345");
        let tavily_secret = SecretString::new("tvly-vault-secret-67890");
        vault_set_with_store(&dir.0, CredentialProvider::Brave, &brave_secret, &store).unwrap();
        let vault_path = dir.0.join(VAULT_FILE_NAME);
        let first_bytes = std::fs::read(&vault_path).unwrap();
        assert!(first_bytes.starts_with(VAULT_MAGIC));
        assert!(!first_bytes
            .windows(brave_secret.as_str().len())
            .any(|w| w == brave_secret.as_str().as_bytes()));

        vault_set_with_store(&dir.0, CredentialProvider::Tavily, &tavily_secret, &store).unwrap();
        let second_bytes = std::fs::read(&vault_path).unwrap();
        assert_ne!(
            &first_bytes[VAULT_MAGIC.len()..VAULT_MAGIC.len() + VAULT_NONCE_LEN],
            &second_bytes[VAULT_MAGIC.len()..VAULT_MAGIC.len() + VAULT_NONCE_LEN],
            "each vault write must allocate a fresh random nonce"
        );
        assert!(!second_bytes
            .windows(tavily_secret.as_str().len())
            .any(|w| w == tavily_secret.as_str().as_bytes()));

        // Resolving all 10 providers performs at most ONE vault key lookup.
        store.reset_calls();
        let all_res =
            resolve_provider_results_with_store(&dir.0, CredentialProvider::ALL, Some(&store))
                .unwrap();
        assert_eq!(store.call_count(), 1);
        let mut by_provider: BTreeMap<CredentialProvider, (CredentialSource, String)> =
            BTreeMap::new();
        for (provider, res) in all_res {
            if let Some(cred) = res.unwrap() {
                by_provider.insert(provider, (cred.source, cred.value.as_str().to_string()));
            }
        }
        assert_eq!(
            by_provider.get(&CredentialProvider::Brave),
            Some(&(CredentialSource::Keychain, "bsa-vault-secret-12345".into()))
        );
        assert_eq!(
            by_provider.get(&CredentialProvider::Tavily),
            Some(&(CredentialSource::Keychain, "tvly-vault-secret-67890".into()))
        );

        // When requested providers are satisfied by env, credentials.enc is untouched (0 Keychain calls).
        drop(_env);
        let _env_override = ClearEnvGuard::clear_all_providers(&[
            (
                "PLEB_WEB_BOT_AUTH_JWK",
                r#"{"crv":"Ed25519","d":"nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A","kty":"OKP","x":"11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"}"#,
            ),
            ("PLEB_WEB_BOT_AUTH_AGENT", "https://agent.example.com"),
        ]);
        store.reset_calls();
        let env_only = resolve_provider_results_with_store(
            &dir.0,
            &[
                CredentialProvider::WebBotAuthJwk,
                CredentialProvider::WebBotAuthAgent,
            ],
            Some(&store),
        )
        .unwrap();
        assert_eq!(store.call_count(), 0);
        assert!(env_only
            .iter()
            .all(|(_, r)| r.as_ref().unwrap().as_ref().unwrap().source
                == CredentialSource::Environment));
    }

    #[test]
    fn vault_rejects_tamper_missing_key_bad_perms_symlink_and_prunes_empty_vault() {
        let _env = ClearEnvGuard::clear_all_providers(&[]);
        let dir = TempVaultDir::new();
        let store = MockVaultKeyStore::new();
        let vault_path = dir.0.join(VAULT_FILE_NAME);

        // Vault takes precedence over file storage when both exist.
        file_set(
            &dir.0,
            CredentialProvider::Brave,
            &SecretString::new("file-secret"),
        )
        .unwrap();
        vault_set_with_store(
            &dir.0,
            CredentialProvider::Brave,
            &SecretString::new("vault-secret"),
            &store,
        )
        .unwrap();

        let res =
            resolve_provider_results_with_store(&dir.0, &[CredentialProvider::Brave], Some(&store))
                .unwrap();
        let cred = res[0].1.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(cred.source, CredentialSource::Keychain);
        assert_eq!(cred.value.as_str(), "vault-secret");

        // Concurrent vault writes preserve all providers cleanly.
        std::thread::scope(|scope| {
            for (provider, val) in [
                (CredentialProvider::Tavily, "tavily-conc"),
                (CredentialProvider::Serper, "serper-conc"),
                (CredentialProvider::Exa, "exa-conc"),
            ] {
                let d = &dir.0;
                let s = &store;
                scope.spawn(move || {
                    vault_set_with_store(d, provider, &SecretString::new(val), s).unwrap();
                });
            }
        });
        for (provider, expected) in [
            (CredentialProvider::Brave, "vault-secret"),
            (CredentialProvider::Tavily, "tavily-conc"),
            (CredentialProvider::Serper, "serper-conc"),
            (CredentialProvider::Exa, "exa-conc"),
        ] {
            let single =
                resolve_provider_results_with_store(&dir.0, &[provider], Some(&store)).unwrap();
            assert_eq!(
                single[0]
                    .1
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .value
                    .as_str(),
                expected
            );
        }

        // Tamper detection without overwriting.
        let valid_bytes = std::fs::read(&vault_path).unwrap();
        let mut tampered = valid_bytes.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        std::fs::write(&vault_path, &tampered).unwrap();
        assert!(resolve_provider_results_with_store(
            &dir.0,
            &[CredentialProvider::Brave],
            Some(&store)
        )
        .is_err());
        assert!(vault_set_with_store(
            &dir.0,
            CredentialProvider::Brave,
            &SecretString::new("new"),
            &store
        )
        .is_err());
        assert_eq!(std::fs::read(&vault_path).unwrap(), tampered);

        // Bad magic rejected.
        let mut bad_magic = valid_bytes.clone();
        bad_magic[0] = b'X';
        std::fs::write(&vault_path, &bad_magic).unwrap();
        assert!(resolve_provider_results_with_store(
            &dir.0,
            &[CredentialProvider::Brave],
            Some(&store)
        )
        .is_err());

        // Restore valid ciphertext and remove master key -> loud failure without overwriting.
        std::fs::write(&vault_path, &valid_bytes).unwrap();
        let saved_key = *store.key.lock().unwrap();
        store.clear_key();
        let err =
            resolve_provider_results_with_store(&dir.0, &[CredentialProvider::Brave], Some(&store))
                .unwrap_err();
        assert!(err.to_string().contains("vault-key"), "err={err}");
        assert!(vault_set_with_store(
            &dir.0,
            CredentialProvider::Brave,
            &SecretString::new("overwrite-attempt"),
            &store
        )
        .is_err());
        assert_eq!(std::fs::read(&vault_path).unwrap(), valid_bytes);
        *store.key.lock().unwrap() = saved_key;

        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o644)).unwrap();
            let err = resolve_provider_results_with_store(
                &dir.0,
                &[CredentialProvider::Brave],
                Some(&store),
            )
            .unwrap_err();
            assert!(err.to_string().contains("insecure permissions"));

            let target = dir.0.join("real-vault.enc");
            std::fs::write(&target, &valid_bytes).unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::remove_file(&vault_path).unwrap();
            symlink(&target, &vault_path).unwrap();
            assert!(resolve_provider_results_with_store(
                &dir.0,
                &[CredentialProvider::Brave],
                Some(&store)
            )
            .is_err());
            std::fs::remove_file(&vault_path).unwrap();
            std::fs::write(&vault_path, &valid_bytes).unwrap();
            std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Removing all stored vault providers removes credentials.enc -> subsequent resolves make 0 Keychain calls.
        for p in [
            CredentialProvider::Brave,
            CredentialProvider::Tavily,
            CredentialProvider::Serper,
            CredentialProvider::Exa,
        ] {
            assert!(vault_delete_with_store(&dir.0, p, &store).unwrap());
        }
        assert!(!vault_path.exists());
        store.reset_calls();
        let after_prune =
            resolve_provider_results_with_store(&dir.0, CredentialProvider::ALL, Some(&store))
                .unwrap();
        assert_eq!(store.call_count(), 0);
        // Brave still resolves cleanly from the remaining File tier.
        let brave_file = after_prune[0].1.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(brave_file.source, CredentialSource::File);
        assert_eq!(brave_file.value.as_str(), "file-secret");

        // Vault key hex encoding roundtrip & validation.
        let hex_buf = encode_lowercase_hex(&[0xab; VAULT_KEY_LEN]);
        let hex_good = std::str::from_utf8(&hex_buf.0).unwrap();
        assert_eq!(
            decode_hex_vault_key(&format!("{hex_good}\n")).unwrap().0,
            vec![0xab; VAULT_KEY_LEN]
        );
        assert!(decode_hex_vault_key("not-hex").is_err());

        for truthy in ["1", "true", "TRUE", " True "] {
            assert!(is_truthy_env_flag(truthy), "expected truthy for {truthy:?}");
        }
        for falsy in ["", "0", "false", "FALSE", "yes", "on"] {
            assert!(!is_truthy_env_flag(falsy), "expected falsy for {falsy:?}");
        }
    }
}
