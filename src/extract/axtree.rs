use anyhow::{bail, Result};

/// Format a CDP `Accessibility.getFullAXTree` JSON response into a compact,
/// indentation-delimited text tree safe for agent context windows.
pub fn format_axtree_from_cdp_json(_raw_json: &str) -> Result<String> {
    bail!("Accessibility.getFullAXTree formatting not yet implemented")
}
