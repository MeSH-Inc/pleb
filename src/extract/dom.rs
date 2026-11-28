use ego_tree::{NodeId, NodeRef};
use scraper::{Html, Node, Selector};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, OnceLock};
use url::Url;

use super::resources::{index_resources_in_subtree, ResourceDescriptor};
use super::url::{normalize_crawl_url, resolve_and_clean_url, ResolvedUrl};
use super::ExtractMode;

#[derive(Debug, Clone, Copy, Default)]
pub struct DomTextMetrics {
    pub visible_text_len: usize,
    pub prose_bytes: usize,
    pub script_count: usize,
    pub has_empty_spa_root: bool,
    pub has_password_input: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NodeMetrics {
    pub prose_bytes: usize,
    pub link_text_bytes: usize,
    pub link_count: usize,
    pub code_bytes: usize,
    pub heading_count: usize,
    pub form_count: usize,
    pub has_landmark: bool,
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
    "skip-main",
    "skipnav",
    "mw-jump-link",
    "mw-editsection",
    "mw-hidden-catlinks",
    "cs1-maint",
    "cs1-hidden-error",
    "mdbook-help-container",
    "wbc-editpage",
    "printfooter",
    "promotedlink",
    "promoted-post",
    "sponsored-post",
    "ad-slot",
    "ad-banner",
    "ad-container",
    "ad-link-bar",
    "left-sidebar",
    "right-sidebar",
];

const CODE_GUTTER_CLASSES: &[&str] = &[
    "hljs-ln",
    "hljs-ln-numbers",
    "blob-num",
    "linenos",
    "gutter",
    "highlighttable",
    "rouge-table",
];

static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static BASE_HREF_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("base[href]").unwrap());
static META_REFRESH_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("meta[http-equiv][content]").unwrap());
static HEAD_LINK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("head link[rel][href]").unwrap());
static BODY_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("body").unwrap());
static ALWAYS_STRIP_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("script, style, noscript, template").unwrap());
static BOILERPLATE_TAG_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("nav, footer, aside").unwrap());
static ARIA_BOILERPLATE_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(concat!(
        "[role=\"navigation\"],",
        "[role=\"search\"],",
        "[role=\"banner\"],",
        "[role=\"contentinfo\"],",
        "[role=\"complementary\"],",
        "[role=\"menu\"],",
        "[role=\"menubar\"],",
        "[role=\"tooltip\"],",
        "[aria-hidden=\"true\"],",
        "[hidden],",
        "dialog:not([open])"
    ))
    .unwrap()
});
static PRE_NOISE_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse(concat!(
        "pre button, pre [role=\"button\"], pre .line-numbers-rows,",
        "h1 button, h2 button, h3 button, h4 button, h5 button, h6 button"
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
static A_HREF_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static BUTTON_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("button").unwrap());
static STYLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("[style]").unwrap());
static IMG_DIM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("img[width], img[height], img[role]").unwrap());
static CHECKBOX_POPUP_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("input[type=\"checkbox\"][aria-haspopup=\"true\"]").unwrap());
pub(crate) static WEBMCP_FORM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("form[toolname][tooldescription]").unwrap());

/// HTML document parsed once via `scraper` (`atomic` tendrils) for metrics,
/// link discovery, and pruned extraction.
pub struct ParsedHtml {
    doc: Html,
    text_metrics_cache: OnceLock<DomTextMetrics>,
}

impl ParsedHtml {
    pub fn parse(html: &str) -> Self {
        Self {
            doc: Html::parse_document(html),
            text_metrics_cache: OnceLock::new(),
        }
    }

    pub fn title(&self) -> Option<String> {
        let title_el = self.doc.select(&TITLE_SEL).next()?;
        collapsed_node_text(*title_el)
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

    pub fn markdown_alternate_url(&self, page_url: &Url) -> Option<Url> {
        let base = self.effective_base_url(page_url);
        for el in self.doc.select(&HEAD_LINK_SEL) {
            let Some(rel) = el.value().attr("rel") else {
                continue;
            };
            if !rel
                .split_whitespace()
                .any(|tok| tok.eq_ignore_ascii_case("alternate"))
                || !crate::policy::is_markdown_content_type(el.value().attr("type"))
            {
                continue;
            }
            let Some(href) = el
                .value()
                .attr("href")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let Ok(resolved) = base.join(href) else {
                continue;
            };
            if resolved != *page_url && crate::policy::is_safe_alternate_url(page_url, &resolved) {
                return Some(resolved);
            }
        }
        None
    }

    pub fn meta_refresh_url(&self, page_url: &Url) -> Option<Url> {
        if self.text_metrics().visible_text_len >= 200 {
            return None;
        }
        let base = self.effective_base_url(page_url);
        for el in self.doc.select(&META_REFRESH_SEL) {
            let Some(equiv) = el.value().attr("http-equiv") else {
                continue;
            };
            if !equiv.trim().eq_ignore_ascii_case("refresh") {
                continue;
            }
            let Some(content) = el.value().attr("content") else {
                continue;
            };
            let Some(raw_target) = parse_meta_refresh_target(content) else {
                continue;
            };
            let Ok(resolved) = base.join(raw_target) else {
                continue;
            };
            if resolved != *page_url && crate::policy::is_safe_subresource_url(page_url, &resolved)
            {
                return Some(resolved);
            }
        }
        None
    }

    pub fn effective_base_url(&self, page_url: &Url) -> Url {
        self.base_href()
            .and_then(|href| page_url.join(&href).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .unwrap_or_else(|| page_url.clone())
    }

    pub fn extract_webmcp_tools(&self, page_url: &Url) -> Vec<super::webmcp::WebMcpToolDescriptor> {
        let effective_base = self.effective_base_url(page_url);
        super::webmcp::extract_declarative_tools(&self.doc, page_url, &effective_base)
    }

    /// Compute bottom-up `NodeMetrics` in a single `O(N)` post-order pass over the DOM,
    /// returning `(metrics_by_node, visited_node_count)`.
    pub fn compute_node_metrics(&self) -> (HashMap<NodeId, NodeMetrics>, usize) {
        collect_doc_metrics(&self.doc, None)
    }

    pub fn text_metrics(&self) -> DomTextMetrics {
        *self
            .text_metrics_cache
            .get_or_init(|| self.compute_text_metrics())
    }

    fn compute_text_metrics(&self) -> DomTextMetrics {
        let body_id = body_or_root_id(&self.doc);
        let mut scan = TextMetricsScan::default();
        let (map, _) = collect_doc_metrics(&self.doc, Some(&mut scan));

        let body_metrics = map.get(&body_id).copied().unwrap_or_default();
        let visible_text_len = body_metrics.prose_bytes + body_metrics.code_bytes;
        let has_empty_spa_root = scan.spa_root_ids.into_iter().any(|id| {
            let m = map.get(&id).copied().unwrap_or_default();
            m.prose_bytes + m.code_bytes < 50
        });

        DomTextMetrics {
            visible_text_len,
            prose_bytes: body_metrics.prose_bytes,
            script_count: scan.script_count,
            has_empty_spa_root,
            has_password_input: scan.has_password_input,
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

    fn prune_and_render(
        &mut self,
        mode: ExtractMode,
        resources_base: Option<&Url>,
        render: impl FnOnce(NodeRef<'_, Node>, &mut String),
    ) -> (String, Vec<ResourceDescriptor>) {
        let metrics = prune_dom(&mut self.doc, mode);
        let root_id = select_root_id(&self.doc, mode, &metrics);
        let mut out = String::new();
        let mut resources = Vec::new();
        if let Some(root) = self.doc.tree.get(root_id) {
            if let Some(base) = resources_base {
                resources = index_resources_in_subtree(&self.doc, root, base);
            }
            render(root, &mut out);
        }
        (out, resources)
    }

    pub fn into_pruned_html(mut self, mode: ExtractMode) -> String {
        self.prune_and_render(mode, None, |r, o| serialize_pruned_html(r, false, o))
            .0
    }

    pub fn into_pruned_html_and_resources(
        mut self,
        mode: ExtractMode,
        base_url: &Url,
    ) -> (String, Vec<ResourceDescriptor>) {
        self.prune_and_render(mode, Some(base_url), |r, o| {
            serialize_pruned_html(r, false, o)
        })
    }

    pub fn into_plain_text(mut self, mode: ExtractMode) -> String {
        let raw = self
            .prune_and_render(mode, None, |r, o| collect_plain_text(r, false, false, o))
            .0;
        normalize_plain_text(&raw)
    }

    pub fn into_plain_text_and_resources(
        mut self,
        mode: ExtractMode,
        base_url: &Url,
    ) -> (String, Vec<ResourceDescriptor>) {
        let (raw, resources) = self.prune_and_render(mode, Some(base_url), |r, o| {
            collect_plain_text(r, false, false, o)
        });
        (normalize_plain_text(&raw), resources)
    }

    pub fn extract_resources(
        mut self,
        mode: ExtractMode,
        base_url: &Url,
    ) -> Vec<ResourceDescriptor> {
        self.prune_and_render(mode, Some(base_url), |_, _| {}).1
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

fn is_spa_root(el: &scraper::node::Element) -> bool {
    if el.name() == "app-root"
        || el.attr("data-reactroot").is_some()
        || el.attr("data-sveltekit-preload-data").is_some()
    {
        return true;
    }
    matches!(el.attr("id"), Some("root" | "app" | "__next" | "__nuxt"))
}

fn is_landmark_element(el: &scraper::node::Element) -> bool {
    matches!(el.name(), "main" | "article")
        || el
            .attr("role")
            .is_some_and(|r| r.eq_ignore_ascii_case("main"))
}

fn is_inert_tag(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "noscript" | "template")
}

fn is_heading_tag(tag: &str) -> bool {
    matches!(tag, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

fn body_or_root_id(doc: &Html) -> NodeId {
    doc.select(&BODY_SEL)
        .next()
        .map(|el| el.id())
        .unwrap_or_else(|| doc.root_element().id())
}

fn collect_doc_metrics(
    doc: &Html,
    scan: Option<&mut TextMetricsScan>,
) -> (HashMap<NodeId, NodeMetrics>, usize) {
    let mut map = HashMap::new();
    let mut visits = 0usize;
    if let Some(root) = doc.tree.get(doc.root_element().id()) {
        walk_node_metrics(root, false, false, &mut map, &mut visits, scan);
    }
    (map, visits)
}

fn is_protected_container(el: &scraper::node::Element) -> bool {
    is_landmark_element(el) || el.name() == "table"
}

#[derive(Default)]
struct TextMetricsScan {
    script_count: usize,
    has_password_input: bool,
    spa_root_ids: Vec<NodeId>,
}

impl NodeMetrics {
    fn accumulate(&mut self, child: Self) {
        self.prose_bytes += child.prose_bytes;
        self.link_text_bytes += child.link_text_bytes;
        self.link_count += child.link_count;
        self.code_bytes += child.code_bytes;
        self.heading_count += child.heading_count;
        self.form_count += child.form_count;
        self.has_landmark |= child.has_landmark;
    }
}

fn walk_node_metrics(
    node: NodeRef<'_, Node>,
    in_link: bool,
    in_code: bool,
    map: &mut HashMap<NodeId, NodeMetrics>,
    visits: &mut usize,
    mut scan: Option<&mut TextMetricsScan>,
) -> NodeMetrics {
    *visits += 1;
    match node.value() {
        Node::Text(text) => {
            let bytes: usize = text.split_whitespace().map(str::len).sum();
            let mut m = NodeMetrics::default();
            if in_code {
                m.code_bytes = bytes;
            } else {
                m.prose_bytes = bytes;
                if in_link {
                    m.link_text_bytes = bytes;
                }
            }
            m
        }
        Node::Element(el) => {
            let tag = el.name();
            if tag == "script" {
                if let Some(s) = scan.as_deref_mut() {
                    s.script_count += 1;
                }
                return NodeMetrics::default();
            }
            if is_inert_tag(tag) || tag == "svg" {
                return NodeMetrics::default();
            }
            if let Some(s) = scan.as_deref_mut() {
                if tag == "input"
                    && el
                        .attr("type")
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"))
                {
                    s.has_password_input = true;
                }
                if is_spa_root(el) {
                    s.spa_root_ids.push(node.id());
                }
            }

            let is_nav_control = matches!(tag, "a" | "button");
            let entering_link = is_nav_control && !in_link;
            let child_in_link = in_link || is_nav_control;
            let child_in_code = in_code || matches!(tag, "pre" | "code");

            let mut acc = NodeMetrics {
                link_count: usize::from(entering_link),
                heading_count: usize::from(is_heading_tag(tag)),
                form_count: usize::from(tag == "form"),
                has_landmark: is_protected_container(el),
                ..Default::default()
            };

            for child in node.children() {
                let cm = walk_node_metrics(
                    child,
                    child_in_link,
                    child_in_code,
                    map,
                    visits,
                    scan.as_deref_mut(),
                );
                acc.accumulate(cm);
            }

            map.insert(node.id(), acc);
            acc
        }
        _ => {
            let mut acc = NodeMetrics::default();
            for child in node.children() {
                let cm =
                    walk_node_metrics(child, in_link, in_code, map, visits, scan.as_deref_mut());
                acc.accumulate(cm);
            }
            acc
        }
    }
}

fn prune_dom(doc: &mut Html, mode: ExtractMode) -> HashMap<NodeId, NodeMetrics> {
    let mut to_detach: Vec<NodeId> = Vec::new();

    collect_selector_ids(doc, &ALWAYS_STRIP_SEL, &mut to_detach);
    collect_selector_ids(doc, &PRE_NOISE_SEL, &mut to_detach);

    if matches!(mode, ExtractMode::Clean | ExtractMode::Article) {
        collect_selector_ids(doc, &BOILERPLATE_TAG_SEL, &mut to_detach);
        collect_selector_ids(doc, &WEBMCP_FORM_SEL, &mut to_detach);

        for el in doc.select(&ARIA_BOILERPLATE_SEL) {
            let name = el.value().name();
            if matches!(name, "html" | "body") {
                continue;
            }
            if find_tex_annotation(*el).is_none() {
                to_detach.push(el.id());
            }
        }

        for el in doc.select(&HEADER_SEL) {
            if el.select(&H1_SEL).next().is_none() {
                to_detach.push(el.id());
            }
        }

        for el in doc.select(&ATTR_SEL) {
            let val = el.value();
            if matches!(val.name(), "html" | "body") || is_landmark_element(val) {
                continue;
            }
            if is_boilerplate_attr(val.attr("id"), val.attr("class"))
                && find_tex_annotation(*el).is_none()
                && !contains_heading_or_landmark(*el)
            {
                to_detach.push(el.id());
            }
        }

        for el in doc.select(&STYLE_SEL) {
            if !matches!(el.value().name(), "html" | "body")
                && el.value().attr("style").is_some_and(is_inline_hidden_style)
                && find_tex_annotation(*el).is_none()
            {
                to_detach.push(el.id());
            }
        }
        for el in doc.select(&IMG_DIM_SEL) {
            let val = el.value();
            let tiny = ["width", "height"].iter().any(|a| {
                val.attr(a)
                    .and_then(parse_html_dimension)
                    .is_some_and(|n| n <= 1)
            });
            let decorative_empty = val
                .attr("role")
                .is_some_and(|r| eq_any_ci(r.trim(), &["presentation", "none"]))
                && val.attr("alt").is_none_or(|a| a.trim().is_empty());
            if tiny || decorative_empty {
                to_detach.push(el.id());
            }
        }
        for desc in doc.root_element().descendants() {
            if let Node::Element(e) = desc.value() {
                let tag = e.name();
                if tag.contains('-')
                    && tag.to_ascii_lowercase().contains("screen-reader")
                    && !contains_heading_or_landmark(desc)
                {
                    to_detach.push(desc.id());
                }
            }
        }
        for el in doc.select(&CHECKBOX_POPUP_SEL) {
            if el.value().attr("checked").is_none() {
                if let Some(parent) = el.parent() {
                    let is_safe_box = matches!(
                        parent.value(),
                        Node::Element(p) if !matches!(p.name(), "html" | "body" | "main" | "article" | "header" | "form" | "section")
                    );
                    if is_safe_box && !contains_heading_or_landmark(parent) {
                        to_detach.push(parent.id());
                    }
                }
            }
        }
        for el in doc.select(&A_HREF_SEL) {
            if is_heading_permalink_anchor(*el) {
                to_detach.push(el.id());
            } else if let Some(skip_target) = standalone_skip_link_detach_target(*el) {
                to_detach.push(skip_target);
            }
        }
        for el in doc.select(&BUTTON_SEL) {
            if is_skip_text(*el)
                && !el
                    .ancestors()
                    .any(|a| matches!(a.value(), Node::Element(e) if is_landmark_element(e)))
            {
                to_detach.push(el.id());
            }
        }
    }

    detach_nodes(doc, to_detach);

    // Single-pass O(N) metrics for landmark-protected link-density pruning & root selection.
    let (mut metrics, _) = collect_doc_metrics(doc, None);

    if matches!(mode, ExtractMode::Clean | ExtractMode::Article) {
        let mut density_detach = Vec::new();
        if let Some(root) = doc.tree.get(doc.root_element().id()) {
            collect_link_density_prune_ids(root, false, false, &metrics, &mut density_detach);
        }
        if !density_detach.is_empty() {
            detach_nodes(doc, density_detach);
            metrics = collect_doc_metrics(doc, None).0;
        }
    }

    metrics
}

fn detach_nodes(doc: &mut Html, ids: Vec<NodeId>) {
    for id in ids {
        if let Some(mut node) = doc.tree.get_mut(id) {
            if node.parent().is_some() {
                node.detach();
            }
        }
    }
}

fn collect_link_density_prune_ids(
    node: NodeRef<'_, Node>,
    in_protected: bool,
    in_header: bool,
    metrics: &HashMap<NodeId, NodeMetrics>,
    out: &mut Vec<NodeId>,
) {
    let Node::Element(el) = node.value() else {
        for child in node.children() {
            collect_link_density_prune_ids(child, in_protected, in_header, metrics, out);
        }
        return;
    };

    let tag = el.name();
    let now_protected = in_protected || is_protected_container(el);
    let now_header = in_header || tag == "header";

    if !now_protected && !matches!(tag, "html" | "body" | "head" | "header") {
        if let Some(m) = metrics.get(&node.id()) {
            let min_links = if in_header { 2 } else { 3 };
            if !m.has_landmark
                && m.heading_count == 0
                && m.code_bytes == 0
                && m.link_count >= min_links
                && m.prose_bytes > 0
                && m.link_text_bytes * 100 > m.prose_bytes * 65
                && (m.link_text_bytes / m.link_count) <= 32
            {
                out.push(node.id());
                return;
            }
        }
    }

    for child in node.children() {
        collect_link_density_prune_ids(child, now_protected, now_header, metrics, out);
    }
}

pub(crate) fn is_element_hidden_or_inert(el: &scraper::node::Element) -> bool {
    is_inert_tag(el.name())
        || (el.name() == "dialog" && el.attr("open").is_none())
        || el.attr("hidden").is_some()
        || el
            .attr("aria-hidden")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
        || el.attr("style").is_some_and(is_inline_hidden_style)
}

pub(crate) fn is_hidden_or_inert(node: NodeRef<'_, Node>) -> bool {
    std::iter::once(node)
        .chain(node.ancestors())
        .any(|n| matches!(n.value(), Node::Element(el) if is_element_hidden_or_inert(el)))
}

fn eq_any_ci(s: &str, opts: &[&str]) -> bool {
    opts.iter().any(|o| s.eq_ignore_ascii_case(o))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

fn parse_css_zero(val: &str) -> bool {
    let n = val
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_alphabetic() || c == '%');
    !n.is_empty() && n.parse::<f32>().is_ok_and(|x| x == 0.0)
}

fn is_offscreen_px(val: &str) -> bool {
    let t = val.trim();
    t.len() > 2
        && t[t.len() - 2..].eq_ignore_ascii_case("px")
        && t[..t.len() - 2]
            .trim()
            .parse::<f32>()
            .is_ok_and(|n| n <= -1000.0)
}

pub(crate) fn is_inline_hidden_style(style: &str) -> bool {
    let (mut overflow_hidden, mut zero_box, mut positioned, mut offscreen) =
        (false, false, false, false);
    for decl in style.split(';') {
        let Some((prop, raw)) = decl.split_once(':') else {
            continue;
        };
        let (prop, trimmed) = (prop.trim(), raw.trim());
        let val = trimmed
            .len()
            .checked_sub(10)
            .filter(|&i| {
                trimmed.is_char_boundary(i) && trimmed[i..].eq_ignore_ascii_case("!important")
            })
            .map_or(trimmed, |i| trimmed[..i].trim_end());
        let zero_clip = prop.eq_ignore_ascii_case("clip")
            && strip_prefix_ci(val, "rect(")
                .and_then(|s| s.strip_suffix(')'))
                .is_some_and(|inner| {
                    let mut n = 0usize;
                    inner
                        .split(|c: char| c == ',' || c.is_ascii_whitespace())
                        .filter(|s| !s.is_empty())
                        .all(|p| {
                            n += 1;
                            parse_css_zero(p)
                        })
                        && n == 4
                });
        if (prop.eq_ignore_ascii_case("display") && val.eq_ignore_ascii_case("none"))
            || (prop.eq_ignore_ascii_case("visibility") && eq_any_ci(val, &["hidden", "collapse"]))
            || (eq_any_ci(prop, &["opacity", "font-size"]) && parse_css_zero(val))
            || (prop.eq_ignore_ascii_case("text-indent") && is_offscreen_px(val))
            || zero_clip
            || (prop.eq_ignore_ascii_case("clip-path")
                && ["inset(50%", "inset(100%", "circle(0"]
                    .iter()
                    .any(|p| strip_prefix_ci(val, p).is_some()))
        {
            return true;
        }
        overflow_hidden |= eq_any_ci(prop, &["overflow", "overflow-x", "overflow-y"])
            && val.eq_ignore_ascii_case("hidden");
        zero_box |=
            eq_any_ci(prop, &["width", "height", "max-width", "max-height"]) && parse_css_zero(val);
        positioned |=
            prop.eq_ignore_ascii_case("position") && eq_any_ci(val, &["absolute", "fixed"]);
        offscreen |= eq_any_ci(prop, &["left", "top"]) && is_offscreen_px(val);
    }
    (overflow_hidden && zero_box) || (positioned && offscreen)
}

fn collapse_non_empty(raw: &str) -> Option<String> {
    let collapsed = collapse_inline_whitespace(raw);
    let trimmed = collapsed.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn node_raw_text(node: NodeRef<'_, Node>) -> String {
    let mut raw = String::new();
    collect_raw_text(node, &mut raw);
    raw
}

pub(crate) fn collapsed_node_text(node: NodeRef<'_, Node>) -> Option<String> {
    collapse_non_empty(&node_raw_text(node))
}

fn is_skip_text(node: NodeRef<'_, Node>) -> bool {
    collapsed_node_text(node)
        .is_some_and(|t| t.len() <= 48 && t.to_ascii_lowercase().starts_with("skip to "))
}

fn is_skip_fragment_link(node: NodeRef<'_, Node>) -> bool {
    let Node::Element(el) = node.value() else {
        return false;
    };
    el.attr("href")
        .is_some_and(|h| h.trim().starts_with('#') && h.trim().len() > 1)
        && is_skip_text(node)
}

fn container_only_skip_links(node: NodeRef<'_, Node>) -> bool {
    let mut saw_skip = false;
    for desc in node.descendants() {
        match desc.value() {
            Node::Element(el) if el.name() == "a" => {
                if !is_skip_fragment_link(desc) {
                    return false;
                }
                saw_skip = true;
            }
            Node::Text(t)
                if !t.trim().is_empty()
                    && !desc
                        .ancestors()
                        .take_while(|a| a.id() != node.id())
                        .any(|a| matches!(a.value(), Node::Element(e) if e.name() == "a")) =>
            {
                return false;
            }
            _ => {}
        }
    }
    saw_skip
}

fn standalone_skip_link_detach_target(anchor: NodeRef<'_, Node>) -> Option<NodeId> {
    if !is_skip_fragment_link(anchor) {
        return None;
    }
    let parent = anchor.parent()?;
    let Node::Element(parent_el) = parent.value() else {
        return None;
    };
    if !container_only_skip_links(parent) {
        return None;
    }
    match parent_el.name() {
        "li" => parent
            .parent()
            .filter(|list| {
                matches!(list.value(), Node::Element(e) if matches!(e.name(), "ul" | "ol"))
                    && container_only_skip_links(*list)
            })
            .map(|list| list.id())
            .or_else(|| Some(parent.id())),
        "div" | "p" | "span" => Some(parent.id()),
        _ => None,
    }
}

fn is_heading_permalink_anchor(node: NodeRef<'_, Node>) -> bool {
    let Node::Element(el) = node.value() else {
        return false;
    };
    let Some(href) = el.attr("href").map(str::trim) else {
        return false;
    };
    if !href.starts_with('#') {
        return false;
    }

    let has_permalink_class = el.attr("class").is_some_and(|cls| {
        cls.split_whitespace().any(|tok| {
            matches!(
                tok.to_ascii_lowercase().as_str(),
                "headerlink"
                    | "anchor"
                    | "anchor-link"
                    | "heading-anchor"
                    | "pilcrow"
                    | "permalink"
                    | "self-link"
                    | "section-anchor"
            )
        })
    });

    let inside_heading = node
        .ancestors()
        .any(|anc| matches!(anc.value(), Node::Element(e) if is_heading_tag(e.name())));

    let cleaned: String = node_raw_text(node)
        .chars()
        .filter(|&c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' | '\u{2060}'
                )
        })
        .collect();
    if matches!(cleaned.as_str(), "¶" | "🔗") {
        return true;
    }
    (has_permalink_class || inside_heading) && matches!(cleaned.as_str(), "" | "#" | "§")
}

fn collect_selector_ids(doc: &Html, selector: &Selector, out: &mut Vec<NodeId>) {
    for el in doc.select(selector) {
        let name = el.value().name();
        if !matches!(name, "html" | "body") {
            out.push(el.id());
        }
    }
}

fn contains_heading_or_landmark(node: NodeRef<'_, Node>) -> bool {
    node.descendants().any(
        |d| matches!(d.value(), Node::Element(e) if e.name() == "h1" || is_landmark_element(e)),
    )
}

fn has_boilerplate_pattern(lower: &str) -> bool {
    BOILERPLATE_ATTR_PATTERNS
        .iter()
        .any(|pat| contains_word_prefix(lower, pat))
}

fn is_boilerplate_attr(id: Option<&str>, class: Option<&str>) -> bool {
    id.is_some_and(|v| has_boilerplate_pattern(&v.to_ascii_lowercase()))
        || class.is_some_and(|v| {
            let lower = v.to_ascii_lowercase();
            has_boilerplate_pattern(&lower) || has_hidden_or_sr_class(&lower)
        })
}

fn has_hidden_or_sr_class(lower: &str) -> bool {
    let has_tok = |target: &str| lower.split_whitespace().any(|t| t == target);
    let has_prefix = |prefixes: &[&str]| {
        lower
            .split_whitespace()
            .any(|t| prefixes.iter().any(|p| t.starts_with(p)))
    };
    has_tok("sr-only")
        || has_tok("visually-hidden")
        || has_tok("screen-reader-text")
        || (has_tok("hidden") && !has_prefix(&["xs:", "sm:", "md:", "lg:", "xl:", "2xl:"]))
        || (has_tok("d-none") && !has_prefix(&["d-sm-", "d-md-", "d-lg-", "d-xl-", "d-xxl-"]))
        || (has_tok("is-hidden")
            && !lower.split_whitespace().any(|t| {
                ["is-block-", "is-flex-", "is-inline-"]
                    .iter()
                    .any(|p| t.starts_with(p))
                    && ["-tablet", "-desktop", "-widescreen", "-fullhd"]
                        .iter()
                        .any(|s| t.ends_with(s))
            }))
}

fn contains_word_prefix(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut offset = 0;
    while let Some(rel) = haystack[offset..].find(needle) {
        let start = offset + rel;
        if start == 0 || !bytes[start - 1].is_ascii_alphanumeric() {
            return true;
        }
        offset = start + 1;
    }
    false
}

fn select_root_id(doc: &Html, mode: ExtractMode, metrics: &HashMap<NodeId, NodeMetrics>) -> NodeId {
    let body_id = body_or_root_id(doc);

    if mode != ExtractMode::Article {
        return body_id;
    }

    let node_text_len = |id: NodeId| {
        metrics
            .get(&id)
            .map(|m| m.prose_bytes + m.code_bytes)
            .unwrap_or(0)
    };

    let articles: Vec<(NodeId, usize)> = doc
        .select(&ARTICLE_SEL)
        .map(|el| (el.id(), node_text_len(el.id())))
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
        .find(|el| node_text_len(el.id()) >= 100)
    {
        return main_el.id();
    }

    let mut best: Option<(NodeId, usize)> = None;
    for el in doc.select(&BLOCK_SEL) {
        let Some(m) = metrics.get(&el.id()) else {
            continue;
        };
        let text_len = m.prose_bytes + m.code_bytes;
        if text_len < 150 {
            continue;
        }
        let score = text_len.saturating_sub(m.link_text_bytes);
        if score >= 150 && best.is_none_or(|(_, b)| score > b) {
            best = Some((el.id(), score));
        }
    }
    if let Some((id, _)) = best {
        return id;
    }

    body_id
}

fn summary_heading_reordered_children<'a>(
    node: NodeRef<'a, Node>,
    tag: &str,
) -> Option<Vec<NodeRef<'a, Node>>> {
    let in_summary_header = tag == "summary"
        || (matches!(tag, "section" | "div" | "header")
            && node
                .parent()
                .is_some_and(|p| matches!(p.value(), Node::Element(e) if e.name() == "summary")));
    if !in_summary_header {
        return None;
    }
    let children: Vec<_> = node.children().collect();
    let h_idx = children
        .iter()
        .position(|c| matches!(c.value(), Node::Element(e) if is_heading_tag(e.name())))?;
    if h_idx == 0 {
        return None;
    }
    let is_inline_meta = |t: &str| {
        matches!(
            t,
            "span"
                | "a"
                | "small"
                | "time"
                | "code"
                | "em"
                | "strong"
                | "sup"
                | "sub"
                | "b"
                | "i"
                | "kbd"
                | "abbr"
                | "mark"
        )
    };
    let mut has_visible = false;
    for &prefix in &children[..h_idx] {
        match prefix.value() {
            Node::Text(t) => has_visible |= !t.trim().is_empty(),
            Node::Element(e) if is_inline_meta(e.name()) => {
                if prefix.descendants().any(|d| {
                    d != prefix
                        && matches!(d.value(), Node::Element(de) if !is_inline_meta(de.name()))
                }) {
                    return None;
                }
                has_visible |= !node_raw_text(prefix).trim().is_empty();
            }
            Node::Comment(_) => {}
            _ => return None,
        }
    }
    has_visible.then(|| {
        std::iter::once(children[h_idx])
            .chain(children[..h_idx].iter().copied())
            .chain(children[h_idx + 1..].iter().copied())
            .collect()
    })
}

fn for_each_ordered_child<'a>(
    node: NodeRef<'a, Node>,
    tag: &str,
    mut f: impl FnMut(NodeRef<'a, Node>),
) {
    if let Some(ordered) = summary_heading_reordered_children(node, tag) {
        for child in ordered {
            f(child);
        }
    } else {
        for child in node.children() {
            f(child);
        }
    }
}

fn emit_math_tag(tex: &str, display: bool, out: &mut String) {
    out.push_str("<math data-tex=\"");
    push_escaped_attr(tex, out);
    out.push_str(if display {
        "\" data-display=\"1\"></math>"
    } else {
        "\" data-display=\"0\"></math>"
    });
}

fn serialize_pruned_html(node: NodeRef<'_, Node>, in_interactive: bool, out: &mut String) {
    match node.value() {
        Node::Text(text) => push_escaped_text(text, out),
        Node::Element(el) => {
            let tag = el.name();
            if is_inert_tag(tag) || tag == "track" {
                return;
            }

            if tag == "svg" {
                if !in_interactive {
                    if let Some(summary) = extract_svg_diagram_summary(node, el) {
                        out.push_str("<svg aria-label=\"");
                        push_escaped_attr(&summary, out);
                        out.push_str("\"></svg>");
                    }
                }
                return;
            }

            if tag == "math" {
                if let Some(tex) = find_tex_annotation(node) {
                    emit_math_tag(&tex, is_display_math(node, el), out);
                    return;
                }
            }

            if tag == "picture" {
                if let Some((src, alt, title)) = resolve_picture_image(node) {
                    emit_img_tag(&src, alt.as_deref(), title.as_deref(), out);
                }
                return;
            }

            if tag == "img" {
                if let Some(src) = resolve_img_src(el) {
                    emit_img_tag(&src, el.attr("alt"), el.attr("title"), out);
                }
                return;
            }

            if tag == "pre" {
                let code = node_raw_text(node);
                if !code.trim().is_empty() {
                    let lang = find_code_lang(
                        node,
                        node.descendants().filter(
                            |n| matches!(n.value(), Node::Element(e) if e.name() == "code"),
                        ),
                    );
                    emit_code_block(lang.as_deref(), &code, out);
                    return;
                }
            }

            if tag == "table" {
                if let Some((lang, code)) = try_extract_code_gutter_table(node) {
                    emit_code_block(lang.as_deref(), &code, out);
                    return;
                }
                if let Some(equations) = try_extract_equation_layout_table(node) {
                    for (tex, eqno) in equations {
                        let tagged = match eqno.as_deref() {
                            Some(raw_eq) if !tex.contains("\\tag") => {
                                let (_, inner) = extract_eqno_tag_inner(raw_eq);
                                if is_valid_eqno_inner(inner) {
                                    std::borrow::Cow::Owned(format!("{tex} \\tag{{{inner}}}"))
                                } else {
                                    std::borrow::Cow::Borrowed(tex.as_str())
                                }
                            }
                            _ => std::borrow::Cow::Borrowed(tex.as_str()),
                        };
                        emit_math_tag(tagged.as_ref(), true, out);
                    }
                    return;
                }
            }

            out.push('<');
            out.push_str(tag);
            for (name, value) in el.attrs() {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                push_escaped_attr(value, out);
                out.push('"');
            }
            out.push('>');

            if is_void_element(tag) {
                return;
            }

            let child_interactive = in_interactive || is_svg_chrome_ancestor(el);
            for_each_ordered_child(node, tag, |child| {
                serialize_pruned_html(child, child_interactive, out);
            });

            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        _ => {
            for child in node.children() {
                serialize_pruned_html(child, in_interactive, out);
            }
        }
    }
}

fn is_void_element(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn push_escaped_text(text: &str, out: &mut String) {
    push_escaped(text, false, out);
}

fn push_escaped_attr(val: &str, out: &mut String) {
    push_escaped(val, true, out);
}

fn push_escaped(val: &str, escape_quote: bool, out: &mut String) {
    for ch in val.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' if escape_quote => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

fn emit_code_block(lang: Option<&str>, code: &str, out: &mut String) {
    out.push_str("<pre><code");
    if let Some(lang) = lang {
        out.push_str(" class=\"language-");
        push_escaped_attr(lang, out);
        out.push('"');
    }
    out.push('>');
    push_escaped_text(code, out);
    out.push_str("</code></pre>");
}

fn emit_img_tag(src: &str, alt: Option<&str>, title: Option<&str>, out: &mut String) {
    out.push_str("<img src=\"");
    push_escaped_attr(src, out);
    out.push('"');
    for (name, val) in [("alt", alt), ("title", title)] {
        if let Some(v) = val {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            push_escaped_attr(v, out);
            out.push('"');
        }
    }
    out.push('>');
}

fn is_data_uri(url: &str) -> bool {
    strip_prefix_ci(url.trim_start(), "data:").is_some()
}

fn valid_img_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty() && !is_data_uri(trimmed)).then(|| trimmed.to_string())
}

pub(crate) fn resolve_img_src(el: &scraper::node::Element) -> Option<String> {
    ["data-src", "data-lazy-src", "data-original"]
        .into_iter()
        .find_map(|attr| el.attr(attr).and_then(valid_img_url))
        .or_else(|| {
            el.attr("srcset")
                .and_then(parse_best_srcset_url)
                .filter(|u| !is_data_uri(u))
        })
        .or_else(|| el.attr("src").and_then(valid_img_url))
}

pub(crate) fn resolve_picture_image(
    picture: NodeRef<'_, Node>,
) -> Option<(String, Option<String>, Option<String>)> {
    let mut best_source: Option<String> = None;
    let mut img_src: Option<String> = None;
    let mut alt: Option<String> = None;
    let mut title: Option<String> = None;

    for child in picture.children() {
        let Node::Element(el) = child.value() else {
            continue;
        };
        match el.name() {
            "source" if best_source.is_none() => {
                best_source = el
                    .attr("srcset")
                    .and_then(parse_best_srcset_url)
                    .filter(|u| !is_data_uri(u));
            }
            "img" => {
                alt = el.attr("alt").map(str::to_string);
                title = el.attr("title").map(str::to_string);
                img_src = resolve_img_src(el);
            }
            _ => {}
        }
    }

    Some((best_source.or(img_src)?, alt, title))
}

/// WHATWG descriptor-aware `srcset` parser that preserves commas inside URLs
/// (such as `/cdn-cgi/image/width=800,quality=80/...`) by splitting candidates
/// only on commas following a descriptor or trailing a URL token.
pub fn parse_best_srcset_url(srcset: &str) -> Option<String> {
    let bytes = srcset.as_bytes();
    let mut i = 0usize;
    let mut best: Option<(String, f64)> = None;

    while i < bytes.len() {
        // 1. Skip leading ASCII whitespace and commas.
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }

        // 2. Collect URL token up to first ASCII whitespace.
        let url_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut url = &srcset[url_start..i];
        let mut score = 1.0f64;

        if url.ends_with(',') {
            url = url.trim_end_matches(',');
        } else {
            // 3. Collect descriptor up to the next top-level comma.
            let desc_start = i;
            let mut paren_depth = 0usize;
            while i < bytes.len() {
                match bytes[i] {
                    b'(' => paren_depth += 1,
                    b')' => paren_depth = paren_depth.saturating_sub(1),
                    b',' if paren_depth == 0 => break,
                    _ => {}
                }
                i += 1;
            }
            let desc = srcset[desc_start..i].trim();
            if i < bytes.len() && bytes[i] == b',' {
                i += 1;
            }
            if let Some(parsed) = parse_srcset_descriptor(desc) {
                score = parsed;
            }
        }

        if !url.is_empty()
            && best
                .as_ref()
                .is_none_or(|(_, best_score)| score >= *best_score)
        {
            best = Some((url.to_string(), score));
        }
    }

    best.map(|(url, _)| url)
}

fn parse_srcset_descriptor(desc: &str) -> Option<f64> {
    let token = desc.split_whitespace().next()?;
    if let Some(w) = token.strip_suffix('w').or_else(|| token.strip_suffix('W')) {
        return w.parse::<f64>().ok().filter(|v| v.is_finite() && *v > 0.0);
    }
    if let Some(x) = token.strip_suffix('x').or_else(|| token.strip_suffix('X')) {
        return x
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v * 1000.0);
    }
    None
}

fn find_tex_annotation(node: NodeRef<'_, Node>) -> Option<String> {
    for desc in node.descendants() {
        if let Node::Element(el) = desc.value() {
            if el.name() == "annotation"
                && el.attr("encoding").is_some_and(|enc| {
                    eq_any_ci(enc, &["application/x-tex", "application/x-latex"])
                })
            {
                let raw = node_raw_text(desc);
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn is_display_math(node: NodeRef<'_, Node>, el: &scraper::node::Element) -> bool {
    if el
        .attr("display")
        .is_some_and(|d| d.eq_ignore_ascii_case("block"))
    {
        return true;
    }
    node.ancestors().take(4).any(|anc| {
        matches!(
            anc.value(),
            Node::Element(e) if e.attr("class").is_some_and(|c| {
                c.split_whitespace().any(|tok| eq_any_ci(tok, &["katex-display", "mjx-display"]))
            })
        )
    })
}

pub(crate) fn parse_html_dimension(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let mut i = 0usize;
    let mut seen_dot = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            i += 1;
        } else if b == b'.' && !seen_dot {
            seen_dot = true;
            i += 1;
        } else {
            break;
        }
    }
    if i == 0 {
        return None;
    }
    let unit = trimmed[i..].trim();
    if !unit.is_empty() && !unit.eq_ignore_ascii_case("px") {
        return None;
    }
    let val = trimmed[..i].parse::<f64>().ok()?;
    if !val.is_finite() || val < 0.0 || val > u32::MAX as f64 {
        return None;
    }
    Some(val.round() as u32)
}

fn is_svg_chrome_ancestor(el: &scraper::node::Element) -> bool {
    matches!(el.name(), "a" | "button" | "nav" | "header" | "footer")
        || el
            .attr("role")
            .is_some_and(|r| r.trim().eq_ignore_ascii_case("button"))
}

fn has_svg_icon_class(el: &scraper::node::Element) -> bool {
    el.attr("class").is_some_and(|cls| {
        cls.split_whitespace().any(|raw| {
            let tok = raw.to_ascii_lowercase();
            matches!(tok.as_str(), "icon" | "octicon" | "lucide" | "feather")
                || ["icon-", "octicon-", "lucide-", "feather-", "fa-"]
                    .iter()
                    .any(|p| tok.starts_with(p))
                || tok.ends_with("-icon")
        })
    })
}

fn extract_svg_diagram_summary(
    node: NodeRef<'_, Node>,
    el: &scraper::node::Element,
) -> Option<String> {
    if el
        .attr("role")
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("button"))
    {
        return None;
    }
    if has_svg_icon_class(el) {
        return None;
    }
    if el
        .attr("width")
        .and_then(parse_html_dimension)
        .is_some_and(|w| w < 64)
        || el
            .attr("height")
            .and_then(parse_html_dimension)
            .is_some_and(|h| h < 64)
    {
        return None;
    }

    let svg_label = extract_svg_label(node, el);
    let segments = collect_svg_text_segments(node);

    match svg_label {
        Some(label) if segments.is_empty() => Some(label),
        Some(label) => Some(format!("{label} — {}", segments.join(", "))),
        None => {
            let total_chars: usize = segments.iter().map(|s| s.chars().count()).sum();
            if segments.len() >= 3 && total_chars >= 12 {
                Some(segments.join(", "))
            } else {
                None
            }
        }
    }
}

fn collect_svg_text_segments(svg_node: NodeRef<'_, Node>) -> Vec<String> {
    const MAX_SEGMENTS: usize = 32;
    let mut segments = Vec::new();
    let mut seen = HashSet::new();
    walk_svg_text_segments(svg_node, &mut segments, &mut seen, MAX_SEGMENTS);
    segments
}

fn is_svg_non_content_tag(tag: &str) -> bool {
    eq_any_ci(tag, &["style", "script", "defs", "title", "desc"])
}

fn walk_svg_text_segments(
    node: NodeRef<'_, Node>,
    segments: &mut Vec<String>,
    seen: &mut HashSet<String>,
    max_segments: usize,
) {
    if segments.len() >= max_segments {
        return;
    }
    for child in node.children() {
        if segments.len() >= max_segments {
            return;
        }
        let Node::Element(el) = child.value() else {
            continue;
        };
        let tag = el.name();
        if is_svg_non_content_tag(tag) {
            continue;
        }
        if eq_any_ci(tag, &["text", "foreignobject"]) {
            let mut raw = String::new();
            collect_svg_visible_text(child, &mut raw);
            if let Some(seg) = collapse_non_empty(&raw) {
                if !is_ignored_svg_segment(&seg) && seen.insert(seg.clone()) {
                    segments.push(seg);
                }
            }
        } else {
            walk_svg_text_segments(child, segments, seen, max_segments);
        }
    }
}

fn collect_svg_visible_text(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(text),
        Node::Element(el) => {
            let tag = el.name();
            if is_svg_non_content_tag(tag) {
                return;
            }
            if eq_any_ci(tag, &["tspan", "div", "p", "br", "li"])
                && !out.is_empty()
                && !out.ends_with(char::is_whitespace)
            {
                out.push(' ');
            }
            for child in node.children() {
                collect_svg_visible_text(child, out);
            }
        }
        _ => {
            for child in node.children() {
                collect_svg_visible_text(child, out);
            }
        }
    }
}

fn is_ignored_svg_segment(seg: &str) -> bool {
    let char_count = seg.chars().count();
    char_count > 80 || (char_count == 1 && !seg.chars().next().unwrap().is_alphanumeric())
}

fn extract_svg_label(node: NodeRef<'_, Node>, el: &scraper::node::Element) -> Option<String> {
    el.attr("aria-label")
        .and_then(collapse_non_empty)
        .or_else(|| {
            node.children()
                .filter(|child| matches!(child.value(), Node::Element(e) if e.name() == "title"))
                .find_map(collapsed_node_text)
        })
}

fn try_extract_code_gutter_table(table: NodeRef<'_, Node>) -> Option<(Option<String>, String)> {
    // Never unwrap tables that contain <thead> or <th> headers.
    if table
        .descendants()
        .any(|d| matches!(d.value(), Node::Element(el) if matches!(el.name(), "thead" | "th")))
    {
        return None;
    }

    let mut rows = Vec::new();
    collect_tbody_rows(table, false, &mut rows)?;
    if rows.is_empty() || rows.iter().any(|tds| tds.len() != 2) {
        return None;
    }

    let mut explicit_gutter = matches!(table.value(), Node::Element(el) if has_gutter_marker(el));
    let mut sequential_ints = rows.len() >= 3;
    let mut all_code_cells = true;

    for (idx, tds) in rows.iter().enumerate() {
        let (col0, col1) = (tds[0], tds[1]);
        if matches!(col0.value(), Node::Element(el) if has_gutter_marker(el)) {
            explicit_gutter = true;
        }
        if sequential_ints && node_raw_text(col0).trim().parse::<usize>().ok() != Some(idx + 1) {
            sequential_ints = false;
        }
        if !subtree_has_code_tag(col1) {
            all_code_cells = false;
        }
    }

    if !explicit_gutter && !(sequential_ints && all_code_cells) {
        return None;
    }

    let lang = find_code_lang(
        table,
        rows.first()
            .into_iter()
            .flat_map(|tds| tds[1].descendants()),
    );
    let lines: Vec<String> = rows
        .iter()
        .map(|tds| {
            node_raw_text(tds[1])
                .trim_end_matches(['\r', '\n'])
                .to_string()
        })
        .collect();

    Some((lang, lines.join("\n")))
}

fn collect_tbody_rows<'a>(
    node: NodeRef<'a, Node>,
    strict: bool,
    rows: &mut Vec<Vec<NodeRef<'a, Node>>>,
) -> Option<()> {
    for child in node.children() {
        let Node::Element(el) = child.value() else {
            continue;
        };
        match el.name() {
            "tbody" => collect_tbody_rows(child, strict, rows)?,
            "tr" => {
                let mut tds = Vec::new();
                for cell in child.children() {
                    if let Node::Element(ce) = cell.value() {
                        if ce.name() == "td" {
                            tds.push(cell);
                        } else if strict {
                            return None;
                        }
                    }
                }
                rows.push(tds);
            }
            "colgroup" | "col" => {}
            _ if strict => return None,
            _ => {}
        }
    }
    Some(())
}

fn has_gutter_marker(el: &scraper::node::Element) -> bool {
    el.attr("data-line-number").is_some()
        || el.attr("class").is_some_and(|cls| {
            cls.split_whitespace()
                .any(|tok| eq_any_ci(tok, CODE_GUTTER_CLASSES))
        })
}

fn subtree_has_code_tag(node: NodeRef<'_, Node>) -> bool {
    node.descendants()
        .any(|d| matches!(d.value(), Node::Element(el) if matches!(el.name(), "pre" | "code")))
}

fn find_code_lang<'a>(
    container: NodeRef<'a, Node>,
    inner: impl Iterator<Item = NodeRef<'a, Node>>,
) -> Option<String> {
    inner
        .chain(std::iter::once(container))
        .chain(container.ancestors().take(3))
        .find_map(|node| match node.value() {
            Node::Element(el) => extract_lang_from_element(el),
            _ => None,
        })
}

fn try_extract_equation_layout_table(
    table: NodeRef<'_, Node>,
) -> Option<Vec<(String, Option<String>)>> {
    if table.descendants().skip(1).any(|d| {
        matches!(
            d.value(),
            Node::Element(el) if matches!(el.name(), "thead" | "th" | "caption" | "table")
        )
    }) {
        return None;
    }

    let mut rows = Vec::new();
    collect_tbody_rows(table, true, &mut rows)?;
    if rows.is_empty() {
        return None;
    }

    let mut equations = Vec::with_capacity(rows.len());
    for tds in rows {
        let mut row_math = Vec::new();
        let mut row_eqno: Option<String> = None;

        for td in tds {
            let has_math = td
                .descendants()
                .any(|d| matches!(d.value(), Node::Element(e) if e.name() == "math"));
            if has_math {
                if !collect_td_math_cell(td, &mut row_math) {
                    return None;
                }
                continue;
            }

            if td.descendants().any(|d| {
                matches!(d.value(), Node::Element(e) if matches!(e.name(), "img" | "svg" | "input"))
            }) {
                return None;
            }

            let cell_text = node_raw_text(td);
            let trimmed = cell_text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if row_eqno.is_some() || !(1..=24).contains(&trimmed.len()) {
                return None;
            }
            let (enclosed, inner) = extract_eqno_tag_inner(trimmed);
            if !has_eqno_marker(td) && !(enclosed && is_valid_eqno_inner(inner)) {
                return None;
            }
            row_eqno = Some(trimmed.to_string());
        }

        if row_math.is_empty() {
            if row_eqno.is_some() {
                return None;
            }
            continue;
        }
        equations.push((row_math.join(" "), row_eqno));
    }

    (!equations.is_empty()).then_some(equations)
}

fn collect_td_math_cell(node: NodeRef<'_, Node>, out: &mut Vec<String>) -> bool {
    match node.value() {
        Node::Text(text) => text
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ',' | '.' | ';' | ':')),
        Node::Element(el) => {
            if is_inert_tag(el.name()) {
                return true;
            }
            if el.name() == "math" {
                let Some(tex) = find_tex_annotation(node) else {
                    return false;
                };
                out.push(tex);
                return true;
            }
            if matches!(el.name(), "img" | "svg" | "input") {
                return false;
            }
            node.children()
                .all(|child| collect_td_math_cell(child, out))
        }
        _ => node
            .children()
            .all(|child| collect_td_math_cell(child, out)),
    }
}

fn has_eqno_marker(td: NodeRef<'_, Node>) -> bool {
    td.descendants().any(|d| {
        matches!(
            d.value(),
            Node::Element(el)
                if (d == td || el.name() == "span")
                    && el.attr("class").is_some_and(|c| contains_ascii_ci(c.as_bytes(), b"eqno"))
        )
    })
}

fn extract_eqno_tag_inner(trimmed: &str) -> (bool, &str) {
    let enclosed = (trimmed.starts_with('(') && trimmed.ends_with(')') && trimmed.len() >= 2)
        || (trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2);
    let core = if enclosed {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    (enclosed, core.trim())
}

fn is_valid_eqno_inner(inner: &str) -> bool {
    !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '\'' | ' '))
}

fn is_valid_lang_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_')
}

fn normalize_lang_token(raw: &str) -> Option<String> {
    let cleaned = raw.trim_matches(|c: char| !is_valid_lang_char(c));
    if cleaned.is_empty()
        || !cleaned
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        || !cleaned.chars().all(is_valid_lang_char)
    {
        return None;
    }
    Some(cleaned.to_ascii_lowercase())
}

fn extract_lang_from_element(el: &scraper::node::Element) -> Option<String> {
    let cls = el.attr("class")?;
    let mut tokens = cls.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        for prefix in ["language-", "lang-", "highlight-source-"] {
            if let Some(rest) = tok.strip_prefix(prefix) {
                if let Some(lang) = normalize_lang_token(rest) {
                    return Some(lang);
                }
            }
        }
        if let Some(rest) = strip_prefix_ci(tok, "brush:") {
            let candidate = if !rest.is_empty() {
                rest
            } else {
                tokens.peek().copied().unwrap_or("")
            };
            if let Some(lang) = normalize_lang_token(candidate) {
                return Some(lang);
            }
        }
    }
    None
}

pub(crate) fn collect_raw_text(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(text),
        Node::Element(el) if is_inert_tag(el.name()) => {}
        _ => {
            for child in node.children() {
                collect_raw_text(child, out);
            }
        }
    }
}

fn collect_plain_text(
    node: NodeRef<'_, Node>,
    in_pre: bool,
    in_interactive: bool,
    out: &mut String,
) {
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
            if is_inert_tag(tag) {
                return;
            }
            if tag == "svg" {
                if !in_interactive {
                    if let Some(summary) = extract_svg_diagram_summary(node, el) {
                        if !out.is_empty() && !out.ends_with([' ', '\n']) {
                            out.push(' ');
                        }
                        out.push_str("[Diagram: ");
                        out.push_str(&summary);
                        out.push(']');
                    }
                }
                return;
            }
            if tag == "math" {
                if let Some(tex) = find_tex_annotation(node) {
                    if !out.is_empty() && !out.ends_with([' ', '\n']) {
                        out.push(' ');
                    }
                    out.push('$');
                    out.push_str(&tex);
                    out.push('$');
                    return;
                }
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
                    | "details"
                    | "summary"
                    | "hr"
            );
            let is_cell = matches!(tag, "td" | "th");

            if is_block && !out.is_empty() && !out.ends_with('\n') {
                trim_trailing_spaces(out);
                out.push('\n');
            }

            let child_pre = in_pre || tag == "pre";
            let child_interactive = in_interactive || is_svg_chrome_ancestor(el);
            for_each_ordered_child(node, tag, |child| {
                collect_plain_text(child, child_pre, child_interactive, out);
            });

            if is_cell && !out.is_empty() && !out.ends_with([' ', '\n']) {
                out.push(' ');
            } else if is_block && !out.is_empty() && !out.ends_with('\n') {
                trim_trailing_spaces(out);
                out.push('\n');
            }
        }
        _ => {
            for child in node.children() {
                collect_plain_text(child, in_pre, in_interactive, out);
            }
        }
    }
}

fn trim_trailing_spaces(s: &mut String) {
    while s.ends_with(' ') {
        s.pop();
    }
}

pub(crate) fn collapse_inline_whitespace(input: &str) -> String {
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

/// Fast zero-allocation pre-check avoiding eager HTML parsing when no
/// `<meta http-equiv="refresh">` tag can be present.
pub fn may_contain_meta_refresh(html: &str) -> bool {
    if html.len() > 2_000_000 {
        return false;
    }
    let bytes = html.as_bytes();
    contains_ascii_ci(bytes, b"http-equiv") && contains_ascii_ci(bytes, b"refresh")
}

fn contains_ascii_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

fn parse_meta_refresh_target(content: &str) -> Option<&str> {
    let trimmed = content.trim();
    let idx = trimmed.find([';', ','])?;
    let (delay_part, rest) = (&trimmed[..idx], trimmed[idx + 1..].trim());
    let delay_secs: u64 = delay_part.trim().parse().ok()?;
    if delay_secs > 5 {
        return None;
    }
    let after_url = strip_prefix_ci(rest, "url=")?.trim();
    let unquoted = after_url
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| {
            after_url
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
        })
        .unwrap_or(after_url)
        .trim();
    (!unquoted.is_empty()).then_some(unquoted)
}

#[cfg(test)]
mod meta_refresh_tests {
    use super::*;

    #[test]
    fn meta_refresh_parses_redirect_stub_and_ignores_content_pages() {
        let base = Url::parse("https://www.inkandswitch.com/local-first/").unwrap();
        let stub = ParsedHtml::parse(
            r#"<!doctype html><html><head><meta http-equiv="refresh" content="0; url=/essay/local-first"></head></html>"#,
        );
        assert_eq!(
            stub.meta_refresh_url(&base).as_ref().map(Url::as_str),
            Some("https://www.inkandswitch.com/essay/local-first")
        );

        let quoted = ParsedHtml::parse(
            r#"<html><head><meta http-equiv="Refresh" content="2; URL='https://example.org/next'"></head></html>"#,
        );
        assert_eq!(
            quoted.meta_refresh_url(&base).as_ref().map(Url::as_str),
            Some("https://example.org/next")
        );

        let slow = ParsedHtml::parse(
            r#"<html><head><meta http-equiv="refresh" content="30; url=/reload"></head></html>"#,
        );
        assert!(slow.meta_refresh_url(&base).is_none());

        let js_scheme = ParsedHtml::parse(
            r#"<html><head><meta http-equiv="refresh" content="0; url=javascript:alert(1)"></head></html>"#,
        );
        assert!(js_scheme.meta_refresh_url(&base).is_none());

        let full_body = format!(
            r#"<html><head><meta http-equiv="refresh" content="0; url=/other"></head><body><p>{}</p></body></html>"#,
            "substantial article content ".repeat(12)
        );
        assert!(ParsedHtml::parse(&full_body)
            .meta_refresh_url(&base)
            .is_none());

        // Pre-parse fast check skips normal HTML without meta-refresh tokens.
        assert!(!may_contain_meta_refresh(
            "<html><head><title>Normal</title></head><body><p>Refresh your view</p></body></html>"
        ));
        assert!(may_contain_meta_refresh(
            r#"<html><head><meta HTTP-EQUIV="Refresh" content="0; url=/next"></head></html>"#
        ));

        // SSRF & HTTPS->HTTP downgrade targets are blocked from a public HTTPS origin.
        for blocked in [
            "http://169.254.169.254/latest/meta-data",
            "https://127.0.0.1/admin",
            "https://localhost:8080/internal",
            "http://example.org/downgrade",
        ] {
            let html = format!(
                r#"<html><head><meta http-equiv="refresh" content="0; url={blocked}"></head></html>"#
            );
            assert!(
                ParsedHtml::parse(&html).meta_refresh_url(&base).is_none(),
                "meta-refresh SSRF/downgrade target must be blocked: {blocked}"
            );
        }
    }
}
