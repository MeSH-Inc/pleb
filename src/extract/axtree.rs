use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::webmcp::is_stripped_control;

const MAX_AX_DEPTH: usize = 512;

#[derive(Debug, Deserialize)]
struct AxTreeEnvelope {
    #[serde(default)]
    nodes: Option<Vec<AxNode>>,
    #[serde(default)]
    result: Option<AxTreeResult>,
}

#[derive(Debug, Deserialize)]
struct AxTreeResult {
    #[serde(default)]
    nodes: Vec<AxNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxNode {
    node_id: String,
    #[serde(default)]
    ignored: bool,
    #[serde(default)]
    role: Option<AxValue>,
    #[serde(default)]
    name: Option<AxValue>,
    #[serde(default)]
    description: Option<AxValue>,
    #[serde(default)]
    value: Option<AxValue>,
    #[serde(default)]
    properties: Option<Vec<AxProperty>>,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default)]
    child_ids: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxValue {
    #[serde(rename = "type", default)]
    _value_type: Option<String>,
    #[serde(default)]
    value: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AxProperty {
    name: String,
    value: AxValue,
}

/// Format a CDP `Accessibility.getFullAXTree` JSON response into a compact,
/// indentation-delimited text tree safe for agent context windows.
pub fn format_axtree_from_cdp_json(raw_json: &str) -> Result<String> {
    let envelope: AxTreeEnvelope =
        serde_json::from_str(raw_json).context("invalid CDP Accessibility.getFullAXTree JSON")?;
    let nodes = match (envelope.nodes, envelope.result) {
        (Some(nodes), _) => nodes,
        (None, Some(res)) => res.nodes,
        (None, None) => bail!("CDP Accessibility.getFullAXTree payload missing 'nodes'"),
    };

    if nodes.is_empty() {
        return Ok(String::new());
    }

    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(nodes.len());
    for (idx, node) in nodes.iter().enumerate() {
        by_id.entry(node.node_id.as_str()).or_insert(idx);
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    let mut has_parent_link = vec![false; nodes.len()];

    for (idx, node) in nodes.iter().enumerate() {
        if let Some(child_ids) = &node.child_ids {
            for cid in child_ids {
                if let Some(&child_idx) = by_id.get(cid.as_str()) {
                    if child_idx != idx && !has_parent_link[child_idx] {
                        children[idx].push(child_idx);
                        has_parent_link[child_idx] = true;
                    }
                }
            }
        }
    }
    for (idx, node) in nodes.iter().enumerate() {
        if !has_parent_link[idx] {
            if let Some(pid) = node.parent_id.as_deref() {
                if let Some(&parent_idx) = by_id.get(pid) {
                    if parent_idx != idx {
                        children[parent_idx].push(idx);
                        has_parent_link[idx] = true;
                    }
                }
            }
        }
    }

    let mut roots: Vec<usize> = (0..nodes.len())
        .filter(|&idx| {
            !has_parent_link[idx]
                && nodes[idx]
                    .parent_id
                    .as_deref()
                    .is_none_or(|pid| !by_id.contains_key(pid))
        })
        .collect();
    if roots.is_empty() {
        roots.push(0);
    }

    let mut lines: Vec<String> = Vec::new();
    let mut visited = HashSet::with_capacity(nodes.len());
    let mut stack: Vec<(usize, usize)> = Vec::new();

    for root in roots.into_iter().rev() {
        stack.push((root, 0));
    }

    while let Some((idx, depth)) = stack.pop() {
        if depth > MAX_AX_DEPTH || !visited.insert(idx) {
            continue;
        }
        let node = &nodes[idx];
        let role = extract_ax_role(node);
        let skip = node.ignored
            || match role.as_deref() {
                None | Some("") => true,
                Some(r) => is_skipped_ax_role(r),
            };

        let child_depth = if skip {
            depth
        } else {
            if let Some(line) = format_ax_line(node, role.as_deref().unwrap_or(""), depth) {
                lines.push(line);
            }
            depth + 1
        };

        for &child_idx in children[idx].iter().rev() {
            if !visited.contains(&child_idx) {
                stack.push((child_idx, child_depth));
            }
        }
    }

    Ok(lines.join("\n"))
}

fn is_skipped_ax_role(role: &str) -> bool {
    matches!(
        role,
        "none" | "Ignored" | "InlineTextBox" | "generic" | "GenericContainer"
    )
}

fn extract_ax_role(node: &AxNode) -> Option<String> {
    node.role
        .as_ref()
        .and_then(|ax| ax.value.as_ref())
        .and_then(|v| v.as_str())
        .map(escape_ax_text)
}

fn extract_ax_string(ax: Option<&AxValue>) -> Option<String> {
    ax.and_then(|v| v.value.as_ref())
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(escape_ax_text)
}

fn escape_ax_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c if is_stripped_control(c) => {}
            c => out.push(c),
        }
    }
    out
}

fn format_ax_json_value(val: &serde_json::Value, prop_name: &str) -> Option<String> {
    match val {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::String(s) => {
            if matches!(prop_name, "checked" | "pressed")
                && matches!(s.as_str(), "true" | "false" | "mixed")
            {
                Some(s.clone())
            } else {
                Some(format!("\"{}\"", escape_ax_text(s)))
            }
        }
        other => Some(format!("\"{}\"", escape_ax_text(&other.to_string()))),
    }
}

fn format_ax_line(node: &AxNode, role: &str, depth: usize) -> Option<String> {
    if role.is_empty() {
        return None;
    }

    let mut line = String::with_capacity(depth * 2 + 64);
    for _ in 0..depth {
        line.push_str("  ");
    }
    line.push_str(role);

    if let Some(name) = extract_ax_string(node.name.as_ref()) {
        if !name.is_empty() {
            line.push_str(" \"");
            line.push_str(&name);
            line.push('"');
        }
    }

    if let Some(val) = node.value.as_ref().and_then(|v| v.value.as_ref()) {
        let should_emit = match val {
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Null => false,
            _ => true,
        };
        if should_emit {
            if let Some(formatted) = format_ax_json_value(val, "value") {
                line.push_str(" value=");
                line.push_str(&formatted);
            }
        }
    }

    if let Some(desc) = extract_ax_string(node.description.as_ref()) {
        if !desc.is_empty() {
            line.push_str(" description=\"");
            line.push_str(&desc);
            line.push('"');
        }
    }

    if let Some(props) = &node.properties {
        for prop in props {
            let key = escape_ax_text(&prop.name);
            if key.is_empty() {
                continue;
            }
            let Some(val) = prop.value.value.as_ref() else {
                continue;
            };
            let Some(formatted) = format_ax_json_value(val, &key) else {
                continue;
            };
            line.push(' ');
            line.push_str(&key);
            line.push('=');
            line.push_str(&formatted);
        }
    }

    Some(line)
}
