use serde::{Deserialize, Serialize};

pub const MAX_WEBMCP_TOOLS: usize = 32;
pub const MAX_SCHEMA_PROPERTIES: usize = 64;
pub const MAX_ENUM_VALUES: usize = 64;
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024;
pub const MAX_SCHEMA_DEPTH: usize = 8;

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

pub fn sanitize_webmcp_tool(mut tool: WebMcpToolDescriptor) -> Option<WebMcpToolDescriptor> {
    if tool.name.is_empty() || tool.name.len() > 128 {
        return None;
    }
    let mut annotations = tool.annotations.unwrap_or_default();
    annotations.untrusted_content = Some(true);
    tool.annotations = Some(annotations);
    Some(tool)
}

pub fn merge_webmcp_tools(
    declarative: Vec<WebMcpToolDescriptor>,
    imperative: Vec<WebMcpToolDescriptor>,
) -> Vec<WebMcpToolDescriptor> {
    let mut merged = Vec::new();
    for tool in declarative.into_iter().chain(imperative) {
        if merged.len() >= MAX_WEBMCP_TOOLS {
            break;
        }
        if let Some(clean) = sanitize_webmcp_tool(tool) {
            if let Some(existing) = merged
                .iter_mut()
                .find(|t: &&mut WebMcpToolDescriptor| t.name == clean.name)
            {
                if clean.source == WebMcpToolSource::Imperative {
                    *existing = clean;
                }
            } else {
                merged.push(clean);
            }
        }
    }
    merged
}

pub fn format_webmcp_manifest_markdown(_tools: &[WebMcpToolDescriptor]) -> String {
    String::new()
}

pub fn format_webmcp_cue(_tools: &[WebMcpToolDescriptor]) -> Option<String> {
    None
}
