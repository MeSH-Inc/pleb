use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ego_tree::{NodeId, NodeRef};
use scraper::{node::Element, Html, Node, Selector};
use serde::{Deserialize, Serialize};
use url::Url;

use super::dom::{
    collapse_inline_whitespace, collect_raw_text, is_element_hidden_or_inert, is_hidden_or_inert,
    WEBMCP_FORM_SEL,
};
use super::resources::escape_table_cell;
use super::{floor_char_boundary, longest_backtick_run};

pub const MAX_WEBMCP_TOOLS: usize = 32;
pub const MAX_SCHEMA_PROPERTIES: usize = 64;
pub const MAX_ENUM_VALUES: usize = 64;
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024;
pub const MAX_SCHEMA_DEPTH: usize = 8;

const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 1024;
const MAX_TOOL_TITLE_BYTES: usize = 256;
const MAX_PARAM_DESCRIPTION_BYTES: usize = 512;
const MAX_LABEL_DESCRIPTION_BYTES: usize = 256;
const MAX_ENUM_ITEM_BYTES: usize = 256;
const MAX_CUE_TOOL_NAMES: usize = 8;

static LABEL_FOR_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("label[for]").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebMcpToolSource {
    Declarative,
    Imperative,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebMcpToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autosubmit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted_content: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebMcpToolDescriptor {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub source: WebMcpToolSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<WebMcpToolAnnotations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

pub(crate) fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' | '\u{200E}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

pub(crate) fn is_stripped_control(ch: char) -> bool {
    ch < '\x20' || ch == '\x7f' || is_bidi_control(ch)
}

pub(crate) fn strip_controls_and_bidi(raw: &str) -> String {
    raw.chars().filter(|&ch| !is_stripped_control(ch)).collect()
}

fn sanitize_collapsed_text(raw: &str, max_bytes: usize) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if is_stripped_control(ch) {
            if matches!(ch, '\t' | '\n' | '\r') && !cleaned.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if ch.is_whitespace() {
            if !cleaned.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            cleaned.push(' ');
            pending_space = false;
        }
        cleaned.push(ch);
    }
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    let end = floor_char_boundary(&cleaned, max_bytes);
    cleaned[..end].trim_end().to_string()
}

fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

fn sanitize_url_without_secrets(mut url: Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_fragment(None);
    let rendered: String = url.into();
    if has_control_or_bidi(&rendered) {
        return None;
    }
    Some(rendered)
}

fn has_control_or_bidi(s: &str) -> bool {
    s.chars().any(is_stripped_control)
}

fn sanitize_stored_action_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || has_control_or_bidi(trimmed) {
        return None;
    }
    let parsed = Url::parse(trimmed).ok()?;
    sanitize_url_without_secrets(parsed)
}

fn resolve_form_action_url(
    raw_action: Option<&str>,
    page_url: &Url,
    base_url: &Url,
) -> Option<String> {
    let trimmed = raw_action.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        // Per HTML5, empty/absent action targets page_url directly, preventing <base href> hijack.
        return sanitize_url_without_secrets(page_url.clone());
    }
    if has_control_or_bidi(trimmed) {
        return None;
    }
    let resolved = base_url.join(trimmed).ok()?;
    if !crate::policy::is_safe_redirect_url(page_url, &resolved) {
        return None;
    }
    if page_url.scheme() == "https" && resolved.scheme() != "https" {
        return None;
    }
    sanitize_url_without_secrets(resolved)
}

fn normalize_form_method(raw_method: Option<&str>) -> Option<String> {
    match raw_method.map(str::trim) {
        None | Some("") => Some("GET".to_string()),
        Some(m) if m.eq_ignore_ascii_case("get") => Some("GET".to_string()),
        Some(m) if m.eq_ignore_ascii_case("post") => Some("POST".to_string()),
        Some(_) => None,
    }
}

fn sanitize_schema_value(
    value: serde_json::Value,
    depth: usize,
    in_enum: bool,
) -> Option<serde_json::Value> {
    if depth > MAX_SCHEMA_DEPTH {
        return None;
    }
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let clean_k = strip_controls_and_bidi(&k);
                if clean_k == "properties" {
                    if let serde_json::Value::Object(ref props) = v {
                        if props.len() > MAX_SCHEMA_PROPERTIES {
                            return None;
                        }
                    }
                }
                let child_in_enum = clean_k == "enum";
                if child_in_enum {
                    if let serde_json::Value::Array(ref items) = v {
                        if items.len() > MAX_ENUM_VALUES {
                            return None;
                        }
                    }
                }
                let clean_v = sanitize_schema_value(v, depth + 1, child_in_enum)?;
                out.insert(clean_k, clean_v);
            }
            Some(serde_json::Value::Object(out))
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let clean_item = sanitize_schema_value(item, depth + 1, in_enum)?;
                if in_enum {
                    if let serde_json::Value::String(ref s) = clean_item {
                        if s.len() > MAX_ENUM_ITEM_BYTES {
                            return None;
                        }
                    }
                }
                out.push(clean_item);
            }
            Some(serde_json::Value::Array(out))
        }
        serde_json::Value::String(s) => {
            Some(serde_json::Value::String(strip_controls_and_bidi(&s)))
        }
        other @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)) => Some(other),
    }
}

fn sanitize_input_schema(schema: serde_json::Value) -> Option<serde_json::Value> {
    if !schema.is_object() {
        return None;
    }
    let clean = sanitize_schema_value(schema, 1, false)?;
    let encoded = serde_json::to_string(&clean).ok()?;
    (encoded.len() <= MAX_SCHEMA_BYTES).then_some(clean)
}

pub fn sanitize_webmcp_tool(mut tool: WebMcpToolDescriptor) -> Option<WebMcpToolDescriptor> {
    if !is_valid_tool_name(&tool.name) {
        return None;
    }
    tool.description = sanitize_collapsed_text(&tool.description, MAX_TOOL_DESCRIPTION_BYTES);
    if tool.description.is_empty() {
        return None;
    }
    tool.title = tool.title.and_then(|t| {
        let clean = sanitize_collapsed_text(&t, MAX_TOOL_TITLE_BYTES);
        (!clean.is_empty()).then_some(clean)
    });
    tool.action_url = tool.action_url.and_then(|u| sanitize_stored_action_url(&u));
    tool.method = tool.method.and_then(|m| {
        let upper = m.trim().to_ascii_uppercase();
        matches!(upper.as_str(), "GET" | "POST").then_some(upper)
    });
    tool.input_schema = tool.input_schema.and_then(sanitize_input_schema);

    let mut annotations = tool.annotations.unwrap_or_default();
    annotations.untrusted_content = Some(true);
    tool.annotations = Some(annotations);
    Some(tool)
}

pub fn merge_webmcp_tools(
    declarative: Vec<WebMcpToolDescriptor>,
    imperative: Vec<WebMcpToolDescriptor>,
) -> Vec<WebMcpToolDescriptor> {
    let mut merged: Vec<WebMcpToolDescriptor> = Vec::new();
    for tool in declarative.into_iter().chain(imperative) {
        let Some(clean) = sanitize_webmcp_tool(tool) else {
            continue;
        };
        if let Some(existing) = merged.iter_mut().find(|t| t.name == clean.name) {
            if clean.source == WebMcpToolSource::Imperative {
                *existing = clean;
            }
        } else if merged.len() < MAX_WEBMCP_TOOLS {
            merged.push(clean);
        }
    }
    merged
}

pub(crate) fn extract_declarative_tools(
    doc: &Html,
    page_url: &Url,
    effective_base: &Url,
) -> Vec<WebMcpToolDescriptor> {
    let label_map = index_visible_labels(doc);
    let mut seen_names = HashSet::new();
    let mut tools = Vec::new();

    for form_el_ref in doc.select(&WEBMCP_FORM_SEL) {
        if tools.len() >= MAX_WEBMCP_TOOLS {
            break;
        }
        let form_node = *form_el_ref;
        if is_hidden_or_inert(form_node) {
            continue;
        }
        let form_el = form_el_ref.value();
        let Some(raw_name) = form_el.attr("toolname") else {
            continue;
        };
        if !is_valid_tool_name(raw_name) {
            continue;
        }
        let Some(raw_desc) = form_el.attr("tooldescription") else {
            continue;
        };

        let title = form_el
            .attr("tooltitle")
            .map(|t| sanitize_collapsed_text(t, MAX_TOOL_TITLE_BYTES))
            .filter(|t| !t.is_empty());
        let autosubmit = form_el.attr("toolautosubmit").is_some().then_some(true);
        let action_url = resolve_form_action_url(form_el.attr("action"), page_url, effective_base);
        let method = normalize_form_method(form_el.attr("method"));
        let input_schema = Some(synthesize_form_schema(form_node, &label_map));

        let candidate = WebMcpToolDescriptor {
            name: raw_name.to_string(),
            description: raw_desc.to_string(),
            title,
            source: WebMcpToolSource::Declarative,
            input_schema,
            annotations: Some(WebMcpToolAnnotations {
                read_only: None,
                autosubmit,
                untrusted_content: Some(true),
            }),
            action_url,
            method,
        };

        if let Some(clean) = sanitize_webmcp_tool(candidate) {
            if seen_names.insert(clean.name.clone()) {
                tools.push(clean);
            }
        }
    }

    tools
}

fn index_visible_labels(doc: &Html) -> HashMap<&str, String> {
    let mut map = HashMap::new();
    for label_ref in doc.select(&LABEL_FOR_SEL) {
        let label_node = *label_ref;
        if is_hidden_or_inert(label_node) {
            continue;
        }
        let Some(for_id) = label_ref.value().attr("for").filter(|id| !id.is_empty()) else {
            continue;
        };
        if map.contains_key(for_id) {
            continue;
        }
        let mut raw = String::new();
        collect_visible_text(label_node, &mut raw);
        let cleaned = sanitize_collapsed_text(&raw, MAX_LABEL_DESCRIPTION_BYTES);
        if !cleaned.is_empty() {
            map.insert(for_id, cleaned);
        }
    }
    map
}

fn collect_visible_text(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(t) => out.push_str(t),
        Node::Element(_) if is_hidden_or_inert(node) => {}
        _ => {
            for child in node.children() {
                collect_visible_text(child, out);
            }
        }
    }
}

fn is_control_disabled(control: NodeRef<'_, Node>, el: &Element, form_id: NodeId) -> bool {
    if el.attr("disabled").is_some() {
        return true;
    }
    control
        .ancestors()
        .take_while(|anc| anc.id() != form_id)
        .any(|anc| {
            matches!(
                anc.value(),
                Node::Element(e) if e.name() == "fieldset" && e.attr("disabled").is_some()
            )
        })
}

fn is_option_disabled(option: NodeRef<'_, Node>, el: &Element, select_id: NodeId) -> bool {
    if el.attr("disabled").is_some() || is_hidden_or_inert_within(option, select_id) {
        return true;
    }
    option
        .ancestors()
        .take_while(|anc| anc.id() != select_id)
        .any(|anc| {
            matches!(
                anc.value(),
                Node::Element(e) if e.name() == "optgroup" && e.attr("disabled").is_some()
            )
        })
}

fn is_hidden_or_inert_within(node: NodeRef<'_, Node>, ancestor_stop_id: NodeId) -> bool {
    std::iter::once(node)
        .chain(
            node.ancestors()
                .take_while(|anc| anc.id() != ancestor_stop_id),
        )
        .any(|n| matches!(n.value(), Node::Element(el) if is_element_hidden_or_inert(el)))
}

fn parse_json_number(raw: &str) -> Option<serde_json::Number> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(i) = trimmed.parse::<i64>() {
        return Some(serde_json::Number::from(i));
    }
    let val = trimmed.parse::<f64>().ok()?;
    if !val.is_finite() {
        return None;
    }
    if val == 0.0 {
        return Some(serde_json::Number::from(0));
    }
    if val.fract() == 0.0 && (-9_007_199_254_740_991.0..=9_007_199_254_740_991.0).contains(&val) {
        return Some(serde_json::Number::from(val as i64));
    }
    serde_json::Number::from_f64(val)
}

fn control_description(el: &Element, label_map: &HashMap<&str, String>) -> Option<String> {
    if let Some(raw) = el.attr("toolparamdescription") {
        let cleaned = sanitize_collapsed_text(raw, MAX_PARAM_DESCRIPTION_BYTES);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    el.attr("id")
        .filter(|id| !id.is_empty())
        .and_then(|id| label_map.get(id).cloned())
}

fn push_unique_string(vec: &mut Vec<String>, val: String) {
    if !vec.iter().any(|existing| existing == &val) {
        vec.push(val);
    }
}

fn synthesize_form_schema(
    form_node: NodeRef<'_, Node>,
    label_map: &HashMap<&str, String>,
) -> serde_json::Value {
    let form_id = form_node.id();
    let mut properties = serde_json::Map::new();
    let mut required: Vec<String> = Vec::new();

    for desc in form_node.descendants().skip(1) {
        let Node::Element(el) = desc.value() else {
            continue;
        };
        let tag = el.name();
        if !matches!(tag, "input" | "select" | "textarea") {
            continue;
        }
        if is_hidden_or_inert_within(desc, form_id) || is_control_disabled(desc, el, form_id) {
            continue;
        }
        let Some(raw_name) = el.attr("name").filter(|n| !n.is_empty() && n.len() <= 128) else {
            continue;
        };
        let name = strip_controls_and_bidi(raw_name);
        if name.is_empty() {
            continue;
        }

        let input_type = if tag == "input" {
            el.attr("type")
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "text".to_string())
        } else {
            String::new()
        };

        if tag == "input"
            && matches!(
                input_type.as_str(),
                "button" | "submit" | "reset" | "image" | "hidden" | "file" | "password"
            )
        {
            continue;
        }

        let desc_text = control_description(el, label_map);
        let is_required = el.attr("required").is_some();

        if tag == "input" && input_type == "radio" {
            let radio_value = strip_controls_and_bidi(el.attr("value").unwrap_or("on"));
            let entry = properties
                .entry(name.clone())
                .or_insert_with(|| serde_json::json!({ "type": "string", "enum": [] }));
            let obj = match entry.as_object_mut() {
                Some(obj) => obj,
                None => {
                    *entry = serde_json::json!({ "type": "string", "enum": [] });
                    entry.as_object_mut().expect("object inserted")
                }
            };
            if !obj.get("enum").is_some_and(serde_json::Value::is_array) {
                obj.insert("enum".to_string(), serde_json::json!([]));
            }
            if let Some(arr) = obj
                .get_mut("enum")
                .and_then(serde_json::Value::as_array_mut)
            {
                if !arr.iter().any(|v| v.as_str() == Some(radio_value.as_str())) {
                    arr.push(serde_json::Value::String(radio_value));
                }
            }
            if !obj.contains_key("description") {
                if let Some(d) = desc_text {
                    obj.insert("description".to_string(), serde_json::Value::String(d));
                }
            }
            if is_required {
                push_unique_string(&mut required, name);
            }
            continue;
        }

        let mut prop = serde_json::Map::new();
        if tag == "select" {
            let select_id = desc.id();
            let mut values = Vec::new();
            for opt_node in desc.descendants().skip(1) {
                let Node::Element(opt_el) = opt_node.value() else {
                    continue;
                };
                if opt_el.name() != "option" || is_option_disabled(opt_node, opt_el, select_id) {
                    continue;
                }
                let val = match opt_el.attr("value") {
                    Some(v) => strip_controls_and_bidi(v),
                    None => {
                        let mut raw = String::new();
                        collect_raw_text(opt_node, &mut raw);
                        strip_controls_and_bidi(collapse_inline_whitespace(&raw).trim())
                    }
                };
                push_unique_string(&mut values, val);
            }
            let mut item = serde_json::Map::new();
            item.insert(
                "type".to_string(),
                serde_json::Value::String("string".to_string()),
            );
            if !values.is_empty() {
                item.insert(
                    "enum".to_string(),
                    serde_json::Value::Array(
                        values.into_iter().map(serde_json::Value::String).collect(),
                    ),
                );
            }
            if el.attr("multiple").is_some() {
                prop.insert(
                    "type".to_string(),
                    serde_json::Value::String("array".to_string()),
                );
                prop.insert("items".to_string(), serde_json::Value::Object(item));
            } else {
                prop = item;
            }
        } else if tag == "input" && input_type == "checkbox" {
            prop.insert(
                "type".to_string(),
                serde_json::Value::String("boolean".to_string()),
            );
        } else if tag == "input" && matches!(input_type.as_str(), "number" | "range") {
            prop.insert(
                "type".to_string(),
                serde_json::Value::String("number".to_string()),
            );
            if let Some(min) = el.attr("min").and_then(parse_json_number) {
                prop.insert("minimum".to_string(), serde_json::Value::Number(min));
            }
            if let Some(max) = el.attr("max").and_then(parse_json_number) {
                prop.insert("maximum".to_string(), serde_json::Value::Number(max));
            }
        } else if tag == "input" && input_type == "email" {
            prop.insert(
                "type".to_string(),
                serde_json::Value::String("string".to_string()),
            );
            prop.insert(
                "format".to_string(),
                serde_json::Value::String("email".to_string()),
            );
        } else {
            prop.insert(
                "type".to_string(),
                serde_json::Value::String("string".to_string()),
            );
        }

        if let Some(d) = desc_text {
            prop.insert("description".to_string(), serde_json::Value::String(d));
        }
        properties.insert(name.clone(), serde_json::Value::Object(prop));
        if is_required {
            push_unique_string(&mut required, name);
        }
    }

    let mut schema = serde_json::Map::new();
    schema.insert(
        "type".to_string(),
        serde_json::Value::String("object".to_string()),
    );
    schema.insert(
        "properties".to_string(),
        serde_json::Value::Object(properties),
    );
    if !required.is_empty() {
        schema.insert(
            "required".to_string(),
            serde_json::Value::Array(
                required
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    serde_json::Value::Object(schema)
}

fn format_inline_code(raw: &str) -> String {
    let cleaned = strip_controls_and_bidi(raw);
    let ticks = "`".repeat((longest_backtick_run(&cleaned) + 1).max(1));
    if cleaned.starts_with('`') || cleaned.ends_with('`') {
        format!("{ticks} {cleaned} {ticks}")
    } else {
        format!("{ticks}{cleaned}{ticks}")
    }
}

pub fn format_webmcp_manifest_markdown(tools: &[WebMcpToolDescriptor]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let mut out = String::from(
        "## Page Tools (WebMCP)\n\n\
         | Name | Source | Method | Action | Title | Description |\n\
         | --- | --- | --- | --- | --- | --- |\n",
    );

    for tool in tools {
        let source = match tool.source {
            WebMcpToolSource::Declarative => "declarative",
            WebMcpToolSource::Imperative => "imperative",
        };
        let name = escape_table_cell(&strip_controls_and_bidi(&tool.name));
        let method = tool
            .method
            .as_deref()
            .map(strip_controls_and_bidi)
            .map(|s| escape_table_cell(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string());
        let action = tool
            .action_url
            .as_deref()
            .map(strip_controls_and_bidi)
            .map(|s| escape_table_cell(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string());
        let title = tool
            .title
            .as_deref()
            .map(strip_controls_and_bidi)
            .map(|s| escape_table_cell(&s))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "-".to_string());
        let description = escape_table_cell(&strip_controls_and_bidi(&tool.description));

        out.push_str(&format!(
            "| {name} | {source} | {method} | {action} | {title} | {description} |\n"
        ));
    }

    for tool in tools {
        let Some(schema) = &tool.input_schema else {
            continue;
        };
        let Ok(pretty_raw) = serde_json::to_string_pretty(schema) else {
            continue;
        };
        let pretty: String = pretty_raw
            .chars()
            .filter(|&ch| ch == '\n' || !is_stripped_control(ch))
            .collect();
        let fence = "`".repeat((longest_backtick_run(&pretty) + 1).max(3));
        out.push_str(&format!(
            "\n### {} schema\n\n{fence}json\n{pretty}\n{fence}\n",
            format_inline_code(&tool.name)
        ));
    }

    out.trim_end_matches('\n').to_string()
}

pub fn format_webmcp_cue(tools: &[WebMcpToolDescriptor]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let noun = if tools.len() == 1 { "tool" } else { "tools" };
    let mut names: Vec<String> = tools
        .iter()
        .take(MAX_CUE_TOOL_NAMES)
        .map(|t| strip_controls_and_bidi(&t.name))
        .collect();
    if tools.len() > MAX_CUE_TOOL_NAMES {
        names.push(format!("+{} more", tools.len() - MAX_CUE_TOOL_NAMES));
    }
    Some(format!(
        "Tools: {} WebMCP {noun} ({}) — pass --tools manifest or --json",
        tools.len(),
        names.join(", ")
    ))
}
