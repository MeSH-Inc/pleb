use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::CredentialProvider;
use crate::context::{acquire_private_lock, ensure_private_dir, write_private_file_atomic};

use super::{ConcreteProvider, Freshness};

const STATE_FILE_NAME: &str = "search-state.json";
const LOCK_FILE_NAME: &str = "search-state.lock";
pub(crate) const BRAVE_BURST_WINDOW_MS: u64 = 1_100;
const BRAVE_MAX_PACING_WAIT_MS: u64 = 2_500;
const MAX_FUTURE_SLOT_HORIZON_MS: u64 = 5_000;

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

#[derive(Debug, Clone)]
pub(crate) struct SkippedProvider {
    pub provider: ConcreteProvider,
    pub cooldown_remaining_secs: u64,
    pub last_error_status: Option<u16>,
}

#[derive(Debug, Clone)]
pub(crate) enum DispatchDecision {
    Selected {
        provider: ConcreteProvider,
        wait_ms: u64,
        skipped: Vec<SkippedProvider>,
    },
    AllCooledDown {
        skipped: Vec<SkippedProvider>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct FederatedReservation {
    pub selected: Vec<(usize, ConcreteProvider, u64)>,
    pub skipped: Vec<SkippedProvider>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DispatchOutcome {
    Success,
    Failure {
        status: Option<u16>,
        cooldown_secs: u64,
    },
}

pub(crate) fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Howard Hinnant's civil-from-days algorithm: days since 1970-01-01 -> (year, month 1..=12, day 1..=31).
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// Howard Hinnant's days-from-civil algorithm: (year, month 1..=12, day 1..=31) -> days since 1970-01-01.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = month as u64;
    let d = day as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

pub(crate) fn utc_month_key(epoch_secs: u64) -> String {
    let (year, month, _) = civil_from_days((epoch_secs / 86_400) as i64);
    format!("{year:04}-{month:02}")
}

pub(crate) fn format_rfc3339_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

pub(crate) fn exa_start_published_date(freshness: Freshness, now_secs: u64) -> String {
    let delta_secs = match freshness {
        Freshness::Day => 86_400,
        Freshness::Week => 7 * 86_400,
        Freshness::Month => 30 * 86_400,
        Freshness::Year => 365 * 86_400,
    };
    format_rfc3339_utc(now_secs.saturating_sub(delta_secs))
}

/// Parses RFC 9110 `Retry-After` as integer delay-seconds or IMF-fixdate, clamping to `5..=300`s.
pub fn parse_retry_after(header_value: &str, now_secs: u64) -> Option<u64> {
    let trimmed = header_value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(secs.clamp(5, 300));
    }

    // RFC 9110 IMF-fixdate: "Sun, 06 Nov 1994 08:49:37 GMT"
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() != 6 || !parts[5].eq_ignore_ascii_case("GMT") {
        return None;
    }
    if !parts[0].ends_with(',') {
        return None;
    }
    let day: u32 = parts[1].parse().ok()?;
    let month: u32 = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    if parts[3].len() != 4 {
        return None;
    }
    let year: i64 = parts[3].parse().ok()?;
    if year < 1970 {
        return None;
    }
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let hour: u64 = hms[0].parse().ok()?;
    let min: u64 = hms[1].parse().ok()?;
    let sec: u64 = hms[2].parse().ok()?;
    if hour > 23 || min > 59 || sec > 60 {
        return None;
    }

    let days = days_from_civil(year, month, day);
    if days < 0 || civil_from_days(days) != (year, month, day) {
        return None;
    }
    let target_secs = (days as u64)
        .checked_mul(86_400)?
        .checked_add(hour * 3_600 + min * 60 + sec.min(59))?;
    let delta = target_secs.saturating_sub(now_secs);
    Some(delta.clamp(5, 300))
}

/// Computes a deterministic, non-reversible truncated SHA-256 hex fingerprint of a secret.
pub(crate) fn key_fingerprint(secret: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, secret.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest.as_ref()[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

pub(crate) fn is_cooldown_active(
    entry: &ProviderHealthEntry,
    current_fingerprint: Option<&str>,
    now_secs: u64,
) -> bool {
    if entry.cooldown_until_epoch_secs <= now_secs {
        return false;
    }
    if let (Some(stored), Some(current)) = (entry.key_fingerprint.as_deref(), current_fingerprint) {
        if stored != current {
            return false;
        }
    }
    true
}

fn brave_burst_state(entry: Option<&ProviderHealthEntry>, now_ms: u64) -> (bool, u64) {
    let Some(entry) = entry else {
        return (false, 0);
    };
    let last = entry.last_used_epoch_ms;
    if last == 0 {
        return (false, 0);
    }
    // Guard clock rollback while preserving legitimate future slot reservations.
    if last > now_ms.saturating_add(MAX_FUTURE_SLOT_HORIZON_MS) {
        return (false, 0);
    }
    if last > now_ms {
        return (true, last);
    }
    if now_ms.saturating_sub(last) < BRAVE_BURST_WINDOW_MS {
        (true, last)
    } else {
        (false, last)
    }
}

fn record_reservation(
    map: &mut BTreeMap<String, ProviderHealthEntry>,
    provider: ConcreteProvider,
    fingerprint: Option<&str>,
    slot_ms: u64,
    month_key: &str,
) {
    let entry = map.entry(provider.as_str().to_string()).or_default();
    if entry.month_key != month_key {
        entry.month_key = month_key.to_string();
        entry.month_requests = 0;
    }
    entry.month_requests = entry.month_requests.saturating_add(1);
    entry.last_used_epoch_ms = slot_ms;
    if let (Some(stored), Some(current)) = (entry.key_fingerprint.as_deref(), fingerprint) {
        if stored != current {
            entry.cooldown_until_epoch_secs = 0;
            entry.last_error_status = None;
        }
    }
    if let Some(fp) = fingerprint {
        entry.key_fingerprint = Some(fp.to_string());
    }
}

pub(crate) async fn reserve_dispatch(
    state_dir: &Path,
    ordered_candidates: Vec<ConcreteProvider>,
    fingerprints: BTreeMap<ConcreteProvider, String>,
    timeout_ms: u64,
) -> Result<DispatchDecision> {
    let dir = state_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        reserve_dispatch_sync(
            &dir,
            &ordered_candidates,
            &fingerprints,
            now_epoch_ms(),
            timeout_ms,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("search state task failed: {e}"))?
}

pub(crate) fn reserve_dispatch_sync(
    state_dir: &Path,
    ordered_candidates: &[ConcreteProvider],
    fingerprints: &BTreeMap<ConcreteProvider, String>,
    now_ms: u64,
    timeout_ms: u64,
) -> Result<DispatchDecision> {
    ensure_private_dir(state_dir)?;
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let path = state_dir.join(STATE_FILE_NAME);
    let mut map = read_state_map(&path);

    let now_secs = now_ms / 1_000;
    let month_key = utc_month_key(now_secs);
    let mut skipped = Vec::new();
    let mut healthy = Vec::new();

    for &provider in ordered_candidates {
        let fp = fingerprints.get(&provider).map(String::as_str);
        if let Some(entry) = map.get(provider.as_str()) {
            if is_cooldown_active(entry, fp, now_secs) {
                skipped.push(SkippedProvider {
                    provider,
                    cooldown_remaining_secs: entry
                        .cooldown_until_epoch_secs
                        .saturating_sub(now_secs),
                    last_error_status: entry.last_error_status,
                });
                continue;
            }
        }
        healthy.push(provider);
    }

    if healthy.is_empty() {
        return Ok(DispatchDecision::AllCooledDown { skipped });
    }

    let (brave_in_burst, brave_prev_ms) = brave_burst_state(map.get("brave"), now_ms);
    let mut selected = healthy[0];
    let mut wait_ms = 0u64;

    if selected == ConcreteProvider::Brave && brave_in_burst {
        // Demote Brave behind any other healthy configured provider (ahead of DuckDuckGo).
        if let Some(&alt) = healthy
            .iter()
            .skip(1)
            .find(|&&p| p != ConcreteProvider::DuckDuckGo)
        {
            selected = alt;
        } else {
            // Brave is the only healthy configured provider: reserve next pacing slot.
            let cap = timeout_ms.min(BRAVE_MAX_PACING_WAIT_MS);
            let raw_wait = brave_prev_ms
                .saturating_add(BRAVE_BURST_WINDOW_MS)
                .saturating_sub(now_ms);
            wait_ms = raw_wait.min(cap);
        }
    }

    let slot_ms = now_ms.saturating_add(wait_ms);
    let fp = fingerprints.get(&selected).map(String::as_str);
    record_reservation(&mut map, selected, fp, slot_ms, &month_key);
    save_state_atomic(&path, &map)?;

    Ok(DispatchDecision::Selected {
        provider: selected,
        wait_ms,
        skipped,
    })
}

pub(crate) async fn reserve_explicit(
    state_dir: &Path,
    provider: ConcreteProvider,
    fingerprint: Option<String>,
) -> Result<()> {
    let dir = state_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        ensure_private_dir(&dir)?;
        let _lock = acquire_private_lock(&dir.join(LOCK_FILE_NAME))?;
        let path = dir.join(STATE_FILE_NAME);
        let mut map = read_state_map(&path);
        let now_ms = now_epoch_ms();
        let month_key = utc_month_key(now_ms / 1_000);
        record_reservation(
            &mut map,
            provider,
            fingerprint.as_deref(),
            now_ms,
            &month_key,
        );
        save_state_atomic(&path, &map)
    })
    .await
    .map_err(|e| anyhow::anyhow!("search state task failed: {e}"))?
}

pub(crate) async fn reserve_federated(
    state_dir: &Path,
    configured: Vec<ConcreteProvider>,
    fingerprints: BTreeMap<ConcreteProvider, String>,
    timeout_ms: u64,
) -> Result<FederatedReservation> {
    let dir = state_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        reserve_federated_sync(&dir, &configured, &fingerprints, now_epoch_ms(), timeout_ms)
    })
    .await
    .map_err(|e| anyhow::anyhow!("search state task failed: {e}"))?
}

fn reserve_federated_sync(
    state_dir: &Path,
    configured: &[ConcreteProvider],
    fingerprints: &BTreeMap<ConcreteProvider, String>,
    now_ms: u64,
    timeout_ms: u64,
) -> Result<FederatedReservation> {
    ensure_private_dir(state_dir)?;
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let path = state_dir.join(STATE_FILE_NAME);
    let mut map = read_state_map(&path);

    let now_secs = now_ms / 1_000;
    let month_key = utc_month_key(now_secs);
    let mut skipped = Vec::new();
    let (brave_in_burst, brave_prev_ms) = brave_burst_state(map.get("brave"), now_ms);

    let preferred_order: &[ConcreteProvider] = if brave_in_burst {
        &[
            ConcreteProvider::Tavily,
            ConcreteProvider::Serper,
            ConcreteProvider::Exa,
            ConcreteProvider::Searxng,
            ConcreteProvider::Brave,
        ]
    } else {
        &[
            ConcreteProvider::Brave,
            ConcreteProvider::Serper,
            ConcreteProvider::Exa,
            ConcreteProvider::Tavily,
            ConcreteProvider::Searxng,
        ]
    };

    let mut chosen = Vec::new();
    for &provider in preferred_order {
        if !configured.contains(&provider) {
            continue;
        }
        let fp = fingerprints.get(&provider).map(String::as_str);
        if let Some(entry) = map.get(provider.as_str()) {
            if is_cooldown_active(entry, fp, now_secs) {
                skipped.push(SkippedProvider {
                    provider,
                    cooldown_remaining_secs: entry
                        .cooldown_until_epoch_secs
                        .saturating_sub(now_secs),
                    last_error_status: entry.last_error_status,
                });
                continue;
            }
        }
        chosen.push(provider);
        if chosen.len() == 3 {
            break;
        }
    }

    if chosen.is_empty() {
        // 0 configured providers or all configured providers are in cooldown -> degrade to DDG.
        let ddg = ConcreteProvider::DuckDuckGo;
        if let Some(entry) = map.get(ddg.as_str()) {
            if is_cooldown_active(entry, None, now_secs) {
                skipped.push(SkippedProvider {
                    provider: ddg,
                    cooldown_remaining_secs: entry
                        .cooldown_until_epoch_secs
                        .saturating_sub(now_secs),
                    last_error_status: entry.last_error_status,
                });
                return Ok(FederatedReservation {
                    selected: Vec::new(),
                    skipped,
                });
            }
        }
        chosen.push(ddg);
    }

    let mut selected = Vec::with_capacity(chosen.len());
    for (slot, provider) in chosen.into_iter().enumerate() {
        let wait_ms = if provider == ConcreteProvider::Brave && brave_in_burst {
            let cap = timeout_ms.min(BRAVE_MAX_PACING_WAIT_MS);
            brave_prev_ms
                .saturating_add(BRAVE_BURST_WINDOW_MS)
                .saturating_sub(now_ms)
                .min(cap)
        } else {
            0
        };
        let slot_ms = now_ms.saturating_add(wait_ms);
        let fp = fingerprints.get(&provider).map(String::as_str);
        record_reservation(&mut map, provider, fp, slot_ms, &month_key);
        selected.push((slot, provider, wait_ms));
    }

    save_state_atomic(&path, &map)?;
    Ok(FederatedReservation { selected, skipped })
}

pub(crate) async fn record_outcome(
    state_dir: &Path,
    provider: ConcreteProvider,
    fingerprint: Option<String>,
    outcome: DispatchOutcome,
) -> Result<()> {
    let dir = state_dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        record_outcome_sync(
            &dir,
            provider,
            fingerprint.as_deref(),
            outcome,
            now_epoch_secs(),
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("search state task failed: {e}"))?
}

pub(crate) fn record_outcome_sync(
    state_dir: &Path,
    provider: ConcreteProvider,
    fingerprint: Option<&str>,
    outcome: DispatchOutcome,
    now_secs: u64,
) -> Result<()> {
    ensure_private_dir(state_dir)?;
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let path = state_dir.join(STATE_FILE_NAME);
    let mut map = read_state_map(&path);

    let entry = map.entry(provider.as_str().to_string()).or_default();
    match outcome {
        DispatchOutcome::Success => {
            entry.cooldown_until_epoch_secs = 0;
            entry.last_error_status = None;
            if let Some(fp) = fingerprint {
                entry.key_fingerprint = Some(fp.to_string());
            }
        }
        DispatchOutcome::Failure {
            status,
            cooldown_secs,
        } => {
            entry.last_error_status = status;
            if cooldown_secs > 0 {
                entry.cooldown_until_epoch_secs = now_secs.saturating_add(cooldown_secs);
            }
            if let Some(fp) = fingerprint {
                entry.key_fingerprint = Some(fp.to_string());
            }
        }
    }

    save_state_atomic(&path, &map)
}

pub fn provider_health_summary(
    state_dir: &Path,
) -> Result<BTreeMap<&'static str, ProviderHealthEntry>> {
    let path = state_dir.join(STATE_FILE_NAME);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let raw = read_state_map(&path);
    let current_month = utc_month_key(now_epoch_secs());

    let mut summary = BTreeMap::new();
    for provider in ConcreteProvider::ALL {
        let name = provider.as_str();
        if let Some(mut entry) = raw.get(name).cloned() {
            if entry.month_key != current_month {
                entry.month_requests = 0;
            }
            summary.insert(name, entry);
        }
    }
    Ok(summary)
}

pub fn clear_search_state(state_dir: &Path) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let path = state_dir.join(STATE_FILE_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn clear_provider_cooldown(state_dir: &Path, provider: CredentialProvider) -> Result<()> {
    if !state_dir.exists() {
        return Ok(());
    }
    let _lock = acquire_private_lock(&state_dir.join(LOCK_FILE_NAME))?;
    let path = state_dir.join(STATE_FILE_NAME);
    if !path.exists() {
        return Ok(());
    }
    let mut map = read_state_map(&path);
    if let Some(entry) = map.get_mut(provider.as_str()) {
        entry.cooldown_until_epoch_secs = 0;
        entry.last_error_status = None;
        save_state_atomic(&path, &map)?;
    }
    Ok(())
}

fn read_state_map(path: &Path) -> BTreeMap<String, ProviderHealthEntry> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(err) => {
            tracing::warn!("failed to read search state {}: {}", path.display(), err);
            return BTreeMap::new();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(map) => map,
        Err(err) => {
            tracing::warn!(
                "search state {} contains corrupted JSON, starting empty: {}",
                path.display(),
                err
            );
            BTreeMap::new()
        }
    }
}

fn save_state_atomic(path: &Path, map: &BTreeMap<String, ProviderHealthEntry>) -> Result<()> {
    let payload = serde_json::to_vec_pretty(map)?;
    write_private_file_atomic(path, &payload)?;
    Ok(())
}
