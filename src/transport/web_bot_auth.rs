use anyhow::Result;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use url::Url;

use super::{HttpResponse, RequestHeaderOptions, TransportClient, WebBotAuthHeaders};
use crate::auth::{CredentialProvider, SecretString};

pub const SIGNATURE_TTL_SECS: u64 = 300;
pub const REFRESH_SKEW_SECS: u64 = 30;
pub const MAX_CACHED_AUTHORITIES: usize = 256;
pub const MAX_SIGNED_REDIRECT_HOPS: usize = 10;

pub struct WebBotAuthSigner {
    key_id: String,
    signature_agent: String,
    _keypair: Arc<ring::signature::Ed25519KeyPair>,
    _cache: Mutex<HashMap<String, CachedSignature>>,
    allow_loopback_for_test: bool,
}

#[allow(dead_code)]
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
        let jwk = crate::auth::resolve_credential(state_dir, CredentialProvider::WebBotAuthJwk)?;
        let agent =
            crate::auth::resolve_credential(state_dir, CredentialProvider::WebBotAuthAgent)?;
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

    pub fn from_credentials(_jwk: &SecretString, _agent: &SecretString) -> Result<Self> {
        anyhow::bail!("Web Bot Auth signing is not yet implemented")
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

    pub fn sign_url(&self, _url: &Url) -> Result<Option<WebBotAuthHeaders>> {
        anyhow::bail!("Web Bot Auth signing is not yet implemented")
    }

    pub fn sign_url_at(&self, _url: &Url, _now_secs: u64) -> Result<Option<WebBotAuthHeaders>> {
        anyhow::bail!("Web Bot Auth signing is not yet implemented")
    }
}

pub async fn execute_signed_get(
    client: &TransportClient,
    url: &str,
    timeout: std::time::Duration,
    max_body_bytes: usize,
    opts: &RequestHeaderOptions,
    _signer: Option<&WebBotAuthSigner>,
) -> Result<HttpResponse> {
    client
        .get_with_request_options(url, timeout, max_body_bytes, opts)
        .await
}
