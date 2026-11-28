pub mod axtree;
pub mod charset;
pub mod dom;
pub mod feed;
pub mod focus;
pub mod markdown;
pub mod native_markdown;
pub mod resources;
pub mod transcript;
pub mod ui_gates;
pub mod url;
pub mod webmcp;
pub mod youtube;

pub use axtree::format_axtree_from_cdp_json;
pub use charset::decode_text_bytes;
pub use dom::ParsedHtml;
pub use markdown::truncate_markdown;
pub use resources::{
    format_resource_manifest_markdown, resource_identifier, ResourceDescriptor, ResourceKind,
    ResourceSalience,
};
pub use ui_gates::{
    format_ui_gates_cue, format_ui_gates_cue_with_hint, ui_gate_identifier, UiGateDescriptor,
    UiGateKind, UiGatePresence,
};
pub use webmcp::{
    format_webmcp_cue, format_webmcp_cue_with_hint, format_webmcp_manifest_markdown,
    merge_webmcp_tools, sanitize_webmcp_tool, WebMcpToolAnnotations, WebMcpToolDescriptor,
    WebMcpToolSource,
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
    #[value(alias = "ax")]
    Axtree,
    Raw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ExtractMode {
    #[default]
    Clean,
    Article,
    Full,
}

/// Policy for rescuing paired in-landmark ARIA disclosures already present in the DOM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ExpandPolicy {
    /// Preserve existing pruning of all hidden/collapsed panels (default).
    #[default]
    None,
    /// Rescue uniquely paired in-landmark ARIA `[role="tabpanel"]` elements under `Tab: <label>` headings.
    Tabs,
    /// Rescue paired tabpanels plus uniquely paired in-landmark `[aria-expanded="false"][aria-controls]` disclosures.
    All,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub format: OutputFormat,
    pub mode: ExtractMode,
    pub expand: ExpandPolicy,
    pub include_images: bool,
    pub include_links: bool,
    pub max_bytes: Option<usize>,
    pub focus: Option<String>,
    pub section: Option<String>,
    pub offset: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            format: OutputFormat::Markdown,
            mode: ExtractMode::Clean,
            expand: ExpandPolicy::None,
            include_images: true,
            include_links: true,
            max_bytes: None,
            focus: None,
            section: None,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtractedDocument {
    pub title: Option<String>,
    pub content: String,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub warning: Option<String>,
    pub resources: Vec<ResourceDescriptor>,
    pub tools: Vec<WebMcpToolDescriptor>,
    pub ui_gates: Vec<UiGateDescriptor>,
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
    let raw = if options.format == OutputFormat::Markdown {
        let fence = "`".repeat((longest_backtick_run(code) + 1).max(3));
        let lang = language.unwrap_or("");
        let trimmed_code = code.trim_end_matches('\n');
        format!("# {filename}\n\n{fence}{lang}\n{trimmed_code}\n{fence}")
    } else {
        code.to_string()
    };
    let proj = apply_projection_or_slice(&raw, options);
    ExtractedDocument {
        title: Some(filename.to_string()),
        content: proj.content,
        truncated: proj.truncated,
        next_offset: proj.next_offset,
        warning: proj.warning,
        resources: Vec::new(),
        tools: Vec::new(),
        ui_gates: Vec::new(),
    }
}

pub(crate) fn longest_backtick_run(s: &str) -> usize {
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

pub(crate) fn is_concealed_unicode(c: char) -> bool {
    matches!(
        c,
        '\u{E0000}'..='\u{E007F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn strip_concealed_unicode(s: &mut String) {
    if s.chars().any(is_concealed_unicode) {
        s.retain(|c| !is_concealed_unicode(c));
    }
}

pub fn extract_document_parsed(
    body: &str,
    parsed: Option<ParsedHtml>,
    page_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    extract_document_core(body, parsed, page_url, page_url, content_type, options)
}

pub(crate) fn extract_document_with_link_base(
    body: &str,
    page_url: &::url::Url,
    link_base_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    extract_document_core(body, None, page_url, link_base_url, content_type, options)
}

fn extract_document_core(
    body: &str,
    parsed: Option<ParsedHtml>,
    page_url: &::url::Url,
    link_base_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    let defer_slice = options.focus.is_some()
        || options.section.is_some()
        || options.offset > 0
        || options.max_bytes.is_some();
    let inner_opts = if defer_slice {
        ExtractOptions {
            max_bytes: None,
            focus: None,
            section: None,
            offset: 0,
            ..options.clone()
        }
    } else {
        options.clone()
    };
    let mut doc = extract_document_parsed_inner(
        body,
        parsed,
        page_url,
        link_base_url,
        content_type,
        &inner_opts,
    )?;
    if options.mode != ExtractMode::Full
        && options.format != OutputFormat::Html
        && !is_passthrough_non_html(content_type)
    {
        strip_concealed_unicode(&mut doc.content);
        if let Some(title) = doc.title.as_mut() {
            strip_concealed_unicode(title);
        }
    }
    if defer_slice {
        let proj = apply_projection_or_slice(&doc.content, options);
        doc.content = proj.content;
        doc.truncated |= proj.truncated;
        doc.next_offset = proj.next_offset;
        if let Some(w) = proj.warning {
            doc.warning = Some(match doc.warning.take() {
                Some(prev) => format!("{prev}; {w}"),
                None => w,
            });
        }
    }
    Ok(doc)
}

fn extract_document_parsed_inner(
    body: &str,
    parsed: Option<ParsedHtml>,
    page_url: &::url::Url,
    link_base_url: &::url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    if options.format == OutputFormat::Raw {
        bail!("OutputFormat::Raw must use raw byte passthrough, not extract_document");
    }
    if options.format == OutputFormat::Axtree {
        bail!("OutputFormat::Axtree requires CDP Accessibility.getFullAXTree");
    }

    let mime = extract_mime(content_type);
    if native_markdown::is_markdown_mime(mime.as_deref()) {
        return native_markdown::extract_native_markdown_with_bases(
            body,
            link_base_url,
            page_url,
            options,
        );
    }

    if feed::is_feed_document(mime.as_deref(), body) {
        return feed::extract_feed(body, page_url, mime.as_deref(), options);
    }

    if transcript::is_transcript_document(mime.as_deref(), body) {
        return transcript::extract_transcript(body, options);
    }

    if options.format != OutputFormat::Html {
        if let Some(doc) = youtube::extract_youtube_watch(body, page_url, options) {
            return Ok(doc);
        }
    }

    if is_passthrough_non_html(content_type) {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
            next_offset: None,
            warning: None,
            resources: Vec::new(),
            tools: Vec::new(),
            ui_gates: Vec::new(),
        });
    }

    let doc = parsed.unwrap_or_else(|| ParsedHtml::parse(body));
    let title = doc.title();
    let effective_base = doc.effective_base_url(page_url);
    let tools = doc.extract_webmcp_tools(page_url);

    let (raw_content, resources) = match options.format {
        OutputFormat::Html => (
            body.to_string(),
            doc.extract_resources(options.mode, &effective_base),
        ),
        OutputFormat::Text => doc.into_plain_text_and_resources(options.mode, &effective_base),
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
                        format!("# {t}")
                    } else {
                        format!("# {t}\n\n{trimmed_md}")
                    }
                }
                _ => trimmed_md.to_string(),
            };
            (raw_content, resources)
        }
        OutputFormat::Raw | OutputFormat::Axtree => unreachable!(),
    };
    let (content, truncated) = apply_format_limit(&raw_content, options.format, options.max_bytes);
    Ok(ExtractedDocument {
        title,
        content,
        truncated,
        next_offset: None,
        warning: None,
        resources,
        tools,
        ui_gates: Vec::new(),
    })
}

/// Finalize an extracted document by appending optional resource/tool manifests before running
/// `--focus`, `--section`, `--offset`, and `--max-bytes` projection or slicing.
pub fn finalize_extracted_document(
    doc: &mut ExtractedDocument,
    options: &ExtractOptions,
    append_resources_manifest: bool,
    append_tools_manifest: bool,
) {
    if append_resources_manifest && !doc.resources.is_empty() {
        let manifest = resources::format_resource_manifest_markdown(&doc.resources);
        if !manifest.trim().is_empty() {
            if !doc.content.trim().is_empty() {
                doc.content.push_str("\n\n");
            }
            doc.content.push_str(manifest.trim());
        }
    }
    if append_tools_manifest && !doc.tools.is_empty() {
        let manifest = webmcp::format_webmcp_manifest_markdown(&doc.tools);
        if !manifest.trim().is_empty() {
            if !doc.content.trim().is_empty() {
                doc.content.push_str("\n\n");
            }
            doc.content.push_str(manifest.trim());
        }
    }
    let has_slice_or_projection = options.max_bytes.is_some()
        || options.focus.is_some()
        || options.section.is_some()
        || options.offset > 0;
    if has_slice_or_projection {
        let proj = apply_projection_or_slice(&doc.content, options);
        doc.content = proj.content;
        doc.truncated |= proj.truncated;
        doc.next_offset = proj.next_offset;
        if let Some(w) = proj.warning {
            doc.warning = Some(match doc.warning.take() {
                Some(prev) => format!("{prev}; {w}"),
                None => w,
            });
        }
    }
}

/// Detect binary payloads by MIME type or leading magic/NUL bytes (for missing/octet-stream MIMEs,
/// or always sniffing magic headers when `sniff_magic_always` is true).
/// Returns `Some(detected_mime)` when the payload is binary and unsafe for text extraction.
pub fn detect_binary_content(content_type: Option<&str>, body: &[u8]) -> Option<Option<String>> {
    detect_binary_content_with_magic_sniff(content_type, body, false)
}

/// Detect binary payloads with explicit precedence:
/// 1) declared binary MIME, 2) magic headers (always when `sniff_magic_always` is true),
/// 3) UTF-16 BOM / declared UTF-16 charset exemption, 4) generic NUL-byte sniffing.
pub fn detect_binary_content_with_magic_sniff(
    content_type: Option<&str>,
    body: &[u8],
    sniff_magic_always: bool,
) -> Option<Option<String>> {
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

    let sniff_allowed =
        sniff_magic_always || mime.is_none() || mime.as_deref() == Some("application/octet-stream");
    if !sniff_allowed {
        return None;
    }

    if has_binary_magic_header(body) {
        return Some(if mime.as_deref() == Some("application/octet-stream") {
            mime
        } else if sniff_magic_always && mime.is_some() {
            None
        } else {
            mime
        });
    }

    if is_declared_or_bom_utf16(content_type, body) {
        return None;
    }

    let inspect = &body[..body.len().min(8192)];
    if inspect.contains(&0) {
        return Some(mime);
    }

    None
}

fn has_binary_magic_header(body: &[u8]) -> bool {
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
    MAGICS.iter().any(|magic| body.starts_with(magic))
}

fn is_declared_or_bom_utf16(content_type: Option<&str>, body: &[u8]) -> bool {
    if body.starts_with(&[0xFF, 0xFE]) || body.starts_with(&[0xFE, 0xFF]) {
        return true;
    }
    matches!(
        charset::extract_charset_from_content_type(content_type).as_deref(),
        Some("utf-16" | "utf-16le" | "utf-16be")
    )
}

pub(crate) fn apply_utf8_limit(text: &str, max_bytes: Option<usize>) -> (String, bool) {
    match max_bytes {
        Some(max) => truncate_utf8(text, max),
        None => (text.to_string(), false),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectionResult {
    pub content: String,
    pub truncated: bool,
    pub next_offset: Option<usize>,
    pub warning: Option<String>,
}

pub(crate) fn apply_projection_or_slice(
    content: &str,
    options: &ExtractOptions,
) -> ProjectionResult {
    if let Some(query) = options.focus.as_deref() {
        if matches!(options.format, OutputFormat::Markdown | OutputFormat::Text) {
            let (projected, truncated) = focus::focus_document(
                content,
                query,
                options.max_bytes.unwrap_or(focus::DEFAULT_FOCUS_MAX_BYTES),
                options.format == OutputFormat::Markdown,
            );
            return ProjectionResult {
                content: projected,
                truncated,
                next_offset: None,
                warning: None,
            };
        }
    }

    let mut working: std::borrow::Cow<'_, str> = std::borrow::Cow::Borrowed(content);
    if let Some(selector) = options.section.as_deref() {
        if options.format == OutputFormat::Markdown {
            let sec = focus::extract_section(working.as_ref(), selector);
            if !sec.matched {
                return ProjectionResult {
                    content: sec.content,
                    truncated: false,
                    next_offset: None,
                    warning: sec.warning,
                };
            }
            working = std::borrow::Cow::Owned(sec.content);
        }
    }

    let working_str = working.as_ref();
    let mut start = options.offset;
    if start >= working_str.len() {
        let warning = (start > 0 && !working_str.is_empty()).then(|| {
            format!(
                "offset {start} exceeds extracted content length ({} bytes)",
                working_str.len()
            )
        });
        return ProjectionResult {
            content: String::new(),
            truncated: false,
            next_offset: None,
            warning,
        };
    }
    while start < working_str.len() && !working_str.is_char_boundary(start) {
        start += 1;
    }
    if start >= working_str.len() {
        return ProjectionResult {
            content: String::new(),
            truncated: false,
            next_offset: None,
            warning: Some(format!(
                "offset {} exceeds extracted content length ({} bytes)",
                options.offset,
                working_str.len()
            )),
        };
    }

    let mut fence_prefix: Option<&str> = None;
    if start > 0 && options.format == OutputFormat::Markdown {
        if let Some((_, _, open_start, open_line_end)) =
            markdown::open_fence_at(&working_str[..start], start)
        {
            if open_line_end < working_str.len() {
                if start < open_line_end {
                    start = open_line_end;
                }
                fence_prefix = Some(&working_str[open_start..open_line_end]);
            }
        }
    }

    let rem = &working_str[start..];
    match options.max_bytes {
        None => {
            let out = match fence_prefix {
                Some(fp) => format!("{fp}{rem}"),
                None => rem.to_string(),
            };
            ProjectionResult {
                content: out,
                truncated: false,
                next_offset: None,
                warning: None,
            }
        }
        Some(max) if options.format == OutputFormat::Markdown => {
            if let Some(fp) = fence_prefix {
                if max > fp.len() + 1 {
                    let prefixed = format!("{fp}{rem}");
                    let (out, trunc, consumed_prefixed) =
                        markdown::truncate_markdown_with_offset(&prefixed, max);
                    let consumed_orig = consumed_prefixed.saturating_sub(fp.len());
                    let next_offset =
                        (trunc && consumed_orig > 0 && start + consumed_orig < working_str.len())
                            .then_some(start + consumed_orig);
                    return ProjectionResult {
                        content: out,
                        truncated: trunc,
                        next_offset,
                        warning: None,
                    };
                }
            }
            let (out, trunc, consumed) = markdown::truncate_markdown_with_offset(rem, max);
            let next_offset = (trunc && consumed > 0 && start + consumed < working_str.len())
                .then_some(start + consumed);
            ProjectionResult {
                content: out,
                truncated: trunc,
                next_offset,
                warning: None,
            }
        }
        Some(max) => {
            let (out, trunc, consumed) = truncate_utf8_with_offset(rem, max);
            let next_offset = (trunc && consumed > 0 && start + consumed < working_str.len())
                .then_some(start + consumed);
            ProjectionResult {
                content: out,
                truncated: trunc,
                next_offset,
                warning: None,
            }
        }
    }
}

pub(crate) fn apply_format_limit(
    content: &str,
    format: OutputFormat,
    max_bytes: Option<usize>,
) -> (String, bool) {
    match max_bytes {
        Some(max) if format == OutputFormat::Markdown => markdown::truncate_markdown(content, max),
        Some(max) => truncate_utf8(content, max),
        None => (content.to_string(), false),
    }
}

pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    let (out, truncated, _) = truncate_utf8_with_offset(text, max_bytes);
    (out, truncated)
}

pub(crate) fn truncate_utf8_with_offset(text: &str, max_bytes: usize) -> (String, bool, usize) {
    if text.len() <= max_bytes {
        return (text.to_string(), false, text.len());
    }
    if max_bytes == 0 {
        return (String::new(), !text.is_empty(), 0);
    }

    if max_bytes <= TRUNCATED_SUFFIX.len() {
        let end = floor_char_boundary(text, max_bytes);
        return (
            text[..end].to_string(),
            true,
            markdown::advance_past_newline(text, end),
        );
    }

    let limit = max_bytes - TRUNCATED_SUFFIX.len();
    let end = choose_boundary_cut(text, limit);
    let mut out = String::with_capacity(end + TRUNCATED_SUFFIX.len());
    out.push_str(&text[..end]);
    out.push_str(TRUNCATED_SUFFIX);
    (out, true, markdown::advance_past_newline(text, end))
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

fn heading_matches_title(heading: &str, title: &str) -> bool {
    let title_trimmed = title.trim();
    if heading.eq_ignore_ascii_case(title_trimmed) {
        return true;
    }
    for sep in [" | ", " - ", " — ", " – ", " :: ", " · ", " • "] {
        if title_trimmed.contains(sep)
            && title_trimmed
                .split(sep)
                .map(str::trim)
                .any(|seg| !seg.is_empty() && heading.eq_ignore_ascii_case(seg))
        {
            return true;
        }
    }
    false
}

pub(crate) fn starts_with_title_heading(md: &str, title: &str) -> bool {
    let mut fence = markdown::FenceTracker::default();
    let mut checked = 0usize;
    for raw_line in md.lines() {
        if !matches!(fence.step(raw_line), markdown::FenceLine::Outside) {
            return false;
        }
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((level, _, heading)) = markdown::split_atx_heading(trimmed) {
            return level == 1 && heading_matches_title(heading, title);
        }
        checked += 1;
        if checked >= 6 || trimmed.len() > 200 {
            return false;
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
