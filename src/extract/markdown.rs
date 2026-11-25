use anyhow::Result;
use htmd::{element_handler::Handlers, Element, HtmlToMarkdown};

pub fn html_to_compact_markdown(
    html: &str,
    include_images: bool,
    include_links: bool,
) -> Result<String> {
    let mut tags_to_skip = vec!["script", "style", "noscript", "template", "svg"];
    if !include_images {
        tags_to_skip.extend(["img", "picture", "source"]);
    }
    let mut builder = HtmlToMarkdown::builder().skip_tags(tags_to_skip);

    if !include_links {
        builder = builder.add_handler(vec!["a"], |handlers: &dyn Handlers, element: Element| {
            Some(handlers.walk_children(element.node))
        });
    }

    let raw = builder.build().convert(html)?;
    Ok(compact_markdown(&raw))
}

/// Compact bloated markdown outside fenced code blocks:
/// - Remove column-width padding inside pipe tables
/// - Normalize table separator cells (`------` -> `---`)
/// - Collapse 3+ consecutive newlines down to 2 (one blank line)
/// - Trim trailing horizontal whitespace per line
pub fn compact_markdown(md: &str) -> String {
    let mut out_lines: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut blank_run = 0usize;

    for raw_line in md.lines() {
        if let Some((fence_char, fence_len)) = fence {
            out_lines.push(raw_line.to_string());
            if is_closing_fence(raw_line, fence_char, fence_len) {
                fence = None;
            }
            blank_run = 0;
            continue;
        }

        if let Some(f) = parse_opening_fence(raw_line) {
            fence = Some(f);
            out_lines.push(raw_line.trim_end().to_string());
            blank_run = 0;
            continue;
        }

        let trimmed_end = raw_line.trim_end();
        if trimmed_end.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out_lines.is_empty() {
                out_lines.push(String::new());
            }
            continue;
        }
        blank_run = 0;

        let line = if let Some(compacted_row) = compact_table_row(trimmed_end) {
            compacted_row
        } else {
            trimmed_end.to_string()
        };
        out_lines.push(line);
    }

    while out_lines.last().is_some_and(String::is_empty) {
        out_lines.pop();
    }

    out_lines.join("\n")
}

fn compact_table_row(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !(trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() >= 2) {
        return None;
    }

    // Split interior between outer pipes without breaking escaped `\|`.
    let inner = &trimmed[1..trimmed.len() - 1];
    let cells = split_table_cells(inner);
    if cells.is_empty() {
        return None;
    }

    let is_sep = cells.iter().all(|c| is_separator_cell(c));
    let mut out = String::with_capacity(trimmed.len() / 2 + 4);
    out.push('|');
    for cell in cells {
        let cell_trimmed = cell.trim();
        out.push(' ');
        if is_sep {
            out.push_str(&compact_separator_cell(cell_trimmed));
        } else {
            out.push_str(&collapse_spaces(cell_trimmed));
        }
        out.push_str(" |");
    }
    Some(out)
}

fn split_table_cells(inner: &str) -> Vec<&str> {
    let mut cells = Vec::new();
    let mut start = 0usize;
    let bytes = inner.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            cells.push(&inner[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    cells.push(&inner[start..]);
    cells
}

fn is_separator_cell(cell: &str) -> bool {
    let t = cell.trim();
    if t.is_empty() {
        return false;
    }
    let core = t.trim_start_matches(':').trim_end_matches(':');
    !core.is_empty() && core.chars().all(|c| c == '-')
}

fn compact_separator_cell(cell: &str) -> String {
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    match (left, right) {
        (true, true) => ":---:".to_string(),
        (true, false) => ":---".to_string(),
        (false, true) => "---:".to_string(),
        (false, false) => "---".to_string(),
    }
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        if ch == ' ' || ch == '\t' {
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

fn parse_opening_fence(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|&c| c == first).count();
    if count >= 3 {
        Some((first, count))
    } else {
        None
    }
}

fn is_closing_fence(line: &str, fence_char: char, min_len: usize) -> bool {
    let trimmed = line.trim();
    if trimmed.len() < min_len {
        return false;
    }
    trimmed.chars().all(|c| c == fence_char)
}
