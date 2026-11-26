use std::net::IpAddr;
use url::Url;

use crate::policy::parent_domain;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawSourceKind {
    Markdown,
    Code { language: Option<&'static str> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSourceCandidate {
    pub raw_url: Url,
    pub filename: String,
    pub kind: RawSourceKind,
}

/// Resolve deterministic raw-source URLs for supported code forges
/// (GitHub blobs, GitLab blobs, and GitHub Gists).
pub fn resolve_raw_source_candidate(url: &Url) -> Option<RawSourceCandidate> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();

    match host.as_str() {
        "github.com" | "www.github.com" => {
            if segments.len() < 5 || segments[2] != "blob" {
                return None;
            }
            let filename = (*segments.last()?).to_string();
            let kind = classify_filename(&filename)?;
            let ref_and_path = segments[3..].join("/");
            let raw_url = Url::parse(&format!(
                "https://raw.githubusercontent.com/{}/{}/{}",
                segments[0], segments[1], ref_and_path
            ))
            .ok()?;
            Some(RawSourceCandidate {
                raw_url,
                filename,
                kind,
            })
        }
        "gitlab.com" | "www.gitlab.com" => {
            let dash_pos = segments.windows(2).position(|w| w == ["-", "blob"])?;
            if dash_pos < 2 || segments.len() < dash_pos + 4 {
                return None;
            }
            let filename = (*segments.last()?).to_string();
            let kind = classify_filename(&filename)?;
            let prefix = segments[..dash_pos].join("/");
            let ref_and_path = segments[dash_pos + 2..].join("/");
            let raw_url = Url::parse(&format!(
                "https://gitlab.com/{}/-/raw/{}",
                prefix, ref_and_path
            ))
            .ok()?;
            Some(RawSourceCandidate {
                raw_url,
                filename,
                kind,
            })
        }
        "gist.github.com" => {
            if segments.len() != 2 {
                return None;
            }
            let user = segments[0];
            let gist_id = segments[1];
            if gist_id.len() < 8 || !gist_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return None;
            }
            let raw_url = Url::parse(&format!(
                "https://gist.githubusercontent.com/{}/{}/raw",
                user, gist_id
            ))
            .ok()?;
            Some(RawSourceCandidate {
                raw_url,
                filename: gist_id.to_string(),
                kind: RawSourceKind::Code { language: None },
            })
        }
        _ => None,
    }
}

/// Parse an RFC 8288 `Link` header for an `alternate` Markdown target
/// (`type="text/markdown"` or `type="text/x-markdown"`), enforcing SSRF and
/// no-HTTPS-downgrade guards against `base`.
pub fn parse_markdown_link_header(header: &str, base: &Url) -> Option<Url> {
    for entry in split_link_entries(header) {
        let trimmed = entry.trim();
        let Some(rest) = trimmed.strip_prefix('<') else {
            continue;
        };
        let (target_raw, params_raw) = match rest.split_once('>') {
            Some(pair) => pair,
            None => continue,
        };
        let target = target_raw.trim();
        if target.is_empty() {
            continue;
        }

        let mut is_alternate = false;
        let mut is_markdown = false;
        for param in split_params(params_raw) {
            let Some((key, val)) = param.split_once('=') else {
                continue;
            };
            let key = key.trim();
            let val = unquote_param(val.trim());
            if key.eq_ignore_ascii_case("rel") {
                if val
                    .split_ascii_whitespace()
                    .any(|tok| tok.eq_ignore_ascii_case("alternate"))
                {
                    is_alternate = true;
                }
            } else if key.eq_ignore_ascii_case("type") && is_markdown_content_type(Some(val)) {
                is_markdown = true;
            }
        }

        if is_alternate && is_markdown {
            if let Ok(resolved) = base.join(target) {
                if is_safe_alternate_url(base, &resolved) {
                    return Some(resolved);
                }
            }
        }
    }
    None
}

fn mime_prefix(content_type: Option<&str>) -> Option<&str> {
    let mime = content_type?.split(';').next()?.trim();
    (!mime.is_empty()).then_some(mime)
}

pub fn is_markdown_content_type(content_type: Option<&str>) -> bool {
    mime_prefix(content_type).is_some_and(|mime| {
        mime.eq_ignore_ascii_case("text/markdown") || mime.eq_ignore_ascii_case("text/x-markdown")
    })
}

pub fn is_mirror_acceptable_content_type(content_type: Option<&str>) -> bool {
    is_markdown_content_type(content_type)
        || mime_prefix(content_type).is_some_and(|mime| mime.eq_ignore_ascii_case("text/plain"))
}

pub fn is_safe_alternate_url(base: &Url, target: &Url) -> bool {
    if !matches!(target.scheme(), "http" | "https") {
        return false;
    }
    if base.scheme() == "https" && target.scheme() != "https" {
        return false;
    }
    let (Some(base_host), Some(target_host)) = (base.host_str(), target.host_str()) else {
        return false;
    };

    if is_local_or_private_host(base_host) {
        return base_host.eq_ignore_ascii_case(target_host)
            && base.port_or_known_default() == target.port_or_known_default();
    }

    if is_local_or_private_host(target_host) {
        return false;
    }

    same_origin_or_parent_domain(base_host, target_host)
        && base.port_or_known_default() == target.port_or_known_default()
}

pub fn is_safe_subresource_url(parent: &Url, target: &Url) -> bool {
    is_safe_alternate_url(parent, target)
}

pub fn compute_safe_referer(_parent: &Url, _target: &Url) -> Option<String> {
    None
}

fn classify_filename(filename: &str) -> Option<RawSourceKind> {
    let lower = filename.to_ascii_lowercase();

    if lower == "dockerfile" || lower.starts_with("dockerfile.") {
        return Some(RawSourceKind::Code {
            language: Some("dockerfile"),
        });
    }
    if lower == "makefile" || lower == "gnumakefile" {
        return Some(RawSourceKind::Code {
            language: Some("makefile"),
        });
    }

    if let Some((_, ext)) = lower.rsplit_once('.') {
        if is_binary_extension(ext) {
            return None;
        }
        if matches!(ext, "md" | "markdown" | "mdx") {
            return Some(RawSourceKind::Markdown);
        }
        let language = match ext {
            "rs" => Some("rust"),
            "py" => Some("python"),
            "js" | "mjs" | "cjs" => Some("javascript"),
            "ts" | "mts" | "cts" => Some("typescript"),
            "tsx" => Some("tsx"),
            "jsx" => Some("jsx"),
            "go" => Some("go"),
            "c" | "h" => Some("c"),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" => Some("cpp"),
            "m" | "mm" => Some("objc"),
            "java" => Some("java"),
            "kt" | "kts" => Some("kotlin"),
            "swift" => Some("swift"),
            "rb" => Some("ruby"),
            "sh" | "bash" => Some("bash"),
            "zsh" => Some("zsh"),
            "toml" => Some("toml"),
            "yaml" | "yml" => Some("yaml"),
            "json" | "jsonc" => Some("json"),
            "sql" => Some("sql"),
            "css" | "scss" => Some("css"),
            "html" | "htm" => Some("html"),
            "xml" | "svg" => Some("xml"),
            "proto" => Some("protobuf"),
            _ => None,
        };
        return Some(RawSourceKind::Code { language });
    }

    Some(RawSourceKind::Code { language: None })
}

fn is_binary_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "tiff"
            | "tif"
            | "avif"
            | "heic"
            | "pdf"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
            | "bz2"
            | "xz"
            | "7z"
            | "rar"
            | "wasm"
            | "exe"
            | "dll"
            | "so"
            | "dylib"
            | "bin"
            | "class"
            | "jar"
            | "pyc"
            | "pyo"
            | "o"
            | "a"
            | "mp3"
            | "mp4"
            | "mov"
            | "avi"
            | "mkv"
            | "wav"
            | "flac"
            | "ogg"
            | "webm"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "eot"
            | "xls"
            | "xlsx"
            | "doc"
            | "docx"
            | "ppt"
            | "pptx"
            | "sqlite"
            | "db"
            | "parquet"
            | "dmg"
            | "iso"
            | "deb"
            | "rpm"
            | "apk"
    )
}

fn split_link_entries(header: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut in_angles = false;
    let mut in_quotes = false;
    let mut escaped = false;

    for (idx, ch) in header.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' if !in_angles => in_quotes = !in_quotes,
            '<' if !in_quotes => in_angles = true,
            '>' if !in_quotes => in_angles = false,
            ',' if !in_angles && !in_quotes => {
                entries.push(&header[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start <= header.len() {
        entries.push(&header[start..]);
    }
    entries
}

fn split_params(params: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (idx, ch) in params.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ';' if !in_quotes => {
                let part = params[start..idx].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if start < params.len() {
        let part = params[start..].trim();
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts
}

fn unquote_param(val: &str) -> &str {
    val.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(val)
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64)
}

fn is_local_or_private_host(host: &str) -> bool {
    let normalized = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized.ends_with(".local")
        || normalized.ends_with(".internal")
    {
        return true;
    }
    if let Ok(ip) = normalized.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => is_private_v4(v4),
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return is_private_v4(v4);
                }
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        };
    }
    false
}

fn same_origin_or_parent_domain(base_host: &str, target_host: &str) -> bool {
    let a = base_host.trim_end_matches('.').to_ascii_lowercase();
    let b = target_host.trim_end_matches('.').to_ascii_lowercase();
    if a == b {
        return true;
    }
    if a.parse::<IpAddr>().is_ok() || b.parse::<IpAddr>().is_ok() {
        return false;
    }
    if is_subdomain_of(&a, &b) || is_subdomain_of(&b, &a) {
        return true;
    }
    match (parent_domain(&a), parent_domain(&b)) {
        (Some(pa), Some(pb)) => pa == pb && !is_shared_public_suffix(pa),
        _ => false,
    }
}

fn is_subdomain_of(child: &str, parent: &str) -> bool {
    parent.contains('.')
        && !is_shared_public_suffix(parent)
        && child.len() > parent.len() + 1
        && child.ends_with(parent)
        && child.as_bytes()[child.len() - parent.len() - 1] == b'.'
}

fn is_shared_public_suffix(domain: &str) -> bool {
    matches!(
        domain,
        "co.uk"
            | "org.uk"
            | "ac.uk"
            | "gov.uk"
            | "com.au"
            | "net.au"
            | "org.au"
            | "co.jp"
            | "ne.jp"
            | "or.jp"
            | "com.br"
            | "com.cn"
            | "co.in"
            | "co.nz"
            | "github.io"
            | "gitlab.io"
            | "pages.dev"
            | "netlify.app"
            | "vercel.app"
            | "herokuapp.com"
            | "appspot.com"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_github_gitlab_and_gist_raw_sources() {
        let gh_md = Url::parse("https://github.com/tokio-rs/tokio/blob/master/README.md").unwrap();
        let cand = resolve_raw_source_candidate(&gh_md).unwrap();
        assert_eq!(
            cand.raw_url.as_str(),
            "https://raw.githubusercontent.com/tokio-rs/tokio/master/README.md"
        );
        assert_eq!(cand.filename, "README.md");
        assert_eq!(cand.kind, RawSourceKind::Markdown);

        let gh_rs =
            Url::parse("https://github.com/tokio-rs/tokio/blob/master/tokio/src/lib.rs").unwrap();
        let cand_rs = resolve_raw_source_candidate(&gh_rs).unwrap();
        assert_eq!(
            cand_rs.raw_url.as_str(),
            "https://raw.githubusercontent.com/tokio-rs/tokio/master/tokio/src/lib.rs"
        );
        assert_eq!(
            cand_rs.kind,
            RawSourceKind::Code {
                language: Some("rust")
            }
        );

        let gh_bin = Url::parse("https://github.com/org/repo/blob/main/assets/logo.png").unwrap();
        assert!(resolve_raw_source_candidate(&gh_bin).is_none());

        let gl = Url::parse("https://gitlab.com/group/sub/proj/-/blob/main/Dockerfile").unwrap();
        let cand_gl = resolve_raw_source_candidate(&gl).unwrap();
        assert_eq!(
            cand_gl.raw_url.as_str(),
            "https://gitlab.com/group/sub/proj/-/raw/main/Dockerfile"
        );
        assert_eq!(
            cand_gl.kind,
            RawSourceKind::Code {
                language: Some("dockerfile")
            }
        );

        let gist = Url::parse("https://gist.github.com/octocat/0123456789abcdef").unwrap();
        let cand_gist = resolve_raw_source_candidate(&gist).unwrap();
        assert_eq!(
            cand_gist.raw_url.as_str(),
            "https://gist.githubusercontent.com/octocat/0123456789abcdef/raw"
        );

        let gist_revs =
            Url::parse("https://gist.github.com/octocat/0123456789abcdef/revisions").unwrap();
        assert!(resolve_raw_source_candidate(&gist_revs).is_none());
    }

    #[test]
    fn parses_rfc8288_markdown_link_header_with_ssrf_and_downgrade_guards() {
        let base = Url::parse("https://docs.example.com/guide/intro").unwrap();

        let rel = parse_markdown_link_header(
            r#", invalid-entry, </guide/intro.md>; rel="alternate"; type="text/markdown", </rss.xml>; rel="alternate"; type="application/rss+xml""#,
            &base,
        )
        .unwrap();
        assert_eq!(rel.as_str(), "https://docs.example.com/guide/intro.md");

        let sub = parse_markdown_link_header(
            r#"<https://raw.example.com/guide/intro.md>; rel="canonical alternate"; type="text/x-markdown; charset=utf-8""#,
            &base,
        )
        .unwrap();
        assert_eq!(sub.as_str(), "https://raw.example.com/guide/intro.md");

        // Reject HTTPS -> HTTP downgrade.
        assert!(parse_markdown_link_header(
            r#"<http://docs.example.com/guide/intro.md>; rel="alternate"; type="text/markdown""#,
            &base,
        )
        .is_none());

        // Reject cross-origin and private/loopback SSRF targets from public base.
        assert!(parse_markdown_link_header(
            r#"<https://evil.example.org/intro.md>; rel="alternate"; type="text/markdown""#,
            &base,
        )
        .is_none());
        assert!(parse_markdown_link_header(
            r#"<https://127.0.0.1/intro.md>; rel="alternate"; type="text/markdown""#,
            &base,
        )
        .is_none());
        assert!(parse_markdown_link_header(
            r#"<https://169.254.169.254/latest/meta-data>; rel="alternate"; type="text/markdown""#,
            &base,
        )
        .is_none());
    }
}
