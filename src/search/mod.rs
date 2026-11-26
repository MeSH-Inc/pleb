pub mod providers;
pub mod state;

use anyhow::{bail, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinSet;

use crate::auth::{self, CredentialProvider, CredentialSource, ResolvedCredential};
use crate::context::RuntimeContext;
use crate::extract::url::normalize_crawl_url;

pub use providers::{clean_ddg_url, parse_brave_json, parse_ddg_html};
pub use state::{
    civil_from_days, clear_provider_cooldown, clear_search_state, days_from_civil,
    parse_retry_after, provider_health_summary, ProviderHealthEntry,
};

use providers::{execute_json_provider, search_ddg, ProviderFailure};
use state::{
    is_cooldown_active, key_fingerprint, record_outcome, reserve_dispatch, reserve_explicit,
    reserve_federated, DispatchDecision, DispatchOutcome, SkippedProvider,
};

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_SEARCH_ENDPOINT: &str = "https://api.tavily.com/search";
const SERPER_SEARCH_ENDPOINT: &str = "https://google.serper.dev/search";
const EXA_SEARCH_ENDPOINT: &str = "https://api.exa.ai/search";
const DDG_SEARCH_ENDPOINT: &str = "https://html.duckduckgo.com/html/";
const MAX_EXTRA_SNIPPETS: usize = 5;
const RRF_K: f64 = 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SearchProvider {
    #[default]
    Auto,
    Brave,
    Tavily,
    Serper,
    Exa,
    Searxng,
    #[value(name = "duckduckgo", alias = "ddg")]
    DuckDuckGo,
    Federated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ConcreteProvider {
    Brave,
    Tavily,
    Serper,
    Exa,
    Searxng,
    DuckDuckGo,
}

impl ConcreteProvider {
    pub(crate) const ALL: &'static [Self] = &[
        Self::Brave,
        Self::Tavily,
        Self::Serper,
        Self::Exa,
        Self::Searxng,
        Self::DuckDuckGo,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Tavily => "tavily",
            Self::Serper => "serper",
            Self::Exa => "exa",
            Self::Searxng => "searxng",
            Self::DuckDuckGo => "duckduckgo",
        }
    }

    pub(crate) const fn credential_provider(self) -> Option<CredentialProvider> {
        match self {
            Self::Brave => Some(CredentialProvider::Brave),
            Self::Tavily => Some(CredentialProvider::Tavily),
            Self::Serper => Some(CredentialProvider::Serper),
            Self::Exa => Some(CredentialProvider::Exa),
            Self::Searxng => Some(CredentialProvider::Searxng),
            Self::DuckDuckGo => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum SearchMode {
    #[default]
    Auto,
    Exact,
    Semantic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Freshness {
    Day,
    Week,
    Month,
    Year,
}

impl Freshness {
    pub fn brave_param(self) -> &'static str {
        match self {
            Self::Day => "pd",
            Self::Week => "pw",
            Self::Month => "pm",
            Self::Year => "py",
        }
    }

    pub fn ddg_param(self) -> &'static str {
        match self {
            Self::Day => "d",
            Self::Week => "w",
            Self::Month => "m",
            Self::Year => "y",
        }
    }

    pub(crate) fn time_range_param(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    pub(crate) fn serper_tbs(self) -> &'static str {
        match self {
            Self::Day => "qdr:d",
            Self::Week => "qdr:w",
            Self::Month => "qdr:m",
            Self::Year => "qdr:y",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub limit: usize,
    pub provider: SearchProvider,
    pub mode: SearchMode,
    pub freshness: Option<Freshness>,
    pub site: Option<String>,
    pub extra_snippets: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_snippets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResponse {
    pub provider: &'static str,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers_used: Vec<&'static str>,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchProviderStatus {
    pub configured: bool,
    pub source: Option<CredentialSource>,
    pub cooldown_remaining_secs: u64,
    pub month_requests: u32,
    pub last_error_status: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct ProviderEndpoints {
    pub brave_url: String,
    pub tavily_url: String,
    pub serper_url: String,
    pub exa_url: String,
    pub ddg_url: String,
}

impl Default for ProviderEndpoints {
    fn default() -> Self {
        Self {
            brave_url: BRAVE_SEARCH_ENDPOINT.to_string(),
            tavily_url: TAVILY_SEARCH_ENDPOINT.to_string(),
            serper_url: SERPER_SEARCH_ENDPOINT.to_string(),
            exa_url: EXA_SEARCH_ENDPOINT.to_string(),
            ddg_url: DDG_SEARCH_ENDPOINT.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
enum ProviderAttempt {
    Skipped {
        provider: ConcreteProvider,
        cooldown_remaining_secs: u64,
        last_error_status: Option<u16>,
    },
    Failed {
        provider: ConcreteProvider,
        status: Option<u16>,
        message: String,
    },
}

impl ProviderAttempt {
    fn format_summary(&self) -> String {
        match self {
            Self::Skipped {
                provider,
                cooldown_remaining_secs,
                last_error_status,
            } => match last_error_status {
                Some(code) => format!(
                    "{}: skipped (cooldown {}s remaining, last HTTP {code})",
                    provider.as_str(),
                    cooldown_remaining_secs
                ),
                None => format!(
                    "{}: skipped (cooldown {}s remaining)",
                    provider.as_str(),
                    cooldown_remaining_secs
                ),
            },
            Self::Failed {
                provider,
                status,
                message,
            } => match status {
                Some(code) => format!("{}: failed (HTTP {code}: {message})", provider.as_str()),
                None => format!("{}: failed ({message})", provider.as_str()),
            },
        }
    }
}

pub fn search_provider_statuses(
    state_dir: &Path,
) -> Result<BTreeMap<String, SearchProviderStatus>> {
    let resolved = auth::resolve_all_credentials(state_dir)?;
    let health = provider_health_summary(state_dir)?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut statuses = BTreeMap::new();
    for (provider, cred) in resolved {
        let name = provider.as_str();
        let entry = health.get(name);
        let cooldown_remaining_secs = match entry {
            Some(e) if e.cooldown_until_epoch_secs > now_secs => {
                let current_fp = cred.as_ref().map(|r| key_fingerprint(r.value.as_str()));
                if is_cooldown_active(e, current_fp.as_deref(), now_secs) {
                    e.cooldown_until_epoch_secs.saturating_sub(now_secs)
                } else {
                    0
                }
            }
            _ => 0,
        };
        let (configured, source) = match cred {
            Some(r) => (true, Some(r.source)),
            None => (false, None),
        };
        statuses.insert(
            name.to_string(),
            SearchProviderStatus {
                configured,
                source,
                cooldown_remaining_secs,
                month_requests: entry.map(|e| e.month_requests).unwrap_or(0),
                last_error_status: entry.and_then(|e| e.last_error_status),
            },
        );
    }
    Ok(statuses)
}

pub fn normalize_site(raw: &str) -> Option<String> {
    let mut s = raw.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("https://") {
        s = &s[8..];
    } else if lower.starts_with("http://") {
        s = &s[7..];
    }
    let cleaned = s.trim_matches('/').trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

pub fn build_effective_query(query: &str, site: Option<&str>) -> String {
    match site.and_then(normalize_site) {
        Some(domain) => format!("site:{domain} {}", query.trim()),
        None => query.to_string(),
    }
}

pub fn build_brave_search_url(
    query: &str,
    num_results: usize,
    freshness: Option<Freshness>,
    extra_snippets: bool,
) -> String {
    let encoded = urlencoding::encode(query);
    let mut url = format!("{BRAVE_SEARCH_ENDPOINT}?count={num_results}&q={encoded}");
    if let Some(f) = freshness {
        url.push_str("&freshness=");
        url.push_str(f.brave_param());
    }
    if extra_snippets {
        url.push_str("&extra_snippets=true");
    }
    url
}

pub fn build_ddg_search_url(query: &str, freshness: Option<Freshness>) -> String {
    let encoded = urlencoding::encode(query);
    let mut url = format!("{DDG_SEARCH_ENDPOINT}?q={encoded}");
    if let Some(f) = freshness {
        url.push_str("&df=");
        url.push_str(f.ddg_param());
    }
    url
}

/// Strict lexical needle detector on raw `query` (never `effective_query` with `site:`).
pub fn is_lexical_needle(raw_query: &str) -> bool {
    has_non_empty_quoted_phrase(raw_query)
        || has_scope_operator(raw_query)
        || has_error_or_hex_token(raw_query)
        || has_path_or_namespace_token(raw_query)
}

fn has_non_empty_quoted_phrase(query: &str) -> bool {
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            if let Some(rel_end) = bytes[i + 1..].iter().position(|&b| b == b'"') {
                let inner = &query[i + 1..i + 1 + rel_end];
                if let Some(first) = inner.chars().next() {
                    if !first.is_whitespace() {
                        return true;
                    }
                }
                i = i + 1 + rel_end + 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    false
}

fn is_ascii_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn has_scope_operator(query: &str) -> bool {
    let bytes = query.as_bytes();
    for i in 1..bytes.len().saturating_sub(2) {
        if bytes[i] == b':'
            && bytes[i + 1] == b':'
            && is_ascii_word(bytes[i - 1])
            && is_ascii_word(bytes[i + 2])
        {
            return true;
        }
    }
    false
}

fn has_error_or_hex_token(query: &str) -> bool {
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_ascii_word(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_ascii_word(bytes[i]) {
            i += 1;
        }
        let token = &bytes[start..i];

        // \bE[0-9]{4}\b
        if token.len() == 5 && token[0] == b'E' && token[1..].iter().all(u8::is_ascii_digit) {
            return true;
        }
        // \bERR_[A-Z0-9_]{2,}\b
        if token.len() >= 6
            && token.starts_with(b"ERR_")
            && token[4..]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
        {
            return true;
        }
        // \b0x[0-9a-fA-F]{2,}\b (rejects 20x20 because whole word is "20x20")
        if token.len() >= 4
            && token[0] == b'0'
            && token[1] == b'x'
            && token[2..].iter().all(u8::is_ascii_hexdigit)
        {
            return true;
        }
    }
    false
}

const PROSE_SLASH_PAIRS: &[&str] = &[
    "pros/cons",
    "and/or",
    "ci/cd",
    "true/false",
    "yes/no",
    "read/write",
    "input/output",
    "either/or",
    "his/her",
    "he/she",
    "w/o",
    "n/a",
    "a/b",
    "on/off",
    "up/down",
    "in/out",
    "client/server",
    "front/back",
    "before/after",
    "pass/fail",
    "win/lose",
];

fn has_path_or_namespace_token(query: &str) -> bool {
    for raw_word in query.split_whitespace() {
        let token = raw_word.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')' | ',' | ';' | '\'' | '"' | '[' | ']' | '?' | '!' | ':'
            )
        });
        if !token.contains('/') {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if PROSE_SLASH_PAIRS.contains(&lower.as_str()) {
            continue;
        }
        let segments: Vec<&str> = token.split('/').filter(|s| !s.is_empty()).collect();
        if segments.len() < 2 {
            continue;
        }
        // Exclude pure numeric dates (2026/11, 2026/11/26) and ratios (50/50, 24/7).
        if segments
            .iter()
            .all(|seg| seg.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        return true;
    }
    false
}

fn auto_candidate_order(raw_query: &str, options: &SearchOptions) -> [ConcreteProvider; 6] {
    match (
        options.mode,
        is_lexical_needle(raw_query),
        options.extra_snippets,
    ) {
        (SearchMode::Semantic, _, _) => [
            ConcreteProvider::Exa,
            ConcreteProvider::Tavily,
            ConcreteProvider::Brave,
            ConcreteProvider::Serper,
            ConcreteProvider::Searxng,
            ConcreteProvider::DuckDuckGo,
        ],
        (SearchMode::Exact, _, _) | (SearchMode::Auto, true, _) => [
            ConcreteProvider::Searxng,
            ConcreteProvider::Serper,
            ConcreteProvider::Brave,
            ConcreteProvider::Tavily,
            ConcreteProvider::Exa,
            ConcreteProvider::DuckDuckGo,
        ],
        (SearchMode::Auto, false, true) => [
            ConcreteProvider::Brave,
            ConcreteProvider::Tavily,
            ConcreteProvider::Exa,
            ConcreteProvider::Serper,
            ConcreteProvider::Searxng,
            ConcreteProvider::DuckDuckGo,
        ],
        (SearchMode::Auto, false, false) => [
            ConcreteProvider::Searxng,
            ConcreteProvider::Brave,
            ConcreteProvider::Tavily,
            ConcreteProvider::Serper,
            ConcreteProvider::Exa,
            ConcreteProvider::DuckDuckGo,
        ],
    }
}

fn resolve_all_credentials(
    state_dir: &Path,
) -> Result<(
    BTreeMap<ConcreteProvider, ResolvedCredential>,
    BTreeMap<ConcreteProvider, String>,
)> {
    let mut creds = BTreeMap::new();
    let mut fingerprints = BTreeMap::new();
    for (cp, resolved_opt) in auth::resolve_all_credentials(state_dir)? {
        if let Some(resolved) = resolved_opt {
            let provider = match cp {
                CredentialProvider::Brave => ConcreteProvider::Brave,
                CredentialProvider::Tavily => ConcreteProvider::Tavily,
                CredentialProvider::Serper => ConcreteProvider::Serper,
                CredentialProvider::Exa => ConcreteProvider::Exa,
                CredentialProvider::Searxng => ConcreteProvider::Searxng,
            };
            fingerprints.insert(provider, key_fingerprint(resolved.value.as_str()));
            creds.insert(provider, resolved);
        }
    }
    Ok((creds, fingerprints))
}

pub async fn search(
    ctx: &RuntimeContext,
    query: &str,
    options: &SearchOptions,
) -> Result<SearchResponse> {
    search_with_endpoints(ctx, query, options, &ProviderEndpoints::default()).await
}

pub async fn search_with_endpoints(
    ctx: &RuntimeContext,
    query: &str,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<SearchResponse> {
    let count = options.limit.clamp(1, 20);

    match options.provider {
        SearchProvider::Auto => search_auto(ctx, query, count, options, endpoints).await,
        SearchProvider::Federated => search_federated(ctx, query, count, options, endpoints).await,
        explicit => {
            let provider = match explicit {
                SearchProvider::Brave => ConcreteProvider::Brave,
                SearchProvider::Tavily => ConcreteProvider::Tavily,
                SearchProvider::Serper => ConcreteProvider::Serper,
                SearchProvider::Exa => ConcreteProvider::Exa,
                SearchProvider::Searxng => ConcreteProvider::Searxng,
                SearchProvider::DuckDuckGo => ConcreteProvider::DuckDuckGo,
                SearchProvider::Auto | SearchProvider::Federated => unreachable!(),
            };
            search_explicit(ctx, provider, query, count, options, endpoints).await
        }
    }
}

async fn search_explicit(
    ctx: &RuntimeContext,
    provider: ConcreteProvider,
    raw_query: &str,
    count: usize,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<SearchResponse> {
    let (cred_opt, fp_opt) = match provider.credential_provider() {
        Some(cp) => {
            let resolved =
                auth::resolve_credential(&ctx.config.state_dir, cp)?.ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} is not configured (set {} or run `wick auth set {}`)",
                        provider.as_str(),
                        cp.env_var(),
                        cp.as_str()
                    )
                })?;
            let fp = key_fingerprint(resolved.value.as_str());
            (Some(resolved), Some(fp))
        }
        None => (None, None),
    };

    reserve_explicit(&ctx.config.state_dir, provider, fp_opt.clone()).await?;

    let outcome = execute_concrete_provider(
        ctx,
        provider,
        cred_opt.as_ref().map(|c| c.value.as_str()),
        raw_query,
        count,
        options,
        endpoints,
    )
    .await;

    match outcome {
        Ok(results) => {
            record_outcome(
                &ctx.config.state_dir,
                provider,
                fp_opt,
                DispatchOutcome::Success,
            )
            .await?;
            Ok(SearchResponse {
                provider: provider.as_str(),
                providers_used: vec![],
                results,
            })
        }
        Err(failure) => {
            let _ = record_outcome(
                &ctx.config.state_dir,
                provider,
                fp_opt,
                DispatchOutcome::Failure {
                    status: failure.status,
                    cooldown_secs: failure.cooldown_secs,
                },
            )
            .await;
            bail!("{}", failure.message);
        }
    }
}

async fn search_auto(
    ctx: &RuntimeContext,
    raw_query: &str,
    count: usize,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<SearchResponse> {
    let (creds, fingerprints) = resolve_all_credentials(&ctx.config.state_dir)?;
    let full_order = auto_candidate_order(raw_query, options);
    let configured_order: Vec<ConcreteProvider> = full_order
        .into_iter()
        .filter(|&p| p == ConcreteProvider::DuckDuckGo || creds.contains_key(&p))
        .collect();

    let timeout_ms = ctx.config.timeout.as_millis().min(u64::MAX as u128) as u64;
    let mut tried = Vec::new();
    let mut attempts: Vec<ProviderAttempt> = Vec::new();
    let mut skipped_recorded: HashSet<ConcreteProvider> = HashSet::new();

    loop {
        let remaining: Vec<ConcreteProvider> = configured_order
            .iter()
            .copied()
            .filter(|p| !tried.contains(p))
            .collect();
        if remaining.is_empty() {
            break;
        }

        let decision = reserve_dispatch(
            &ctx.config.state_dir,
            remaining,
            fingerprints.clone(),
            timeout_ms,
        )
        .await?;

        let (provider, wait_ms, skipped) = match decision {
            DispatchDecision::Selected {
                provider,
                wait_ms,
                skipped,
            } => (provider, wait_ms, skipped),
            DispatchDecision::AllCooledDown { skipped } => {
                record_skipped_attempts(&mut attempts, &mut skipped_recorded, skipped);
                break;
            }
        };

        record_skipped_attempts(&mut attempts, &mut skipped_recorded, skipped);

        if wait_ms > 0 {
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }

        let secret = creds.get(&provider).map(|c| c.value.as_str());
        let fp = fingerprints.get(&provider).cloned();
        match execute_concrete_provider(ctx, provider, secret, raw_query, count, options, endpoints)
            .await
        {
            Ok(results) => {
                record_outcome(
                    &ctx.config.state_dir,
                    provider,
                    fp,
                    DispatchOutcome::Success,
                )
                .await?;
                return Ok(SearchResponse {
                    provider: provider.as_str(),
                    providers_used: vec![],
                    results,
                });
            }
            Err(failure) => {
                tracing::warn!(
                    "search provider {} failed ({}); trying next candidate",
                    provider.as_str(),
                    failure.message
                );
                let _ = record_outcome(
                    &ctx.config.state_dir,
                    provider,
                    fp,
                    DispatchOutcome::Failure {
                        status: failure.status,
                        cooldown_secs: failure.cooldown_secs,
                    },
                )
                .await;
                attempts.push(ProviderAttempt::Failed {
                    provider,
                    status: failure.status,
                    message: failure.message,
                });
                tried.push(provider);
            }
        }
    }

    bail!("{}", format_all_attempts_failed(&attempts))
}

async fn search_federated(
    ctx: &RuntimeContext,
    raw_query: &str,
    count: usize,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<SearchResponse> {
    let (creds, fingerprints) = resolve_all_credentials(&ctx.config.state_dir)?;
    let configured: Vec<ConcreteProvider> = creds.keys().copied().collect();
    let timeout_ms = ctx.config.timeout.as_millis().min(u64::MAX as u128) as u64;

    let reservation = reserve_federated(
        &ctx.config.state_dir,
        configured,
        fingerprints.clone(),
        timeout_ms,
    )
    .await?;

    let mut attempts: Vec<ProviderAttempt> = reservation
        .skipped
        .into_iter()
        .map(|s| ProviderAttempt::Skipped {
            provider: s.provider,
            cooldown_remaining_secs: s.cooldown_remaining_secs,
            last_error_status: s.last_error_status,
        })
        .collect();

    if reservation.selected.is_empty() {
        bail!("{}", format_all_attempts_failed(&attempts));
    }

    let sub_limit = count.clamp(10, 20);
    let mut successes: Vec<(usize, ConcreteProvider, Vec<SearchResult>)> = Vec::new();

    if reservation.selected.len() == 1 && reservation.selected[0].1 == ConcreteProvider::DuckDuckGo
    {
        let (slot, provider, _wait_ms) = reservation.selected[0];
        let effective_query = build_effective_query(raw_query, options.site.as_deref());
        match search_ddg(
            ctx,
            &endpoints.ddg_url,
            &effective_query,
            sub_limit,
            options.freshness,
        )
        .await
        {
            Ok(results) => {
                record_outcome(
                    &ctx.config.state_dir,
                    provider,
                    None,
                    DispatchOutcome::Success,
                )
                .await?;
                successes.push((slot, provider, results));
            }
            Err(failure) => {
                let _ = record_outcome(
                    &ctx.config.state_dir,
                    provider,
                    None,
                    DispatchOutcome::Failure {
                        status: failure.status,
                        cooldown_secs: failure.cooldown_secs,
                    },
                )
                .await;
                attempts.push(ProviderAttempt::Failed {
                    provider,
                    status: failure.status,
                    message: failure.message,
                });
            }
        }
    } else {
        let client = ctx.http_client()?;
        let timeout = ctx.config.timeout;
        let mut join_set: JoinSet<(
            usize,
            ConcreteProvider,
            Result<Vec<SearchResult>, ProviderFailure>,
        )> = JoinSet::new();

        for (slot, provider, wait_ms) in reservation.selected {
            let client = client.clone();
            let secret = creds
                .get(&provider)
                .map(|c| c.value.as_str().to_string())
                .unwrap_or_default();
            let raw_query = raw_query.to_string();
            let options = options.clone();
            let endpoints = endpoints.clone();
            join_set.spawn(async move {
                if wait_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
                let res = execute_json_provider(
                    &client, timeout, provider, &secret, &raw_query, sub_limit, &options,
                    &endpoints,
                )
                .await;
                (slot, provider, res)
            });
        }

        while let Some(joined) = join_set.join_next().await {
            let (slot, provider, res) = match joined {
                Ok(tuple) => tuple,
                Err(err) => {
                    tracing::warn!("federated search task panicked/cancelled: {err}");
                    continue;
                }
            };
            let fp = fingerprints.get(&provider).cloned();
            match res {
                Ok(results) => {
                    record_outcome(
                        &ctx.config.state_dir,
                        provider,
                        fp,
                        DispatchOutcome::Success,
                    )
                    .await?;
                    successes.push((slot, provider, results));
                }
                Err(failure) => {
                    tracing::warn!(
                        "federated search provider {} failed: {}",
                        provider.as_str(),
                        failure.message
                    );
                    let _ = record_outcome(
                        &ctx.config.state_dir,
                        provider,
                        fp,
                        DispatchOutcome::Failure {
                            status: failure.status,
                            cooldown_secs: failure.cooldown_secs,
                        },
                    )
                    .await;
                    attempts.push(ProviderAttempt::Failed {
                        provider,
                        status: failure.status,
                        message: failure.message,
                    });
                }
            }
        }
    }

    if successes.is_empty() {
        bail!("{}", format_all_attempts_failed(&attempts));
    }

    successes.sort_by_key(|(slot, _, _)| *slot);
    let providers_used: Vec<&'static str> = successes.iter().map(|(_, p, _)| p.as_str()).collect();
    let results = fuse_rrf(successes, count, options.extra_snippets);

    Ok(SearchResponse {
        provider: "federated",
        providers_used,
        results,
    })
}

struct FusedEntry {
    canonical_url: String,
    score: f64,
    provider_hit_count: usize,
    min_rank: usize,
    first_provider_slot: usize,
    title: String,
    url: String,
    snippet: String,
    extra_snippets: Vec<String>,
}

fn fuse_rrf(
    provider_results: Vec<(usize, ConcreteProvider, Vec<SearchResult>)>,
    limit: usize,
    include_extra_snippets: bool,
) -> Vec<SearchResult> {
    let mut fused: HashMap<String, FusedEntry> = HashMap::new();

    for (slot, _provider, results) in provider_results {
        let mut seen_in_provider: HashSet<String> = HashSet::new();
        for (idx, item) in results.into_iter().enumerate() {
            let canonical = url::Url::parse(&item.url)
                .ok()
                .map(|u| normalize_crawl_url(&u))
                .unwrap_or_else(|| item.url.clone());

            if !seen_in_provider.insert(canonical.clone()) {
                continue;
            }
            let rank = idx + 1;
            let contribution = 1.0 / (RRF_K + rank as f64);

            match fused.get_mut(&canonical) {
                Some(entry) => {
                    entry.score += contribution;
                    entry.provider_hit_count += 1;
                    entry.min_rank = entry.min_rank.min(rank);
                    entry.first_provider_slot = entry.first_provider_slot.min(slot);

                    if entry.title.is_empty() && !item.title.is_empty() {
                        entry.title = item.title;
                    }
                    if !item.snippet.is_empty() {
                        if item.snippet.len() > entry.snippet.len() {
                            let displaced = std::mem::replace(&mut entry.snippet, item.snippet);
                            if include_extra_snippets && !displaced.is_empty() {
                                push_unique_extra(
                                    &mut entry.extra_snippets,
                                    &entry.snippet,
                                    displaced,
                                );
                            }
                        } else if include_extra_snippets {
                            push_unique_extra(
                                &mut entry.extra_snippets,
                                &entry.snippet,
                                item.snippet,
                            );
                        }
                    }
                    for extra in item.extra_snippets {
                        push_unique_extra(&mut entry.extra_snippets, &entry.snippet, extra);
                    }
                }
                None => {
                    let mut extras = Vec::new();
                    for extra in item.extra_snippets {
                        push_unique_extra(&mut extras, &item.snippet, extra);
                    }
                    fused.insert(
                        canonical.clone(),
                        FusedEntry {
                            canonical_url: canonical,
                            score: contribution,
                            provider_hit_count: 1,
                            min_rank: rank,
                            first_provider_slot: slot,
                            title: item.title,
                            url: item.url,
                            snippet: item.snippet,
                            extra_snippets: extras,
                        },
                    );
                }
            }
        }
    }

    let mut ranked: Vec<FusedEntry> = fused.into_values().collect();
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.provider_hit_count.cmp(&a.provider_hit_count))
            .then_with(|| a.min_rank.cmp(&b.min_rank))
            .then_with(|| a.first_provider_slot.cmp(&b.first_provider_slot))
            .then_with(|| a.canonical_url.cmp(&b.canonical_url))
    });

    ranked
        .into_iter()
        .take(limit)
        .map(|entry| SearchResult {
            title: entry.title,
            url: entry.url,
            snippet: entry.snippet,
            extra_snippets: entry.extra_snippets,
        })
        .collect()
}

fn push_unique_extra(extras: &mut Vec<String>, primary_snippet: &str, candidate: String) {
    if extras.len() >= MAX_EXTRA_SNIPPETS {
        return;
    }
    let trimmed = candidate.trim();
    if trimmed.is_empty() || trimmed == primary_snippet || extras.iter().any(|e| e == trimmed) {
        return;
    }
    extras.push(trimmed.to_string());
}

async fn execute_concrete_provider(
    ctx: &RuntimeContext,
    provider: ConcreteProvider,
    secret: Option<&str>,
    raw_query: &str,
    limit: usize,
    options: &SearchOptions,
    endpoints: &ProviderEndpoints,
) -> Result<Vec<SearchResult>, ProviderFailure> {
    if provider == ConcreteProvider::DuckDuckGo {
        let effective_query = build_effective_query(raw_query, options.site.as_deref());
        return search_ddg(
            ctx,
            &endpoints.ddg_url,
            &effective_query,
            limit,
            options.freshness,
        )
        .await;
    }

    let client = ctx.http_client().map_err(|err| ProviderFailure {
        status: None,
        cooldown_secs: 60,
        message: format!("failed to initialize HTTP client: {err}"),
    })?;
    let secret = secret.unwrap_or_default();
    execute_json_provider(
        &client,
        ctx.config.timeout,
        provider,
        secret,
        raw_query,
        limit,
        options,
        endpoints,
    )
    .await
}

fn record_skipped_attempts(
    attempts: &mut Vec<ProviderAttempt>,
    recorded: &mut HashSet<ConcreteProvider>,
    skipped: Vec<SkippedProvider>,
) {
    for s in skipped {
        if recorded.insert(s.provider) {
            attempts.push(ProviderAttempt::Skipped {
                provider: s.provider,
                cooldown_remaining_secs: s.cooldown_remaining_secs,
                last_error_status: s.last_error_status,
            });
        }
    }
}

fn format_all_attempts_failed(attempts: &[ProviderAttempt]) -> String {
    if attempts.is_empty() {
        return "all search providers failed or are unavailable".to_string();
    }
    let details: Vec<String> = attempts
        .iter()
        .map(ProviderAttempt::format_summary)
        .collect();
    format!(
        "all search providers failed or are in cooldown: {}",
        details.join("; ")
    )
}

pub fn format_urls_only(response: &SearchResponse) -> String {
    response
        .results
        .iter()
        .map(|r| r.url.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_results(response: &SearchResponse) -> String {
    if response.results.is_empty() {
        return "No results found.".to_string();
    }

    let mut out = String::new();
    for (i, r) in response.results.iter().enumerate() {
        out.push_str(&format!("{}. [{}]({})\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("   {}\n", r.snippet));
        }
        for extra in &r.extra_snippets {
            out.push_str(&format!("   > {}\n", extra));
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}
