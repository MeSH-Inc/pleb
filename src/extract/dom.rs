use ego_tree::NodeId;
use scraper::{ElementRef, Html, Node, Selector};
use std::collections::HashSet;
use std::sync::LazyLock;
use url::Url;

use super::url::{normalize_crawl_url, resolve_and_clean_url, ResolvedUrl};
use super::ExtractMode;

#[derive(Debug, Clone, Copy, Default)]
pub struct DomTextMetrics {
    pub visible_text_len: usize,
    pub script_count: usize,
    pub has_empty_spa_root: bool,
}

const BOILERPLATE_ATTR_PATTERNS: &[&str] = &[
    "cookie-banner",
    "cookie-consent",
    "cookie-notice",
    "cookie-modal",
    "onetrust",
    "cmp-container",
    "skip-link",
    "skip-to-content",
    "skipnav",
];

static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static BASE_HREF_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("base[href]").unwrap());
static SCRIPT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("script").unwrap());
static BODY_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").unwrap());
static SPA_ROOT_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#root, #app, #__next, #__nuxt, app-root").unwrap());
static ALWAYS_STRIP_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("script, style, noscript, template").unwrap());
static IMG_SRC_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img[src]").unwrap());
static BOILERPLATE_TAG_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("svg, nav, footer, aside").unwrap());
static ARIA_BOILERPLATE_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(concat!(
        "[role=\"navigation\"],",
        "[role=\"banner\"],",
        "[role=\"contentinfo\"],",
        "[role=\"complementary\"],",
        "[aria-hidden=\"true\"],",
        "[hidden]"
    ))
    .unwrap()
});
static HEADER_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("header").unwrap());
static H1_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
static ATTR_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("[id], [class]").unwrap());
static ARTICLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("article").unwrap());
static MAIN_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("main, [role=\"main\"]").unwrap());
static BLOCK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("section, div").unwrap());
static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static A_HREF_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());

/// HTML document parsed once via `scraper` (`atomic` tendrils) for metrics,
/// link discovery, and pruned extraction.
pub struct ParsedHtml {
    doc: Html,
}

impl ParsedHtml {
    pub fn parse(html: &str) -> Self {
        Self {
            doc: Html::parse_document(html),
        }
    }

    pub fn title(&self) -> Option<String> {
        let title_el = self.doc.select(&TITLE_SEL).next()?;
        let raw: String = title_el.text().collect();
        let collapsed = collapse_inline_whitespace(&raw);
        let trimmed = collapsed.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    pub fn base_href(&self) -> Option<String> {
        self.doc
            .select(&BASE_HREF_SEL)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    pub fn effective_base_url(&self, page_url: &Url) -> Url {
        self.base_href()
            .and_then(|href| page_url.join(&href).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .unwrap_or_else(|| page_url.clone())
    }

    pub fn text_metrics(&self) -> DomTextMetrics {
        let script_count = self.doc.select(&SCRIPT_SEL).count();
        let body_id = self
            .doc
            .select(&BODY_SEL)
            .next()
            .map(|el| el.id())
            .unwrap_or_else(|| self.doc.root_element().id());

        let visible_text_len = visible_text_len_of(self.doc.tree.get(body_id));
        let has_empty_spa_root = self
            .doc
            .select(&SPA_ROOT_SEL)
            .any(|el| visible_text_len_of(Some(*el)) < 50);

        DomTextMetrics {
            visible_text_len,
            script_count,
            has_empty_spa_root,
        }
    }

    pub fn links(&self, page_url: &Url) -> Vec<Url> {
        let base = self.effective_base_url(page_url);
        let mut links = Vec::new();
        let mut seen = HashSet::new();

        for element in self.doc.select(&A_HREF_SEL) {
            let Some(href) = element.value().attr("href") else {
                continue;
            };
            let trimmed = href.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let ResolvedUrl::Resolved(cleaned) = resolve_and_clean_url(trimmed, &base) {
                if let Ok(resolved) = Url::parse(&cleaned) {
                    if !matches!(resolved.scheme(), "http" | "https") {
                        continue;
                    }
                    let norm = normalize_crawl_url(&resolved);
                    if seen.insert(norm) {
                        links.push(resolved);
                    }
                }
            }
        }

        links
    }

    pub fn into_pruned_html(mut self, mode: ExtractMode) -> String {
        prune_dom(&mut self.doc, mode);
        let root_id = select_root_id(&self.doc, mode);
        self.doc
            .tree
            .get(root_id)
            .and_then(ElementRef::wrap)
            .map(|el| el.html())
            .unwrap_or_default()
    }

    pub fn into_plain_text(mut self, mode: ExtractMode) -> String {
        prune_dom(&mut self.doc, mode);
        let root_id = select_root_id(&self.doc, mode);

        let mut raw = String::new();
        if let Some(root) = self.doc.tree.get(root_id) {
            collect_plain_text(root, false, &mut raw);
        }
        normalize_plain_text(&raw)
    }
}

pub fn extract_title(html: &str) -> Option<String> {
    ParsedHtml::parse(html).title()
}

pub fn dom_text_metrics(html: &str) -> DomTextMetrics {
    ParsedHtml::parse(html).text_metrics()
}

pub fn prune_html_for_markdown(html: &str, mode: ExtractMode) -> String {
    ParsedHtml::parse(html).into_pruned_html(mode)
}

pub fn extract_plain_text(html: &str, mode: ExtractMode) -> String {
    ParsedHtml::parse(html).into_plain_text(mode)
}

fn prune_dom(doc: &mut Html, mode: ExtractMode) {
    let mut to_detach: Vec<NodeId> = Vec::new();

    collect_selector_ids(doc, &ALWAYS_STRIP_SEL, &mut to_detach);

    // Drop inline data: URI images that bloat markdown output without adding value.
    for el in doc.select(&IMG_SRC_SEL) {
        if let Some(src) = el.value().attr("src") {
            if src.trim_start().to_ascii_lowercase().starts_with("data:") {
                to_detach.push(el.id());
            }
        }
    }

    if matches!(mode, ExtractMode::Clean | ExtractMode::Article) {
        collect_selector_ids(doc, &BOILERPLATE_TAG_SEL, &mut to_detach);
        collect_selector_ids(doc, &ARIA_BOILERPLATE_SEL, &mut to_detach);

        for el in doc.select(&HEADER_SEL) {
            if el.select(&H1_SEL).next().is_none() {
                to_detach.push(el.id());
            }
        }

        for el in doc.select(&ATTR_SEL) {
            let name = el.value().name();
            if matches!(name, "html" | "body" | "main" | "article") {
                continue;
            }
            if is_boilerplate_attr(el.value().attr("id"), el.value().attr("class")) {
                to_detach.push(el.id());
            }
        }
    }

    for id in to_detach {
        if let Some(mut node) = doc.tree.get_mut(id) {
            if node.parent().is_some() {
                node.detach();
            }
        }
    }
}

fn collect_selector_ids(doc: &Html, selector: &Selector, out: &mut Vec<NodeId>) {
    for el in doc.select(selector) {
        let name = el.value().name();
        if !matches!(name, "html" | "body") {
            out.push(el.id());
        }
    }
}

fn is_boilerplate_attr(id: Option<&str>, class: Option<&str>) -> bool {
    let mut combined = String::new();
    if let Some(id) = id {
        combined.push_str(&id.to_ascii_lowercase());
        combined.push(' ');
    }
    if let Some(class) = class {
        combined.push_str(&class.to_ascii_lowercase());
    }
    if combined.is_empty() {
        return false;
    }

    if BOILERPLATE_ATTR_PATTERNS
        .iter()
        .any(|pat| combined.contains(pat))
    {
        return true;
    }

    class
        .unwrap_or("")
        .split_whitespace()
        .any(|tok| tok.eq_ignore_ascii_case("sr-only"))
}

fn select_root_id(doc: &Html, mode: ExtractMode) -> NodeId {
    let body_id = doc
        .select(&BODY_SEL)
        .next()
        .map(|el| el.id())
        .unwrap_or_else(|| doc.root_element().id());

    if mode != ExtractMode::Article {
        return body_id;
    }

    let articles: Vec<(NodeId, usize)> = doc
        .select(&ARTICLE_SEL)
        .map(|el| (el.id(), visible_text_len_of(Some(*el))))
        .filter(|(_, len)| *len >= 100)
        .collect();

    if articles.len() == 1 {
        return articles[0].0;
    }
    if articles.len() > 1 {
        let total: usize = articles.iter().map(|(_, l)| *l).sum();
        if let Some((best_id, best_len)) = articles.iter().max_by_key(|(_, l)| *l) {
            if *best_len * 100 >= total * 65 {
                return *best_id;
            }
        }
    }

    if let Some(main_el) = doc
        .select(&MAIN_SEL)
        .find(|el| visible_text_len_of(Some(**el)) >= 100)
    {
        return main_el.id();
    }

    let mut best: Option<(NodeId, usize)> = None;
    for el in doc.select(&BLOCK_SEL) {
        let text_len = visible_text_len_of(Some(*el));
        if text_len < 150 {
            continue;
        }
        let link_len: usize = el
            .select(&A_SEL)
            .map(|a| visible_text_len_of(Some(*a)))
            .sum();
        let score = text_len.saturating_sub(link_len);
        if score >= 150 && best.map_or(true, |(_, b)| score > b) {
            best = Some((el.id(), score));
        }
    }
    if let Some((id, _)) = best {
        return id;
    }

    body_id
}

fn visible_text_len_of(node: Option<ego_tree::NodeRef<'_, Node>>) -> usize {
    let Some(node) = node else {
        return 0;
    };
    let mut len = 0usize;
    accumulate_visible_len(node, &mut len);
    len
}

fn accumulate_visible_len(node: ego_tree::NodeRef<'_, Node>, len: &mut usize) {
    match node.value() {
        Node::Text(text) => {
            *len += text.split_whitespace().map(str::len).sum::<usize>();
        }
        Node::Element(el) => {
            let tag = el.name();
            if matches!(tag, "script" | "style" | "noscript" | "template" | "svg") {
                return;
            }
            for child in node.children() {
                accumulate_visible_len(child, len);
            }
        }
        _ => {
            for child in node.children() {
                accumulate_visible_len(child, len);
            }
        }
    }
}

fn collect_plain_text(node: ego_tree::NodeRef<'_, Node>, in_pre: bool, out: &mut String) {
    match node.value() {
        Node::Text(text) => {
            if in_pre {
                out.push_str(text);
            } else {
                let collapsed = collapse_inline_whitespace(text);
                if !collapsed.is_empty() {
                    if collapsed.starts_with(' ') && out.ends_with([' ', '\n']) {
                        out.push_str(collapsed.trim_start_matches(' '));
                    } else {
                        out.push_str(&collapsed);
                    }
                }
            }
        }
        Node::Element(el) => {
            let tag = el.name();
            if matches!(tag, "script" | "style" | "noscript" | "template" | "svg") {
                return;
            }
            if tag == "br" {
                trim_trailing_spaces(out);
                out.push('\n');
                return;
            }

            let is_block = matches!(
                tag,
                "p" | "div"
                    | "section"
                    | "article"
                    | "main"
                    | "header"
                    | "footer"
                    | "aside"
                    | "nav"
                    | "blockquote"
                    | "pre"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "li"
                    | "ul"
                    | "ol"
                    | "tr"
                    | "dt"
                    | "dd"
                    | "figcaption"
                    | "figure"
                    | "hr"
            );
            let is_cell = matches!(tag, "td" | "th");

            if is_block && !out.is_empty() && !out.ends_with('\n') {
                trim_trailing_spaces(out);
                out.push('\n');
            }

            let child_pre = in_pre || tag == "pre";
            for child in node.children() {
                collect_plain_text(child, child_pre, out);
            }

            if is_cell && !out.is_empty() && !out.ends_with([' ', '\n']) {
                out.push(' ');
            } else if is_block && !out.is_empty() && !out.ends_with('\n') {
                trim_trailing_spaces(out);
                out.push('\n');
            }
        }
        _ => {
            for child in node.children() {
                collect_plain_text(child, in_pre, out);
            }
        }
    }
}

fn trim_trailing_spaces(s: &mut String) {
    while s.ends_with(' ') {
        s.pop();
    }
}

fn collapse_inline_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn normalize_plain_text(raw: &str) -> String {
    let mut lines = Vec::new();
    let mut blank_run = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
        } else {
            blank_run = 0;
            lines.push(trimmed.to_string());
        }
    }

    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    lines.join("\n")
}
