pub mod dom;
pub mod markdown;

use anyhow::{bail, Result};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    #[default]
    Markdown,
    Html,
    Text,
    Raw,
}

impl FromStr for OutputFormat {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "markdown" | "md" => Ok(Self::Markdown),
            "html" => Ok(Self::Html),
            "text" | "txt" => Ok(Self::Text),
            "raw" => Ok(Self::Raw),
            other => bail!(
                "invalid output format {:?} (expected markdown, html, text, or raw)",
                other
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtractMode {
    #[default]
    Clean,
    Article,
    Full,
}

impl FromStr for ExtractMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "clean" => Ok(Self::Clean),
            "article" => Ok(Self::Article),
            "full" => Ok(Self::Full),
            other => bail!(
                "invalid extract mode {:?} (expected clean, article, or full)",
                other
            ),
        }
    }
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
}

pub fn extract_document(
    body: &str,
    _url: &url::Url,
    content_type: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    if options.format == OutputFormat::Raw {
        bail!("OutputFormat::Raw must use raw byte passthrough, not extract_document");
    }

    if is_passthrough_non_html(content_type) {
        let (content, truncated) = match options.max_bytes {
            Some(max) => truncate_utf8(body, max),
            None => (body.to_string(), false),
        };
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
        });
    }

    let title = dom::extract_title(body);

    let raw_content = match options.format {
        OutputFormat::Html => body.to_string(),
        OutputFormat::Text => dom::extract_plain_text(body, options.mode),
        OutputFormat::Markdown => {
            let pruned = dom::prune_html_for_markdown(body, options.mode);
            let md = markdown::html_to_compact_markdown(
                &pruned,
                options.include_images,
                options.include_links,
            )?;
            let trimmed_md = md.trim();
            match &title {
                Some(t) if !starts_with_title_heading(trimmed_md, t) => {
                    if trimmed_md.is_empty() {
                        format!("# {}", t)
                    } else {
                        format!("# {}\n\n{}", t, trimmed_md)
                    }
                }
                _ => trimmed_md.to_string(),
            }
        }
        OutputFormat::Raw => unreachable!(),
    };

    let (content, truncated) = match options.max_bytes {
        Some(max) => truncate_utf8(&raw_content, max),
        None => (raw_content, false),
    };

    Ok(ExtractedDocument {
        title,
        content,
        truncated,
    })
}

pub fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), !text.is_empty());
    }

    const SUFFIX: &str = "\n\n[... truncated]";
    if max_bytes <= SUFFIX.len() {
        let end = floor_char_boundary(text, max_bytes);
        return (text[..end].to_string(), true);
    }

    let end = floor_char_boundary(text, max_bytes - SUFFIX.len());
    let mut out = String::with_capacity(end + SUFFIX.len());
    out.push_str(&text[..end]);
    out.push_str(SUFFIX);
    (out, true)
}

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn starts_with_title_heading(md: &str, title: &str) -> bool {
    let first_line = md.lines().next().unwrap_or("").trim();
    first_line
        .strip_prefix("# ")
        .map(|h| h.trim().eq_ignore_ascii_case(title.trim()))
        .unwrap_or(false)
}

fn is_passthrough_non_html(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else {
        return false;
    };
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mime.is_empty() || mime == "text/html" || mime == "application/xhtml+xml" {
        return false;
    }
    mime == "application/json"
        || mime == "text/plain"
        || mime == "text/markdown"
        || mime == "text/x-markdown"
        || mime == "application/xml"
        || mime == "text/xml"
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}
