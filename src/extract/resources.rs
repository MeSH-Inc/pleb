use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceSalience {
    High,
    Medium,
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

pub fn format_resource_manifest_markdown(_resources: &[ResourceDescriptor]) -> String {
    String::new()
}
