use ego_tree::NodeRef;
use scraper::{Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;
use url::Url;

use super::dom::{
    collapse_inline_whitespace, collect_raw_text, parse_html_dimension, resolve_img_src,
    resolve_picture_image,
};
use super::url::{resolve_and_clean_url, ResolvedUrl};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Diagram,
    Image,
    Transcript,
    Document,
    Dataset,
    Schema,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Diagram => "diagram",
            Self::Image => "image",
            Self::Transcript => "transcript",
            Self::Document => "document",
            Self::Dataset => "dataset",
            Self::Schema => "schema",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceSalience {
    High,
    Medium,
}

impl ResourceSalience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDescriptor {
    pub identifier: String,
    pub kind: ResourceKind,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<(u32, u32)>,
    pub salience: ResourceSalience,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

const IMAGE_NOISE_URL_PATTERNS: &[&str] = &[
    "shields.io",
    "badge",
    "avatar",
    "gravatar",
    "emoji",
    "1x1",
    "pixel",
];

const TECHNICAL_VISUAL_CUES: &[&str] = &[
    "diagram",
    "arch",
    "flow",
    "state",
    "chart",
    "plot",
    "graph",
    "bench",
    "schema",
    "topology",
    "figure",
    "pipeline",
    "sequence",
    "flamegraph",
];

static HEAD_META_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("head meta[name][content]").unwrap());

pub(crate) fn index_resources_in_subtree(
    doc: &Html,
    root: NodeRef<'_, Node>,
    base_url: &Url,
) -> Vec<ResourceDescriptor> {
    let mut ctx = ResourceIndexContext {
        base_url,
        current_heading: None,
        seen_urls: HashSet::new(),
        resources: Vec::new(),
    };
    walk_for_resources(root, false, None, &mut ctx);
    index_head_citation_pdf(doc, &mut ctx);
    ctx.resources
}

fn index_head_citation_pdf(doc: &Html, ctx: &mut ResourceIndexContext<'_>) {
    let (mut pdf_url, mut citation_title) = (None, None);
    for el in doc.select(&HEAD_META_SEL) {
        let (Some(name), Some(content)) = (
            el.value().attr("name").map(str::trim),
            el.value()
                .attr("content")
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        if pdf_url.is_none() && name.eq_ignore_ascii_case("citation_pdf_url") {
            pdf_url = Some(content);
        } else if citation_title.is_none() && name.eq_ignore_ascii_case("citation_title") {
            citation_title = clean_optional_text(content);
        }
    }
    let Some(resolved_url) = pdf_url.and_then(|raw| resolve_http_url(raw, ctx.base_url)) else {
        return;
    };
    ctx.current_heading = None;
    ctx.push(
        ResourceKind::Document,
        resolved_url.to_string(),
        Some("application/pdf".to_string()),
        citation_title.or_else(|| Some("PDF".to_string())),
        None,
        ResourceSalience::High,
    );
}

struct ResourceIndexContext<'a> {
    base_url: &'a Url,
    current_heading: Option<String>,
    seen_urls: HashSet<String>,
    resources: Vec<ResourceDescriptor>,
}

impl ResourceIndexContext<'_> {
    fn push(
        &mut self,
        kind: ResourceKind,
        url: String,
        mime_hint: Option<String>,
        label: Option<String>,
        dimensions: Option<(u32, u32)>,
        salience: ResourceSalience,
    ) {
        if !self.seen_urls.insert(url.clone()) {
            return;
        }
        let identifier = format!("r{}", self.resources.len() + 1);
        self.resources.push(ResourceDescriptor {
            identifier,
            kind,
            url,
            mime_hint,
            section_heading: self.current_heading.clone(),
            label,
            dimensions,
            salience,
            staged_path: None,
            byte_count: None,
            content_type: None,
        });
    }
}

fn walk_for_resources(
    node: NodeRef<'_, Node>,
    in_figure: bool,
    figure_caption: Option<&str>,
    ctx: &mut ResourceIndexContext<'_>,
) {
    let Node::Element(el) = node.value() else {
        for child in node.children() {
            walk_for_resources(child, in_figure, figure_caption, ctx);
        }
        return;
    };

    let tag = el.name();
    if matches!(tag, "script" | "style" | "noscript" | "template") {
        return;
    }

    if matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        if let Some(heading) = extract_node_clean_text(node) {
            ctx.current_heading = Some(heading);
        }
    }

    if tag == "figure" {
        let caption = find_figcaption_text(node);
        for child in node.children() {
            walk_for_resources(child, true, caption.as_deref(), ctx);
        }
        return;
    }

    if tag == "picture" {
        if let Some((src, alt, title)) = resolve_picture_image(node) {
            let img_el = node.children().find_map(|child| match child.value() {
                Node::Element(e) if e.name() == "img" => Some(e),
                _ => None,
            });
            maybe_index_image(
                &src,
                alt.as_deref(),
                title.as_deref(),
                img_el,
                in_figure,
                figure_caption,
                ctx,
            );
        }
        return;
    }

    if tag == "img" {
        if let Some(src) = resolve_img_src(el) {
            maybe_index_image(
                &src,
                el.attr("alt"),
                el.attr("title"),
                Some(el),
                in_figure,
                figure_caption,
                ctx,
            );
        }
        return;
    }

    if tag == "track" {
        maybe_index_track(el, ctx);
        return;
    }

    if tag == "a" {
        maybe_index_anchor(node, el, ctx);
    }

    for child in node.children() {
        walk_for_resources(child, in_figure, figure_caption, ctx);
    }
}

fn maybe_index_image(
    raw_src: &str,
    alt: Option<&str>,
    title: Option<&str>,
    el: Option<&scraper::node::Element>,
    in_figure: bool,
    figure_caption: Option<&str>,
    ctx: &mut ResourceIndexContext<'_>,
) {
    if let Some(img_el) = el {
        if img_el.attr("role").is_some_and(|r| {
            let t = r.trim();
            t.eq_ignore_ascii_case("presentation") || t.eq_ignore_ascii_case("none")
        }) {
            return;
        }
        if img_el
            .attr("aria-hidden")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        {
            return;
        }
        if has_image_noise_class(img_el) {
            return;
        }
    }

    let width = el
        .and_then(|e| e.attr("width"))
        .and_then(parse_html_dimension);
    let height = el
        .and_then(|e| e.attr("height"))
        .and_then(parse_html_dimension);

    if width.is_some_and(|w| w < 100) || height.is_some_and(|h| h < 80) {
        return;
    }

    let Some(resolved_url) = resolve_http_url(raw_src, ctx.base_url) else {
        return;
    };

    if is_image_noise_url(&resolved_url) {
        return;
    }

    let alt_clean = alt.and_then(clean_optional_text);
    let title_clean = title.and_then(clean_optional_text);
    let label = if in_figure {
        figure_caption
            .and_then(clean_optional_text)
            .or_else(|| alt_clean.clone())
            .or(title_clean)
    } else {
        alt_clean.clone().or(title_clean)
    };

    let large_dims = width.is_some_and(|w| w >= 300) && height.is_some_and(|h| h >= 150);
    let is_diagram =
        in_figure || large_dims || has_technical_visual_cue(label.as_deref(), resolved_url.path());

    let (kind, salience) = if is_diagram {
        (ResourceKind::Diagram, ResourceSalience::High)
    } else if alt_clean.as_ref().is_some_and(|a| a.len() >= 16) {
        (ResourceKind::Image, ResourceSalience::Medium)
    } else {
        return;
    };

    let dimensions = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    };
    let mime_hint = infer_image_mime(resolved_url.path());

    ctx.push(
        kind,
        resolved_url.to_string(),
        mime_hint,
        label,
        dimensions,
        salience,
    );
}

fn maybe_index_track(el: &scraper::node::Element, ctx: &mut ResourceIndexContext<'_>) {
    let Some(raw_src) = el.attr("src").map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let Some(resolved_url) = resolve_http_url(raw_src, ctx.base_url) else {
        return;
    };
    let path_lower = resolved_url.path().to_ascii_lowercase();
    let is_caption_ext = path_lower.ends_with(".vtt") || path_lower.ends_with(".srt");

    let kind_ok = match el.attr("kind").map(str::trim) {
        None | Some("") => true,
        Some(k) => {
            k.eq_ignore_ascii_case("captions")
                || k.eq_ignore_ascii_case("subtitles")
                || k.eq_ignore_ascii_case("descriptions")
        }
    };

    if !kind_ok && !is_caption_ext {
        return;
    }

    let label = el
        .attr("label")
        .and_then(clean_optional_text)
        .or_else(|| el.attr("srclang").and_then(clean_optional_text));

    let mime_hint = if path_lower.ends_with(".srt") {
        Some("application/x-subrip".to_string())
    } else {
        Some("text/vtt".to_string())
    };

    ctx.push(
        ResourceKind::Transcript,
        resolved_url.to_string(),
        mime_hint,
        label,
        None,
        ResourceSalience::High,
    );
}

fn maybe_index_anchor(
    node: NodeRef<'_, Node>,
    el: &scraper::node::Element,
    ctx: &mut ResourceIndexContext<'_>,
) {
    let Some(raw_href) = el.attr("href").map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    if raw_href.starts_with('#') {
        return;
    }
    let Some(resolved_url) = resolve_http_url(raw_href, ctx.base_url) else {
        return;
    };
    let Some((kind, mime_hint)) = classify_linked_artifact_url(&resolved_url) else {
        return;
    };

    let label =
        extract_node_clean_text(node).or_else(|| el.attr("title").and_then(clean_optional_text));

    ctx.push(
        kind,
        resolved_url.to_string(),
        Some(mime_hint.to_string()),
        label,
        None,
        ResourceSalience::High,
    );
}

fn classify_linked_artifact_url(url: &Url) -> Option<(ResourceKind, &'static str)> {
    let path_lower = url.path().to_ascii_lowercase();
    let filename = path_lower.rsplit('/').next().unwrap_or("");
    if matches!(filename, "openapi.yaml" | "openapi.yml") {
        return Some((ResourceKind::Schema, "application/yaml"));
    }
    if matches!(filename, "openapi.json" | "swagger.json") || path_lower.ends_with(".schema.json") {
        return Some((ResourceKind::Schema, "application/json"));
    }
    if matches!(filename, "llms.txt" | "llms-full.txt") {
        return Some((ResourceKind::Schema, "text/plain"));
    }
    match filename.rsplit_once('.')?.1 {
        "pdf" => Some((ResourceKind::Document, "application/pdf")),
        "csv" => Some((ResourceKind::Dataset, "text/csv")),
        "tsv" => Some((ResourceKind::Dataset, "text/tab-separated-values")),
        "parquet" => Some((ResourceKind::Dataset, "application/vnd.apache.parquet")),
        "jsonl" => Some((ResourceKind::Dataset, "application/x-ndjson")),
        "proto" => Some((ResourceKind::Schema, "text/plain")),
        "sql" => Some((ResourceKind::Schema, "application/sql")),
        _ => None,
    }
}

fn resolve_http_url(raw: &str, base_url: &Url) -> Option<Url> {
    let ResolvedUrl::Resolved(cleaned) = resolve_and_clean_url(raw, base_url) else {
        return None;
    };
    let parsed = Url::parse(&cleaned).ok()?;
    matches!(parsed.scheme(), "http" | "https").then_some(parsed)
}

fn is_image_noise_url(url: &Url) -> bool {
    let lower = url.as_str().to_ascii_lowercase();
    if IMAGE_NOISE_URL_PATTERNS.iter().any(|p| lower.contains(p)) {
        return true;
    }
    url.path().to_ascii_lowercase().ends_with(".ico")
}

fn has_image_noise_class(el: &scraper::node::Element) -> bool {
    let check = |val: &str| {
        val.split_whitespace().any(|raw| {
            let tok = raw.to_ascii_lowercase();
            tok.contains("badge")
                || tok.contains("avatar")
                || tok.contains("gravatar")
                || tok.contains("emoji")
                || tok.contains("pixel")
                || tok.contains("tracker")
                || tok == "icon"
                || tok.starts_with("icon-")
                || tok.ends_with("-icon")
        })
    };
    el.attr("class").is_some_and(check) || el.attr("id").is_some_and(check)
}

fn has_technical_visual_cue(label: Option<&str>, path: &str) -> bool {
    let check = |text: &str| {
        let lower = text.to_ascii_lowercase();
        if TECHNICAL_VISUAL_CUES.iter().any(|cue| lower.contains(cue)) {
            return true;
        }
        lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|tok| {
                tok == "fig"
                    || tok.starts_with("fig") && tok[3..].chars().all(|c| c.is_ascii_digit())
            })
    };
    label.is_some_and(check) || check(path)
}

fn infer_image_mime(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('/').next()?.rsplit_once('.')?.1;
    let mime = match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        _ => return None,
    };
    Some(mime.to_string())
}

fn find_figcaption_text(figure: NodeRef<'_, Node>) -> Option<String> {
    for desc in figure.descendants() {
        if let Node::Element(el) = desc.value() {
            if el.name() == "figcaption" {
                if let Some(text) = extract_node_clean_text(desc) {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn extract_node_clean_text(node: NodeRef<'_, Node>) -> Option<String> {
    let mut raw = String::new();
    collect_raw_text(node, &mut raw);
    clean_optional_text(&raw)
}

fn clean_optional_text(raw: &str) -> Option<String> {
    let collapsed = collapse_inline_whitespace(raw);
    let trimmed = collapsed.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn format_resource_manifest_markdown(resources: &[ResourceDescriptor]) -> String {
    if resources.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "## Linked Resources\n\n\
         | ID | Kind | Salience | Section | Dimensions | Label | URL |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n",
    );

    for res in resources {
        let section = res
            .section_heading
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_else(|| "-".to_string());
        let dims = res
            .dimensions
            .map(|(w, h)| format!("{w}x{h}"))
            .unwrap_or_else(|| "-".to_string());
        let label = res
            .label
            .as_deref()
            .map(escape_table_cell)
            .unwrap_or_else(|| "-".to_string());
        let url = escape_table_cell(&res.url);

        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            escape_table_cell(&res.identifier),
            res.kind.as_str(),
            res.salience.as_str(),
            section,
            dims,
            label,
            url
        ));
    }

    out.trim_end_matches('\n').to_string()
}

pub(crate) fn escape_table_cell(raw: &str) -> String {
    collapse_inline_whitespace(raw).trim().replace('|', "\\|")
}
