#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StagedArtifact {
    pub path: std::path::PathBuf,
    pub content_type: Option<String>,
    pub byte_count: usize,
}

pub fn stage_artifact(
    _state_dir: &std::path::Path,
    _source_url: &url::Url,
    _content_type: Option<&str>,
    _bytes: &[u8],
    _expected_kind: Option<crate::extract::ResourceKind>,
) -> anyhow::Result<StagedArtifact> {
    anyhow::bail!("unimplemented")
}
