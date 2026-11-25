use ego_tree::NodeId;
use scraper::{ElementRef, Html, Node, Selector};

use super::ExtractMode;

#[derive(Debug, Clone, Copy, Default)]
pub struct DomTextMetrics {
    pub visible_text_len: usize,
    pub script_count: usize,
    pub has_empty_spa_root: bool,
}

const ALWAYS_STRIP_TAGS: &[&str] = &["script", "style", "noscript", "template"];
const BOILERPLATE_TAGS: &[&str] = &["svg", "nav", "footer", "aside"];
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

pub fn extract_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    let title_el = doc.select(&sel).next()?;
    let raw: String = title_el.text().collect();
    let collapsed = collapse_inline_whitespace(&raw);
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn dom_text_metrics(html: &str) -> DomTextMetrics {
    let doc = Html::parse_document(html);
    let script_count = Selector::parse("script")
        .map(|s| doc.select(&s).count())
        .unwrap_or(0);

    let body_id = Selector::parse("body")
        .ok()
        .and_then(|s| doc.select(&s).next().map(|el| el.id()))
        .unwrap_or_else(|| doc.root_element().id());

    let visible_text_len = visible_text_len_of(doc.tree.get(body_id));

    let spa_selectors = ["#root", "#app", "#__next", "#__nuxt", "app-root"];
    let mut has_empty_spa_root = false;
    for pat in spa_selectors {
        if let Ok(sel) = Selector::parse(pat) {
            for el in doc.select(&sel) {
                if visible_text_len_of(Some(*el)) < 50 {
                    has_empty_spa_root = true;
                    break;
                }
            }
        }
        if has_empty_spa_root {
            break;
        }
    }

    DomTextMetrics {
        visible_text_len,
        script_count,
        has_empty_spa_root,
    }
}

pub fn prune_html_for_markdown(html: &str, mode: ExtractMode) -> String {
    let mut doc = Html::parse_document(html);
    prune_dom(&mut doc, mode);
    let root_id = select_root_id(&doc, mode);
    doc.tree
        .get(root_id)
        .and_then(ElementRef::wrap)
        .map(|el| el.html())
        .unwrap_or_default()
}

pub fn extract_plain_text(html: &str, mode: ExtractMode) -> String {
    let mut doc = Html::parse_document(html);
    prune_dom(&mut doc, mode);
    let root_id = select_root_id(&doc, mode);

    let mut raw = String::new();
    if let Some(root) = doc.tree.get(root_id) {
        collect_plain_text(root, false, &mut raw);
    }
    normalize_plain_text(&raw)
}

fn prune_dom(doc: &mut Html, mode: ExtractMode) {
    let mut to_detach: Vec<NodeId> = Vec::new();

    for tag in ALWAYS_STRIP_TAGS {
        collect_selector_ids(doc, tag, &mut to_detach);
    }

    // Drop inline data: URI images that bloat markdown output without adding value.
    if let Ok(img_sel) = Selector::parse("img[src]") {
        for el in doc.select(&img_sel) {
            if let Some(src) = el.value().attr("src") {
                if src.trim_start().to_ascii_lowercase().starts_with("data:") {
                    to_detach.push(el.id());
                }
            }
        }
    }

    if matches!(mode, ExtractMode::Clean | ExtractMode::Article) {
        for tag in BOILERPLATE_TAGS {
            collect_selector_ids(doc, tag, &mut to_detach);
        }

        let aria_sel = concat!(
            "[role=\"navigation\"],",
            "[role=\"banner\"],",
            "[role=\"contentinfo\"],",
            "[role=\"complementary\"],",
            "[aria-hidden=\"true\"],",
            "[hidden]"
        );
        collect_selector_ids(doc, aria_sel, &mut to_detach);

        if let (Ok(header_sel), Ok(h1_sel)) = (Selector::parse("header"), Selector::parse("h1")) {
            for el in doc.select(&header_sel) {
                if el.select(&h1_sel).next().is_none() {
                    to_detach.push(el.id());
                }
            }
        }

        if let Ok(all_sel) = Selector::parse("[id], [class]") {
            for el in doc.select(&all_sel) {
                let name = el.value().name();
                if matches!(name, "html" | "body" | "main" | "article") {
                    continue;
                }
                if is_boilerplate_attr(el.value().attr("id"), el.value().attr("class")) {
                    to_detach.push(el.id());
                }
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

fn collect_selector_ids(doc: &Html, selector: &str, out: &mut Vec<NodeId>) {
    if let Ok(sel) = Selector::parse(selector) {
        for el in doc.select(&sel) {
            let name = el.value().name();
            if !matches!(name, "html" | "body") {
                out.push(el.id());
            }
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
    let body_id = Selector::parse("body")
        .ok()
        .and_then(|s| doc.select(&s).next().map(|el| el.id()))
        .unwrap_or_else(|| doc.root_element().id());

    if mode != ExtractMode::Article {
        return body_id;
    }

    if let Ok(article_sel) = Selector::parse("article") {
        let articles: Vec<(NodeId, usize)> = doc
            .select(&article_sel)
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
    }

    if let Ok(main_sel) = Selector::parse("main, [role=\"main\"]") {
        if let Some(main_el) = doc
            .select(&main_sel)
            .find(|el| visible_text_len_of(Some(**el)) >= 100)
        {
            return main_el.id();
        }
    }

    if let (Ok(block_sel), Ok(link_sel)) = (Selector::parse("section, div"), Selector::parse("a")) {
        let mut best: Option<(NodeId, usize)> = None;
        for el in doc.select(&block_sel) {
            let text_len = visible_text_len_of(Some(*el));
            if text_len < 150 {
                continue;
            }
            let link_len: usize = el
                .select(&link_sel)
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
                out.push_str(&text);
            } else {
                let collapsed = collapse_inline_whitespace(&text);
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
