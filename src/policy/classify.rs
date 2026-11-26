use crate::extract::dom;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseClassification {
    Ok,
    Blocked { status: u16 },
    Challenge { status: u16, reason: String },
    JavaScriptShell { reason: String },
    HttpError { status: u16 },
}

pub fn classify_response(status: u16, is_html: bool, body: &str) -> ResponseClassification {
    let mut parsed = None;
    classify_response_with_parsed(status, is_html, body, &mut parsed)
}

pub fn classify_response_with_parsed(
    status: u16,
    is_html: bool,
    body: &str,
    parsed: &mut Option<dom::ParsedHtml>,
) -> ResponseClassification {
    classify_response_with_context(status, is_html, None, None, body, parsed)
}

pub fn classify_response_with_context(
    status: u16,
    is_html: bool,
    requested_url: Option<&str>,
    final_url: Option<&str>,
    body: &str,
    parsed: &mut Option<dom::ParsedHtml>,
) -> ResponseClassification {
    if status == 403 || status == 503 {
        let lower = body.to_ascii_lowercase();
        if let Some(reason) = detect_challenge_lower(&lower) {
            return ResponseClassification::Challenge { status, reason };
        }
        return ResponseClassification::Blocked { status };
    }

    if status >= 400 {
        return ResponseClassification::HttpError { status };
    }

    if is_html {
        if body.len() > 2_000_000 {
            return ResponseClassification::Ok;
        }

        // Gate HTTP-200 challenge/shell/soft-block detection on post-strip prose volume.
        // Bodies under 600 raw bytes cannot reach 600 prose bytes, preserving lazy parsing.
        let metrics_if_large = if body.len() >= 600 {
            let doc = parsed.get_or_insert_with(|| dom::ParsedHtml::parse(body));
            let m = doc.text_metrics();
            if m.prose_bytes >= 600 {
                return ResponseClassification::Ok;
            }
            Some(m)
        } else {
            None
        };

        let lower = body.to_ascii_lowercase();
        if let Some(reason) = detect_interstitial_challenge_lower(&lower, body.len()) {
            return ResponseClassification::Challenge { status, reason };
        }
        if let Some(reason) = detect_js_shell_with_parsed_lower(body, &lower, parsed) {
            return ResponseClassification::JavaScriptShell { reason };
        }

        let metrics = metrics_if_large.unwrap_or_else(|| {
            let doc = parsed.get_or_insert_with(|| dom::ParsedHtml::parse(body));
            doc.text_metrics()
        });

        if metrics.prose_bytes < 600 && is_login_redirect(requested_url, final_url) {
            return ResponseClassification::Blocked { status };
        }
        if metrics.prose_bytes < 400 && (metrics.has_password_input || is_cookie_wall_lower(&lower))
        {
            return ResponseClassification::Blocked { status };
        }
        if metrics.prose_bytes < 300 {
            let title = parsed.as_ref().and_then(dom::ParsedHtml::title);
            if title.as_deref().is_some_and(is_soft_404_title) {
                return ResponseClassification::HttpError { status: 404 };
            }
        }
    }

    ResponseClassification::Ok
}

fn is_auth_path(raw_url: &str) -> bool {
    let path = url::Url::parse(raw_url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|_| raw_url.to_ascii_lowercase());
    ["/login", "/signin", "/sign-in", "/sso", "/auth", "/oauth"]
        .iter()
        .any(|seg| {
            path.strip_prefix(seg).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with('/') || rest.starts_with('?')
            })
        })
}

fn is_login_redirect(requested_url: Option<&str>, final_url: Option<&str>) -> bool {
    let (Some(req), Some(fin)) = (requested_url, final_url) else {
        return false;
    };
    !is_auth_path(req) && is_auth_path(fin)
}

fn is_cookie_wall_lower(lower: &str) -> bool {
    [
        "before you continue to",
        "accept cookies to continue",
        "enable cookies to continue",
    ]
    .iter()
    .any(|sig| lower.contains(sig))
}

fn is_soft_404_title(title: &str) -> bool {
    let lower = title.trim().to_ascii_lowercase();
    lower == "404"
        || lower == "not found"
        || lower.starts_with("404 ")
        || lower.starts_with("404:")
        || lower.starts_with("404 -")
        || lower.contains("404 not found")
        || lower.contains("page not found")
}

pub fn detect_challenge(body: &str) -> Option<String> {
    detect_challenge_lower(&body.to_ascii_lowercase())
}

fn detect_challenge_lower(lower: &str) -> Option<String> {
    let signals = [
        (
            "challenges.cloudflare.com",
            "Cloudflare Turnstile/challenge",
        ),
        ("cf-browser-verification", "Cloudflare browser verification"),
        ("just a moment...", "Cloudflare challenge interstitial"),
        ("checking your browser", "Browser verification challenge"),
        ("google.com/recaptcha", "Google reCAPTCHA"),
        ("hcaptcha.com", "hCaptcha challenge"),
        ("captcha-delivery.com", "DataDome challenge"),
        ("px-captcha", "PerimeterX challenge"),
        ("awswaf", "AWS WAF challenge"),
    ];
    for (sig, label) in signals {
        if lower.contains(sig) {
            return Some(label.to_string());
        }
    }
    None
}

pub fn detect_interstitial_challenge(body: &str) -> Option<String> {
    detect_interstitial_challenge_lower(&body.to_ascii_lowercase(), body.len())
}

fn detect_interstitial_challenge_lower(lower: &str, body_len: usize) -> Option<String> {
    if lower.contains("just a moment")
        && (lower.contains("performing security verification")
            || lower.contains("checking your browser")
            || lower.contains("challenge-platform"))
    {
        return Some("Cloudflare challenge interstitial".to_string());
    }
    if lower.contains("captcha-delivery.com") && body_len < 50_000 {
        return Some("DataDome interstitial".to_string());
    }
    None
}

pub fn detect_js_shell(body: &str) -> Option<String> {
    let mut parsed = None;
    let lower = body.to_ascii_lowercase();
    detect_js_shell_with_parsed_lower(body, &lower, &mut parsed)
}

fn detect_js_shell_with_parsed_lower(
    body: &str,
    lower: &str,
    parsed: &mut Option<dom::ParsedHtml>,
) -> Option<String> {
    if body.len() > 2_000_000 {
        return None;
    }

    if crate::extract::youtube::has_embedded_youtube_metadata(body) {
        return None;
    }

    const SIGNALS: &[&str] = &[
        "javascript is not available",
        "we've detected that javascript is disabled",
        "we’ve detected that javascript is disabled",
        "you need to enable javascript to run this app",
        "please enable javascript to continue",
        "enable javascript and cookies to continue",
        "this site requires javascript",
        "necessary to enable javascript",
        "enable javascript to see google maps",
        "when you have eliminated the javascript",
    ];
    for sig in SIGNALS {
        if lower.contains(sig) {
            return Some(format!("noscript signal ({})", sig));
        }
    }

    let doc = parsed.get_or_insert_with(|| dom::ParsedHtml::parse(body));
    let metrics = doc.text_metrics();
    if (metrics.visible_text_len == 0 && metrics.script_count >= 1)
        || (metrics.visible_text_len < 200
            && (metrics.has_empty_spa_root || metrics.script_count >= 3))
    {
        return Some(format!(
            "empty SPA shell (visible_text_len={}, scripts={}, empty_root={})",
            metrics.visible_text_len, metrics.script_count, metrics.has_empty_spa_root
        ));
    }

    None
}
