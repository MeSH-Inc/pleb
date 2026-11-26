use anyhow::Result;
use url::Url;

use super::dom::ParsedHtml;
use super::markdown::{
    compact_markdown, format_destination, html_to_compact_markdown, truncate_markdown,
};
use super::url::{resolve_and_clean_url, ResolvedUrl};
use super::{apply_utf8_limit, ExtractMode, ExtractOptions, ExtractedDocument, OutputFormat};

const MAX_FEED_ITEMS: usize = 50;
const MAX_XML_DEPTH: usize = 64;

pub fn is_feed_document(mime: Option<&str>, body: &str) -> bool {
    match mime {
        Some(
            "application/rss+xml"
            | "application/atom+xml"
            | "application/rdf+xml"
            | "application/feed+json",
        ) => true,
        Some("application/xml" | "text/xml") => detect_xml_feed_root(body).is_some(),
        _ => false,
    }
}

pub fn extract_feed(
    body: &str,
    page_url: &Url,
    mime: Option<&str>,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    if options.mode == ExtractMode::Full || options.format == OutputFormat::Html {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
        });
    }

    let parsed = if mime == Some("application/feed+json") {
        parse_json_feed(body)
    } else {
        parse_xml_feed(body)
    };

    let Some(feed) = parsed else {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
        });
    };

    let title = feed.title.clone();
    match options.format {
        OutputFormat::Markdown => {
            let rendered = render_feed_markdown(&feed, page_url, options)?;
            let compacted = compact_markdown(&rendered);
            let (content, truncated) = match options.max_bytes {
                Some(max) => truncate_markdown(&compacted, max),
                None => (compacted, false),
            };
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
            })
        }
        OutputFormat::Text => {
            let rendered = render_feed_text(&feed, page_url, options);
            let (content, truncated) = apply_utf8_limit(&rendered, options.max_bytes);
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
            })
        }
        OutputFormat::Html | OutputFormat::Raw => unreachable!(),
    }
}

struct ParsedFeed {
    title: Option<String>,
    description: Option<String>,
    items: Vec<FeedItem>,
}

struct FeedItem {
    title: String,
    link: Option<String>,
    published: Option<String>,
    summary: Option<FeedBody>,
}

enum FeedBody {
    Html(String),
    Plain(String),
}

fn render_feed_markdown(
    feed: &ParsedFeed,
    page_url: &Url,
    options: &ExtractOptions,
) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(title) = &feed.title {
        parts.push(format!("# {title}"));
    }
    if let Some(desc) = &feed.description {
        let cleaned = clean_fragment_markdown(desc, true, page_url, options)?;
        if !cleaned.is_empty() && feed.title.as_deref() != Some(cleaned.as_str()) {
            parts.push(cleaned);
        }
    }

    for item in feed.items.iter().take(MAX_FEED_ITEMS) {
        let mut section = String::new();
        let resolved_link =
            item.link
                .as_deref()
                .and_then(|raw| match resolve_and_clean_url(raw, page_url) {
                    ResolvedUrl::Resolved(u) => Some(u),
                    ResolvedUrl::Drop | ResolvedUrl::UnwrapText => None,
                });

        if options.include_links {
            if let Some(url) = resolved_link {
                section.push_str(&format!(
                    "## [{}]({})",
                    item.title,
                    format_destination(&url)
                ));
            } else {
                section.push_str(&format!("## {}", item.title));
            }
        } else {
            section.push_str(&format!("## {}", item.title));
        }

        if let Some(published) = &item.published {
            section.push_str(&format!("\n*{published}*"));
        }

        if let Some(body) = &item.summary {
            let rendered_body = match body {
                FeedBody::Html(html) => clean_fragment_markdown(html, true, page_url, options)?,
                FeedBody::Plain(text) => clean_fragment_markdown(text, false, page_url, options)?,
            };
            if !rendered_body.is_empty() {
                section.push_str("\n\n");
                section.push_str(&rendered_body);
            }
        }
        parts.push(section);
    }

    Ok(parts.join("\n\n"))
}

fn render_feed_text(feed: &ParsedFeed, page_url: &Url, options: &ExtractOptions) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(title) = &feed.title {
        parts.push(title.clone());
    }
    if let Some(desc) = &feed.description {
        let cleaned = clean_fragment_text(desc, true);
        if !cleaned.is_empty() && feed.title.as_deref() != Some(cleaned.as_str()) {
            parts.push(cleaned);
        }
    }

    for item in feed.items.iter().take(MAX_FEED_ITEMS) {
        let mut lines: Vec<String> = Vec::new();
        lines.push(item.title.clone());
        if options.include_links {
            if let Some(raw) = item.link.as_deref() {
                if let ResolvedUrl::Resolved(u) = resolve_and_clean_url(raw, page_url) {
                    lines.push(u);
                }
            }
        }
        if let Some(published) = &item.published {
            lines.push(published.clone());
        }
        if let Some(body) = &item.summary {
            let text = match body {
                FeedBody::Html(html) => clean_fragment_text(html, true),
                FeedBody::Plain(plain) => clean_fragment_text(plain, false),
            };
            if !text.is_empty() {
                lines.push(text);
            }
        }
        parts.push(lines.join("\n"));
    }

    parts.join("\n\n")
}

fn clean_fragment_markdown(
    fragment: &str,
    may_be_html: bool,
    page_url: &Url,
    options: &ExtractOptions,
) -> Result<String> {
    let trimmed = fragment.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if may_be_html && looks_like_html_fragment(trimmed) {
        let wrapped = format!("<html><body>{trimmed}</body></html>");
        let pruned = ParsedHtml::parse(&wrapped).into_pruned_html(ExtractMode::Clean);
        let md = html_to_compact_markdown(
            &pruned,
            page_url,
            options.include_images,
            options.include_links,
        )?;
        Ok(md.trim().to_string())
    } else {
        Ok(compact_markdown(trimmed))
    }
}

fn clean_fragment_text(fragment: &str, may_be_html: bool) -> String {
    let trimmed = fragment.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if may_be_html && looks_like_html_fragment(trimmed) {
        let wrapped = format!("<html><body>{trimmed}</body></html>");
        ParsedHtml::parse(&wrapped)
            .into_plain_text(ExtractMode::Clean)
            .trim()
            .to_string()
    } else {
        trimmed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn looks_like_html_fragment(s: &str) -> bool {
    let Some(lt) = s.find('<') else {
        return false;
    };
    let rest = &s[lt + 1..];
    rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/') && rest.contains('>')
}

fn parse_json_feed(body: &str) -> Option<ParsedFeed> {
    let val: serde_json::Value = serde_json::from_str(body).ok()?;
    let obj = val.as_object()?;
    let title = obj
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(collapse_inline)
        .filter(|s| !s.is_empty());
    let description = obj
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut items = Vec::new();
    if let Some(arr) = obj.get("items").and_then(serde_json::Value::as_array) {
        for entry in arr.iter().take(MAX_FEED_ITEMS) {
            let Some(item_obj) = entry.as_object() else {
                continue;
            };
            let item_title = item_obj
                .get("title")
                .and_then(serde_json::Value::as_str)
                .map(collapse_inline)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Untitled".to_string());
            let link = item_obj
                .get("url")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    item_obj
                        .get("external_url")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let published = item_obj
                .get("date_published")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    item_obj
                        .get("date_modified")
                        .and_then(serde_json::Value::as_str)
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let summary = item_obj
                .get("content_html")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| FeedBody::Html(s.to_string()))
                .or_else(|| {
                    item_obj
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .or_else(|| {
                            item_obj
                                .get("content_text")
                                .and_then(serde_json::Value::as_str)
                        })
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(|s| FeedBody::Plain(s.to_string()))
                });

            items.push(FeedItem {
                title: item_title,
                link,
                published,
                summary,
            });
        }
    }

    Some(ParsedFeed {
        title,
        description,
        items,
    })
}

fn detect_xml_feed_root(body: &str) -> Option<&'static str> {
    let mut s = body.trim_start_matches('\u{FEFF}');
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("<?") {
            let end = rest.find("?>")?;
            s = &rest[end + 2..];
        } else if let Some(rest) = s.strip_prefix("<!--") {
            let end = rest.find("-->")?;
            s = &rest[end + 3..];
        } else if s.len() >= 9 && s[..9].eq_ignore_ascii_case("<!doctype") {
            let end = s.find('>')?;
            s = &s[end + 1..];
        } else {
            break;
        }
    }

    let rest = s.strip_prefix('<')?;
    let name_end = rest
        .find(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
        .unwrap_or(rest.len());
    let root = rest[..name_end].to_ascii_lowercase();
    match root.as_str() {
        "rss" => Some("rss"),
        "feed" => Some("feed"),
        "rdf:rdf" => Some("rdf:rdf"),
        _ => None,
    }
}

fn parse_xml_feed(body: &str) -> Option<ParsedFeed> {
    let root = parse_xml_root(body)?;
    let root_name = root.name.to_ascii_lowercase();

    if root_name == "rss" {
        let channel = root.child("channel")?;
        return Some(parse_rss_channel(channel, channel));
    }

    if root_name == "rdf:rdf" {
        let channel = root.child("channel").unwrap_or(&root);
        return Some(parse_rss_channel(channel, &root));
    }

    if root_name == "feed" {
        let title = root
            .child_text("title")
            .map(collapse_inline)
            .filter(|s| !s.is_empty());
        let description = root
            .child_text("subtitle")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let mut items = Vec::new();
        for entry in root.children_named("entry").take(MAX_FEED_ITEMS) {
            let item_title = entry
                .child_text("title")
                .map(collapse_inline)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Untitled".to_string());
            let link = extract_atom_link(entry);
            let published = entry
                .child_text("published")
                .or_else(|| entry.child_text("updated"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let summary = entry
                .child("content")
                .or_else(|| entry.child("summary"))
                .and_then(|el| {
                    let raw = el.text.trim();
                    if raw.is_empty() {
                        return None;
                    }
                    let is_plain = el
                        .attr("type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("text"));
                    Some(if is_plain {
                        FeedBody::Plain(raw.to_string())
                    } else {
                        FeedBody::Html(raw.to_string())
                    })
                });

            items.push(FeedItem {
                title: item_title,
                link,
                published,
                summary,
            });
        }

        return Some(ParsedFeed {
            title,
            description,
            items,
        });
    }

    None
}

fn parse_rss_channel(channel: &XmlElement, item_container: &XmlElement) -> ParsedFeed {
    let title = channel
        .child_text("title")
        .map(collapse_inline)
        .filter(|s| !s.is_empty());
    let description = channel
        .child_text("description")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut items = Vec::new();
    for item in item_container.children_named("item").take(MAX_FEED_ITEMS) {
        let item_title = item
            .child_text("title")
            .map(collapse_inline)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Untitled".to_string());
        let link = item
            .child_text("link")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                item.child("guid").and_then(|g| {
                    let is_perm = g
                        .attr("isPermaLink")
                        .map(|v| !v.eq_ignore_ascii_case("false"))
                        .unwrap_or(true);
                    let txt = g.text.trim();
                    (is_perm && (txt.starts_with("http://") || txt.starts_with("https://")))
                        .then(|| txt.to_string())
                })
            });
        let published = item
            .child_text("pubdate")
            .or_else(|| item.child_text("dc:date"))
            .or_else(|| item.child_text("published"))
            .or_else(|| item.child_text("updated"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let summary = item
            .child_text("content:encoded")
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                item.child_text("description")
                    .filter(|s| !s.trim().is_empty())
            })
            .map(|s| FeedBody::Html(s.trim().to_string()));

        items.push(FeedItem {
            title: item_title,
            link,
            published,
            summary,
        });
    }

    ParsedFeed {
        title,
        description,
        items,
    }
}

fn extract_atom_link(entry: &XmlElement) -> Option<String> {
    let mut fallback = None;
    for link in entry.children_named("link") {
        let Some(href) = link.attr("href").map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let rel = link.attr("rel").map(str::trim);
        if rel.is_none() || rel.is_some_and(|r| r.eq_ignore_ascii_case("alternate")) {
            return Some(href.to_string());
        }
        if fallback.is_none() {
            fallback = Some(href.to_string());
        }
    }
    fallback
}

struct XmlElement {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<XmlElement>,
}

impl XmlElement {
    fn child(&self, name: &str) -> Option<&XmlElement> {
        self.children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElement> {
        self.children
            .iter()
            .filter(move |c| c.name.eq_ignore_ascii_case(name))
    }

    fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn parse_xml_root(input: &str) -> Option<XmlElement> {
    let mut pos = 0usize;
    let s = input.trim_start_matches('\u{FEFF}');
    let bytes = s.as_bytes();
    let mut stack: Vec<XmlElement> = Vec::new();
    let mut root: Option<XmlElement> = None;

    while pos < bytes.len() {
        if bytes[pos] == b'<' {
            let rest = &s[pos..];
            if rest.starts_with("<!--") {
                if let Some(end) = rest.find("-->") {
                    pos += end + 3;
                    continue;
                }
                break;
            }
            if rest.starts_with("<![CDATA[") {
                let content_start = pos + 9;
                if let Some(end) = s[content_start..].find("]]>") {
                    if let Some(top) = stack.last_mut() {
                        top.text.push_str(&s[content_start..content_start + end]);
                    }
                    pos = content_start + end + 3;
                    continue;
                }
                break;
            }
            if rest.starts_with("<?") {
                if let Some(end) = rest.find("?>") {
                    pos += end + 2;
                    continue;
                }
                break;
            }
            if rest.starts_with("<!") {
                if let Some(end) = rest.find('>') {
                    pos += end + 1;
                    continue;
                }
                break;
            }
            if rest.starts_with("</") {
                let Some(end) = rest.find('>') else {
                    break;
                };
                let close_name = rest[2..end].trim();
                pos += end + 1;

                if let Some(idx) = stack
                    .iter()
                    .rposition(|el| el.name.eq_ignore_ascii_case(close_name))
                {
                    while stack.len() > idx {
                        let finished = stack.pop().unwrap();
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(finished);
                        } else {
                            root = Some(finished);
                        }
                    }
                    if root.is_some() {
                        break;
                    }
                }
                continue;
            }

            // Opening or self-closing tag.
            let Some(tag_end_rel) = find_tag_end(rest) else {
                break;
            };
            let tag_inner = &rest[1..tag_end_rel];
            pos += tag_end_rel + 1;

            let (tag_body, self_closing) = match tag_inner.strip_suffix('/') {
                Some(stripped) => (stripped.trim_end(), true),
                None => (tag_inner, false),
            };
            let Some((name, attrs)) = parse_tag_name_and_attrs(tag_body) else {
                continue;
            };
            let el = XmlElement {
                name,
                attrs,
                text: String::new(),
                children: Vec::new(),
            };
            if self_closing {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                } else {
                    root = Some(el);
                    break;
                }
            } else if stack.len() < MAX_XML_DEPTH {
                stack.push(el);
            }
        } else {
            let next_lt = s[pos..]
                .find('<')
                .map(|rel| pos + rel)
                .unwrap_or(bytes.len());
            if let Some(top) = stack.last_mut() {
                let decoded = decode_xml_entities(&s[pos..next_lt]);
                top.text.push_str(&decoded);
            }
            pos = next_lt;
        }
    }

    if root.is_none() {
        while let Some(finished) = stack.pop() {
            if let Some(parent) = stack.last_mut() {
                parent.children.push(finished);
            } else {
                root = Some(finished);
            }
        }
    }

    root
}

fn find_tag_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut quote: Option<u8> = None;
    for (idx, &b) in bytes.iter().enumerate().skip(1) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(idx),
            None => {}
        }
    }
    None
}

fn parse_tag_name_and_attrs(tag_body: &str) -> Option<(String, Vec<(String, String)>)> {
    let trimmed = tag_body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let name_end = trimmed
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(trimmed.len());
    let name = trimmed[..name_end].to_string();
    let mut attrs = Vec::new();
    let mut rest = trimmed[name_end..].trim_start();

    while !rest.is_empty() {
        let key_end = rest
            .find(|c: char| c == '=' || c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let key = &rest[..key_end];
        if key.is_empty() {
            break;
        }
        rest = rest[key_end..].trim_start();
        let Some(after_eq) = rest.strip_prefix('=') else {
            continue;
        };
        rest = after_eq.trim_start();
        if rest.is_empty() {
            break;
        }
        let first = rest.as_bytes()[0];
        let (raw_val, consumed) = if first == b'"' || first == b'\'' {
            let quote = first as char;
            if let Some(end) = rest[1..].find(quote) {
                (&rest[1..1 + end], 2 + end)
            } else {
                (&rest[1..], rest.len())
            }
        } else {
            let end = rest
                .find(|c: char| c.is_ascii_whitespace())
                .unwrap_or(rest.len());
            (&rest[..end], end)
        };
        attrs.push((key.to_string(), decode_xml_entities(raw_val)));
        rest = rest[consumed..].trim_start();
    }

    Some((name, attrs))
}

fn decode_xml_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after_amp = &rest[amp + 1..];
        if let Some(semi) = after_amp.find(';') {
            let entity = &after_amp[..semi];
            if let Some(decoded) = decode_single_entity(entity) {
                out.push_str(&decoded);
                rest = &after_amp[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after_amp;
    }
    out.push_str(rest);
    out
}

fn decode_single_entity(entity: &str) -> Option<String> {
    match entity {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" => Some("'".to_string()),
        "nbsp" => Some(" ".to_string()),
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                let code = u32::from_str_radix(hex, 16).ok()?;
                char::from_u32(code).map(|c| c.to_string())
            } else if let Some(dec) = entity.strip_prefix('#') {
                let code = dec.parse::<u32>().ok()?;
                char::from_u32(code).map(|c| c.to_string())
            } else {
                None
            }
        }
    }
}

fn collapse_inline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
