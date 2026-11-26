use url::Url;

const TRACKING_PARAMETERS: &[&str] = &[
    "fbclid",
    "gclid",
    "dclid",
    "msclkid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "_hsenc",
    "_hsmi",
    "ref_src",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedUrl {
    Resolved(String),
    UnwrapText,
    Drop,
}

/// Resolve a raw `href`/`src` attribute against `base` and strip tracking query parameters.
pub fn resolve_and_clean_url(raw: &str, base: &Url) -> ResolvedUrl {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ResolvedUrl::Drop;
    }

    if has_non_navigable_scheme(trimmed) {
        return ResolvedUrl::UnwrapText;
    }

    if trimmed.starts_with('#') {
        return ResolvedUrl::Resolved(trimmed.to_string());
    }

    let Ok(mut resolved) = base.join(trimmed) else {
        return ResolvedUrl::UnwrapText;
    };

    if matches!(resolved.scheme(), "javascript" | "vbscript" | "data") {
        return ResolvedUrl::UnwrapText;
    }

    strip_tracking_parameters(&mut resolved);
    ResolvedUrl::Resolved(resolved.to_string())
}

pub fn clean_url(url: &Url) -> Url {
    let mut cleaned = url.clone();
    strip_tracking_parameters(&mut cleaned);
    cleaned
}

/// Strip known tracking parameters without re-encoding queries that contain none.
pub fn strip_tracking_parameters(url: &mut Url) {
    let Some(query) = url.query() else {
        return;
    };
    if query.is_empty() {
        return;
    }

    let mut matched_any = false;
    let mut kept: Vec<&str> = Vec::new();

    for segment in query.split('&') {
        let raw_key = segment.split('=').next().unwrap_or(segment);
        if !raw_key.is_empty() && is_tracking_parameter_key(raw_key) {
            matched_any = true;
        } else {
            kept.push(segment);
        }
    }

    if !matched_any {
        return;
    }

    if kept.iter().all(|s| s.is_empty()) {
        url.set_query(None);
    } else {
        let updated: Vec<&str> = kept.into_iter().filter(|s| !s.is_empty()).collect();
        if updated.is_empty() {
            url.set_query(None);
        } else {
            let joined = updated.join("&");
            url.set_query(Some(&joined));
        }
    }
}

pub fn normalize_crawl_url(url: &Url) -> String {
    let mut cleaned = url.clone();
    cleaned.set_fragment(None);
    strip_tracking_parameters(&mut cleaned);
    let mut s = cleaned.to_string();
    if s.ends_with('/') && cleaned.path() != "/" {
        s.pop();
    }
    s
}

fn has_non_navigable_scheme(raw: &str) -> bool {
    let prefix: String = raw
        .chars()
        .filter(|&c| !c.is_ascii_control() && !c.is_ascii_whitespace())
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();

    prefix.starts_with("javascript:")
        || prefix.starts_with("vbscript:")
        || prefix.starts_with("data:")
}

fn is_tracking_parameter_key(raw_key: &str) -> bool {
    let decoded = urlencoding::decode(raw_key)
        .map(|cow| cow.to_ascii_lowercase())
        .unwrap_or_else(|_| raw_key.to_ascii_lowercase());
    decoded.starts_with("utm_") || TRACKING_PARAMETERS.contains(&decoded.as_str())
}
