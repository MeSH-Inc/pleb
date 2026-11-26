pub mod charset;
pub mod dom;
pub mod feed;
pub mod markdown;
pub mod native_markdown;
pub mod resources;
pub mod transcript;
pub mod url;

pub use charset::decode_text_bytes;
pub use dom::ParsedHtml;
pub use markdown::truncate_markdown;
pub use resources::{
    format_resource_manifest_markdown, ResourceDescriptor, ResourceKind, ResourceSalience,
};

use anyhow::{bail, Result};

pub(crate) const TRUNCATED_SUFFIX: &str = "\n\n[... truncated]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    #[value(alias = "md")]
    Markdown,
    Html,
    #[value(alias = "txt")]
    Text,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ExtractMode {
    #[default]
    Clean,
    Article,
    Full,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub format: OutputFormat,
    pub mode: ExtractMode,
    pub include_images: bool,
    pub include_links: bool,
    pub max_bytes: Option<usize>,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Markdown,
            mode: ExtractMode::Clean,
            include_images: true,
            include_links: true,
            max_bytes: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractedDocument {
    pub title: Option<String>,
    pub content: String,
    pub truncated: bool,
    pub resources: Vec<ResourceDescriptor>,
}

pub fn format_page_section(
    index: usize,
    total: usize,
    title: Option<&str>,
    url: &str,
    content: &str,
) -> String {
    let title = title.unwrap_or("Untitled");
    format!(
        "\n---\n## [{}/{}] {}\n**URL:** {}\n\n{}\n",
        index, total, title, url, content
    )
}

pub fn wrap_raw_code_document(
    filename: &str,
    code: &str,
    language: Option<&str>,
    options: &ExtractOptions,
) -> ExtractedDocument {
    if options.format == OutputFormat::Markdown {
        let fence_len = (longest_backtick_run(code) + 1).max(3);
        let fence = "`".repeat(fence_len);
        let lang = language.unwrap_or("");
        let trimmed_code = code.trim_end_matches('\n');
        let raw = format!("# {filename}\n\n{fence}{lang}\n{trimmed_code}\n{fence}");
        let (content, truncated) = match options.max_bytes {
            Some(max) => markdown::truncate_markdown(&raw, max),
            None => (raw, false),
        };
        return ExtractedDocument {
            title: Some(filename.to_string()),
            content,
            truncated,
            resources: Vec::new(),
        };
    }

    let (content, truncated) = apply_utf8_limit(code, options.max_bytes);
    ExtractedDocument {
        title: Some(filename.to_string()),
        content,
        truncated,
        resources: Vec::new(),
    }
}

fn longest_backtick_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut current = 0usize;
    for b in s.bytes() {
        if b == b'`' {
            current += 1;
            if current > best {
                best = current;
            }
        } else {
            current = 0;
        }
    }
    best
}

pub fn extract_document_bytes(
    body: &[u8],
    page_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    let decoded = decode_text_bytes(body, content_type);
    extract_document(&decoded, page_url, content_type, options)
}

pub fn extract_document(
    body: &str,
    page_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    extract_document_parsed(body, None, page_url, content_type, options)
}

pub fn extract_document_parsed(
    body: &str,
    parsed: Option<ParsedHtml>,
    page_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    if options.format == OutputFormat::Raw {
        bail!("OutputFormat::Raw must use raw byte passthrough, not extract_document");
    }

    let mime = extract_mime(content_type);
    if native_markdown::is_markdown_mime(mime.as_deref()) {
        return native_markdown::extract_native_markdown(body, page_url, options);
    }

    if feed::is_feed_document(mime.as_deref(), body) {
        return feed::extract_feed(body, page_url, mime.as_deref(), options);
    }

    if transcript::is_transcript_document(mime.as_deref(), body) {
        return transcript::extract_transcript(body, options);
    }

    if is_passthrough_non_html(content_type) {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
            resources: Vec::new(),
        });
    }

    if options.format == OutputFormat::Html {
        let doc = parsed.unwrap_or_else(|| ParsedHtml::parse(body));
        let title = doc.title();
        let effective_base = doc.effective_base_url(page_url);
        let resources = doc.extract_resources(options.mode, &effective_base);
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title,
            content,
            truncated,
            resources,
        });
    }

    let doc = parsed.unwrap_or_else(|| ParsedHtml::parse(body));
    let title = doc.title();
    let effective_base = doc.effective_base_url(page_url);

    match options.format {
        OutputFormat::Text => {
            let (raw_content, resources) =
                doc.into_plain_text_and_resources(options.mode, &effective_base);
            let (content, truncated) = match options.max_bytes {
                Some(max) => truncate_utf8(&raw_content, max),
                None => (raw_content, false),
            };
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
                resources,
            })
        }
        OutputFormat::Markdown => {
            let (pruned, resources) =
                doc.into_pruned_html_and_resources(options.mode, &effective_base);
            let md = markdown::html_to_compact_markdown(
                &pruned,
                &effective_base,
                options.include_images,
                options.include_links,
            )?;
            let trimmed_md = md.trim();
            let raw_content = match &title {
                Some(t) if !starts_with_title_heading(trimmed_md, t) => {
                    if trimmed_md.is_empty() {
                        format!("# {}", t)
                    } else {
                        format!("# {}\n\n{}", t, trimmed_md)
                    }
                }
                _ => trimmed_md.to_string(),
            };
            let (content, truncated) = match options.max_bytes {
                Some(max) => markdown::truncate_markdown(&raw_content, max),
                None => (raw_content, false),
            };
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
                resources,
            })
        }
        OutputFormat::Html | OutputFormat::Raw => unreachable!(),
    }
}

/// Detect binary payloads by MIME type or leading magic/NUL bytes (for missing/octet-stream MIMEs).
/// Returns `Some(detected_mime)` when the payload is binary and unsafe for text extraction.
pub fn detect_binary_content(content_type: Option<&str>, body: &[u8]) -> Option<Option<String>> {
    let mime = extract_mime(content_type);

    if let Some(ref m) = mime {
        let is_binary_mime = matches!(
            m.as_str(),
            "application/pdf"
                | "application/octet-stream"
                | "application/zip"
                | "application/gzip"
                | "application/x-gzip"
                | "application/x-tar"
                | "application/wasm"
                | "application/x-executable"
                | "application/vnd.ms-excel"
                | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
                | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        ) || m.starts_with("audio/")
            || m.starts_with("video/")
            || m.starts_with("font/")
            || (m.starts_with("image/") && m != "image/svg+xml");

        if is_binary_mime {
            return Some(Some(m.clone()));
        }
    }

    let sniff_allowed = mime.is_none() || mime.as_deref() == Some("application/octet-stream");
    if sniff_allowed && looks_like_binary_bytes(body) {
        return Some(mime);
    }

    None
}

fn looks_like_binary_bytes(body: &[u8]) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"%PDF-",
        b"\x89PNG\r\n\x1a\n",
        b"\xFF\xD8\xFF",
        b"GIF87a",
        b"GIF89a",
        b"PK\x03\x04",
        b"\x1F\x8B",
        b"\x00asm",
    ];
    if MAGICS.iter().any(|magic| body.starts_with(magic)) {
        return true;
    }
    let inspect = &body[..body.len().min(8192)];
    inspect.contains(&0)
}

pub(crate) fn apply_utf8_limit(text: &str, max_bytes: Option<usize>) -> (String, bool) {
    match max_bytes {
        Some(max) => truncate_utf8(text, max),
        None => (text.to_string(), false),
    }
}

pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), !text.is_empty());
    }

    if max_bytes <= TRUNCATED_SUFFIX.len() {
        let end = floor_char_boundary(text, max_bytes);
        return (text[..end].to_string(), true);
    }

    let limit = max_bytes - TRUNCATED_SUFFIX.len();
    let end = choose_boundary_cut(text, limit);
    let mut out = String::with_capacity(end + TRUNCATED_SUFFIX.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATED_SUFFIX);
    (out, true)
}

pub(crate) fn choose_boundary_cut(text: &str, limit: usize) -> usize {
    let clamped = floor_char_boundary(text, limit);
    let window_start = (limit * 85) / 100;
    if let Some(rel_nl) = text[..clamped].rfind('\n') {
        if rel_nl >= window_start && rel_nl > 0 {
            return rel_nl;
        }
    }
    clamped
}

pub(crate) fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(crate) fn starts_with_title_heading(md: &str, title: &str) -> bool {
    let first_line = md.lines().next().unwrap_or("").trim();
    let Some(heading) = first_line.strip_prefix("# ").map(str::trim) else {
        return false;
    };
    let title_trimmed = title.trim();
    if heading.eq_ignore_ascii_case(title_trimmed) {
        return true;
    }
    for sep in [" | ", " - ", " — ", " – ", " :: ", " · "] {
        if let Some((prefix, _)) = title_trimmed.split_once(sep) {
            let p = prefix.trim();
            if !p.is_empty() && heading.eq_ignore_ascii_case(p) {
                return true;
            }
        }
    }
    false
}

pub fn extract_mime(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    let mime = ct.split(';').next()?.trim().to_ascii_lowercase();
    (!mime.is_empty()).then_some(mime)
}

pub fn is_html_content_type(content_type: Option<&str>) -> bool {
    match extract_mime(content_type).as_deref() {
        None => true,
        Some("text/html" | "application/xhtml+xml") => true,
        Some(_) => false,
    }
}

fn is_passthrough_non_html(content_type: Option<&str>) -> bool {
    let Some(mime) = extract_mime(content_type) else {
        return false;
    };
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return false;
    }
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/ecmascript"
                | "application/yaml"
                | "application/x-yaml"
                | "application/toml"
                | "application/graphql"
                | "image/svg+xml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}
