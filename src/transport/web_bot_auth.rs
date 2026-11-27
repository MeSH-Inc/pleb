use anyhow::Result;
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::Ed25519KeyPair;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

use super::{
    HttpResponse, RequestHeaderOptions, TransportClient, TransportError, TransportErrorKind,
    WebBotAuthHeaders,
};
use crate::auth::{CredentialProvider, SecretString};

pub const SIGNATURE_TTL_SECS: u64 = 300;
pub const REFRESH_SKEW_SECS: u64 = 30;
pub const MAX_CACHED_AUTHORITIES: usize = 256;
pub const MAX_SIGNED_REDIRECT_HOPS: usize = 10;

pub fn ed25519_jwk_thumbprint(x_bytes: &[u8; 32]) -> String {
    let x_b64 = URL_SAFE_NO_PAD.encode(x_bytes);
    let canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x_b64}"}}"#);
    let digest = ring::digest::digest(&ring::digest::SHA256, canonical.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

#[derive(Default)]
struct SignatureCache {
    entries: HashMap<String, CachedSignature>,
    next_seq: u64,
}

pub struct WebBotAuthSigner {
    key_id: String,
    signature_agent: String,
    keypair: Arc<Ed25519KeyPair>,
    rng: SystemRandom,
    cache: Mutex<SignatureCache>,
    allow_loopback_for_test: bool,
}

struct CachedSignature {
    created: u64,
    expires: u64,
    seq: u64,
    headers: WebBotAuthHeaders,
}

impl fmt::Debug for WebBotAuthSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebBotAuthSigner")
            .field("key_id", &self.key_id)
            .field("signature_agent", &self.signature_agent)
            .field("allow_loopback_for_test", &self.allow_loopback_for_test)
            .finish_non_exhaustive()
    }
}

impl WebBotAuthSigner {
    pub fn from_state_dir(state_dir: &Path) -> Result<Option<Self>> {
        let mut resolved = crate::auth::resolve_credentials(
            state_dir,
            &[
                CredentialProvider::WebBotAuthJwk,
                CredentialProvider::WebBotAuthAgent,
            ],
        )?;
        let jwk = resolved.remove(&CredentialProvider::WebBotAuthJwk);
        let agent = resolved.remove(&CredentialProvider::WebBotAuthAgent);
        match (jwk, agent) {
            (None, None) => Ok(None),
            (Some(j), Some(a)) => Self::from_credentials(&j.value, &a.value).map(Some),
            (Some(_), None) => anyhow::bail!(
                "incomplete Web Bot Auth configuration: run `pleb auth set web-bot-auth-agent`"
            ),
            (None, Some(_)) => anyhow::bail!(
                "incomplete Web Bot Auth configuration: run `pleb auth set web-bot-auth-jwk`"
            ),
        }
    }

    pub fn from_credentials(jwk: &SecretString, agent: &SecretString) -> Result<Self> {
        let parsed_jwk = crate::auth::parse_ed25519_jwk(jwk.as_str())?;
        let validated_agent =
            crate::auth::validate_credential(CredentialProvider::WebBotAuthAgent, agent.as_str())?;
        Ok(Self {
            key_id: parsed_jwk.key_id,
            signature_agent: validated_agent.as_str().to_string(),
            keypair: Arc::new(parsed_jwk.keypair),
            rng: SystemRandom::new(),
            cache: Mutex::new(SignatureCache::default()),
            allow_loopback_for_test: false,
        })
    }

    pub fn with_allow_loopback_for_test(mut self, allow: bool) -> Self {
        self.allow_loopback_for_test = allow;
        self
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn signature_agent(&self) -> &str {
        &self.signature_agent
    }

    pub fn is_url_eligible(&self, url: &Url) -> bool {
        if url.scheme() != "https" {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        if self.allow_loopback_for_test && crate::policy::is_loopback_host(host) {
            return true;
        }
        !crate::policy::is_local_or_private_host(host)
    }

    pub fn sign_url(&self, url: &Url) -> Result<Option<WebBotAuthHeaders>> {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("system clock is before UNIX epoch"))?
            .as_secs();
        self.sign_url_at(url, now_secs)
    }

    pub fn sign_url_at(&self, url: &Url, now_secs: u64) -> Result<Option<WebBotAuthHeaders>> {
        if !self.is_url_eligible(url) {
            return Ok(None);
        }
        let Some(authority) = format_authority(url) else {
            return Ok(None);
        };

        let mut guard = self
            .cache
            .lock()
            .map_err(|_| anyhow::anyhow!("Web Bot Auth signature cache lock poisoned"))?;

        if let Some(cached) = guard.entries.get(&authority) {
            if now_secs >= cached.created
                && now_secs < cached.expires.saturating_sub(REFRESH_SKEW_SECS)
            {
                return Ok(Some(cached.headers.clone()));
            }
        }

        let mut nonce_bytes = [0u8; 32];
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("failed to generate Web Bot Auth nonce"))?;
        let nonce = URL_SAFE_NO_PAD.encode(nonce_bytes);

        let created = now_secs;
        let expires = now_secs.saturating_add(SIGNATURE_TTL_SECS);
        let signature_agent_sf = format!("\"{}\"", self.signature_agent);
        let sig_params = format!(
            "(\"@authority\" \"signature-agent\");created={created};expires={expires};keyid=\"{}\";nonce=\"{nonce}\";alg=\"ed25519\";tag=\"web-bot-auth\"",
            self.key_id
        );
        let signature_base = format!(
            "\"@authority\": {authority}\n\"signature-agent\": {signature_agent_sf}\n\"@signature-params\": {sig_params}"
        );
        let sig = self.keypair.sign(signature_base.as_bytes());
        let headers = WebBotAuthHeaders {
            signature: format!("sig1=:{}:", STANDARD.encode(sig.as_ref())),
            signature_input: format!("sig1={sig_params}"),
            signature_agent: signature_agent_sf,
        };

        if !guard.entries.contains_key(&authority) && guard.entries.len() >= MAX_CACHED_AUTHORITIES
        {
            if let Some(evict_key) = guard
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(k, _)| k.clone())
            {
                guard.entries.remove(&evict_key);
            }
        }

        let seq = guard.next_seq;
        guard.next_seq = guard.next_seq.wrapping_add(1);
        guard.entries.insert(
            authority,
            CachedSignature {
                created,
                expires,
                seq,
                headers: headers.clone(),
            },
        );

        Ok(Some(headers))
    }
}

fn format_authority(url: &Url) -> Option<String> {
    let host = crate::policy::source::canonical_url_host(url).filter(|h| !h.is_empty())?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn safe_hop_referer(referer: Option<&str>, next_url: &Url) -> Option<String> {
    let raw = referer?;
    if let Ok(parsed) = Url::parse(raw) {
        if parsed.scheme() == "https" && next_url.scheme() != "https" {
            return None;
        }
    }
    Some(raw.to_string())
}

pub async fn execute_signed_get(
    client: &TransportClient,
    url: &str,
    timeout: std::time::Duration,
    max_body_bytes: usize,
    opts: &RequestHeaderOptions,
    signer: Option<&WebBotAuthSigner>,
) -> Result<HttpResponse> {
    let Some(signer) = signer else {
        return client
            .get_with_request_options(url, timeout, max_body_bytes, opts)
            .await;
    };

    let origin_url = Url::parse(url).map_err(|_| {
        anyhow::Error::new(TransportError::new(
            TransportErrorKind::LoadError,
            "invalid request URL",
        ))
    })?;
    let mut current_url = origin_url.clone();
    let deadline = tokio::time::Instant::now() + timeout;

    for hop in 0..=MAX_SIGNED_REDIRECT_HOPS {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::Error::new(TransportError::new(
                TransportErrorKind::Timeout,
                format!("request timed out after {:?}", timeout),
            )));
        }

        let hop_opts = RequestHeaderOptions {
            accept_markdown: opts.accept_markdown,
            referer: safe_hop_referer(opts.referer.as_deref(), &current_url),
            subresource_dest: opts.subresource_dest,
            web_bot_auth: signer.sign_url(&current_url)?,
            disable_redirects: true,
        };

        let resp = client
            .get_with_request_options(current_url.as_str(), remaining, max_body_bytes, &hop_opts)
            .await?;

        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
            if let Some(raw_loc) = resp
                .location_header
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let next_url = current_url.join(raw_loc).map_err(|_| {
                    anyhow::Error::new(TransportError::new(
                        TransportErrorKind::RedirectBlocked,
                        "blocked malformed redirect location",
                    ))
                })?;
                if !crate::policy::is_safe_redirect_url(&origin_url, &next_url)
                    || !crate::policy::is_safe_redirect_url(&current_url, &next_url)
                {
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::RedirectBlocked,
                        format!("blocked unsafe redirect to {next_url}"),
                    )));
                }
                if hop == MAX_SIGNED_REDIRECT_HOPS {
                    return Err(anyhow::Error::new(TransportError::new(
                        TransportErrorKind::LoadError,
                        format!("exceeded maximum redirect hops ({MAX_SIGNED_REDIRECT_HOPS})"),
                    )));
                }
                current_url = next_url;
                continue;
            }
        }

        return Ok(resp);
    }

    Err(anyhow::Error::new(TransportError::new(
        TransportErrorKind::LoadError,
        format!("exceeded maximum redirect hops ({MAX_SIGNED_REDIRECT_HOPS})"),
    )))
}
