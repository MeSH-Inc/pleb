use anyhow::Result;
use htmd::{element_handler::Handlers, Element, HtmlToMarkdown};
use url::Url;

use super::url::{resolve_and_clean_url, ResolvedUrl};
use super::{choose_boundary_cut, floor_char_boundary, TRUNCATED_SUFFIX};

pub fn html_to_compact_markdown(
    html: &str,
    base_url: &Url,
    include_images: bool,
    include_links: bool,
) -> Result<String> {
    let mut tags_to_skip = vec!["script", "style", "noscript", "template", "svg"];
    if !include_images {
        tags_to_skip.extend(["img", "picture", "source"]);
    }
    let mut builder = HtmlToMarkdown::builder().skip_tags(tags_to_skip);

    let anchor_base = base_url.clone();
    builder = builder.add_handler(
        vec!["a"],
        move |handlers: &dyn Handlers, element: Element| {
            let mut href: Option<&str> = None;
            let mut title: Option<&str> = None;
            for attr in element.attrs.iter() {
                let name: &str = attr.name.local.as_ref();
                if name == "href" {
                    href = Some(&attr.value);
                } else if name == "title" {
                    title = Some(&attr.value);
                }
            }

            let inner = handlers.walk_children(element.node).content;
            let (leading_ws, core, trailing_ws) = split_document_whitespace(&inner);
            if core.is_empty() {
                return Some(String::new().into());
            }

            if !include_links {
                return Some(inner.into());
            }

            let Some(raw_href) = href else {
                return Some(inner.into());
            };

            match resolve_and_clean_url(raw_href, &anchor_base) {
                ResolvedUrl::Drop | ResolvedUrl::UnwrapText => Some(inner.into()),
                ResolvedUrl::Resolved(dest) => {
                    let formatted_dest = format_destination(&dest);
                    let formatted_title = format_title(title);
                    let out = format!(
                        "{leading_ws}[{core}]({formatted_dest}{formatted_title}){trailing_ws}"
                    );
                    Some(out.into())
                }
            }
        },
    );

    if include_images {
        let img_base = base_url.clone();
        builder = builder.add_handler(
            vec!["img"],
            move |_handlers: &dyn Handlers, element: Element| {
                let mut src: Option<&str> = None;
                let mut alt: Option<&str> = None;
                let mut title: Option<&str> = None;
                for attr in element.attrs.iter() {
                    let name: &str = attr.name.local.as_ref();
                    if name == "src" {
                        src = Some(&attr.value);
                    } else if name == "alt" {
                        alt = Some(&attr.value);
                    } else if name == "title" {
                        title = Some(&attr.value);
                    }
                }

                let raw_src = src?;
                match resolve_and_clean_url(raw_src, &img_base) {
                    ResolvedUrl::Resolved(dest) => {
                        let clean_alt = alt.map(clean_alt_text).unwrap_or_default();
                        let formatted_dest = format_destination(&dest);
                        let formatted_title = format_title(title);
                        Some(format!("![{clean_alt}]({formatted_dest}{formatted_title})").into())
                    }
                    ResolvedUrl::Drop | ResolvedUrl::UnwrapText => Some(String::new().into()),
                }
            },
        );
    }

    let raw = builder.build().convert(html)?;
    Ok(compact_markdown(&raw))
}

fn split_document_whitespace(text: &str) -> (&str, &str, &str) {
    let trimmed_start = text.trim_start_matches(['\t', '\n', '\r', ' ']);
    let leading_ws = &text[..text.len() - trimmed_start.len()];
    let core = trimmed_start.trim_end_matches(['\t', '\n', '\r', ' ']);
    let trailing_ws = &trimmed_start[core.len()..];
    (leading_ws, core, trailing_ws)
}

fn has_unbalanced_parens(s: &str) -> bool {
    let mut depth: i32 = 0;
    for ch in s.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    depth != 0
}

fn format_destination(dest: &str) -> String {
    if dest
        .chars()
        .any(|c| c.is_ascii_whitespace() || c.is_ascii_control() || c == '<' || c == '>')
    {
        let mut out = String::with_capacity(dest.len() + 4);
        out.push('<');
        for ch in dest.chars() {
            match ch {
                '\\' | '<' | '>' => {
                    out.push('\\');
                    out.push(ch);
                }
                _ => out.push(ch),
            }
        }
        out.push('>');
        return out;
    }

    if has_unbalanced_parens(dest) {
        let mut out = String::with_capacity(dest.len() + 4);
        for ch in dest.chars() {
            match ch {
                '(' => out.push_str("\\("),
                ')' => out.push_str("\\)"),
                _ => out.push(ch),
            }
        }
        out
    } else {
        dest.to_string()
    }
}

fn format_title(title: Option<&str>) -> String {
    let Some(raw) = title else {
        return String::new();
    };
    let collapsed: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.is_empty() {
        return String::new();
    }
    let escaped = collapsed.replace('\\', "\\\\").replace('"', "\\\"");
    format!(" \"{escaped}\"")
}

fn clean_alt_text(raw: &str) -> String {
    let collapsed: String = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::with_capacity(collapsed.len());
    for ch in collapsed.chars() {
        match ch {
            '[' | ']' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Truncate Markdown cleanly within `max_bytes`, closing any open fenced code block
/// (or rewinding past its opening fence if no code line fits inside the budget).
pub fn truncate_markdown(md: &str, max_bytes: usize) -> (String, bool) {
    if md.len() <= max_bytes {
        return (md.to_string(), false);
    }
    if max_bytes == 0 {
        return (String::new(), !md.is_empty());
    }

    let suffix = if max_bytes <= TRUNCATED_SUFFIX.len() {
        ""
    } else {
        TRUNCATED_SUFFIX
    };
    let raw_limit = max_bytes - suffix.len();
    let candidate_cut = if suffix.is_empty() {
        floor_char_boundary(md, raw_limit)
    } else {
        choose_boundary_cut(md, raw_limit)
    };

    if let Some((fc, fl, open_start, open_line_end)) =
        open_fence_at(&md[..candidate_cut], candidate_cut)
    {
        let reserve = 1 + fl + suffix.len();
        let fence_budget = max_bytes.saturating_sub(reserve);
        if candidate_cut <= open_line_end || fence_budget <= open_line_end {
            return rewind_before_fence(md, open_start, max_bytes, suffix);
        }

        let inner_cut = if suffix.is_empty() {
            floor_char_boundary(md, fence_budget)
        } else {
            let cut = choose_boundary_cut(md, fence_budget);
            if cut < open_line_end {
                floor_char_boundary(md, fence_budget)
            } else {
                cut
            }
        };
        let prefix = md[..inner_cut].trim_end_matches('\n');
        if prefix.len() < open_line_end.saturating_sub(1) || prefix.len() <= open_start {
            return rewind_before_fence(md, open_start, max_bytes, suffix);
        }

        let mut out = String::with_capacity(prefix.len() + 1 + fl + suffix.len());
        out.push_str(prefix);
        if open_fence_at(prefix, prefix.len()).is_some() {
            out.push('\n');
            for _ in 0..fl {
                out.push(fc);
            }
        }
        out.push_str(suffix);
        debug_assert!(out.len() <= max_bytes);
        debug_assert!(open_fence_at(&out, out.len()).is_none());
        return (out, true);
    }

    let prefix = &md[..candidate_cut];
    let mut out = String::with_capacity(prefix.len() + suffix.len());
    out.push_str(prefix);
    out.push_str(suffix);
    debug_assert!(out.len() <= max_bytes);
    debug_assert!(open_fence_at(&out, out.len()).is_none());
    (out, true)
}

fn rewind_before_fence(
    md: &str,
    open_start: usize,
    max_bytes: usize,
    suffix: &str,
) -> (String, bool) {
    let clean = md[..open_start].trim_end_matches(['\n', ' ']);
    if clean.is_empty() {
        if suffix.is_empty() {
            return (String::new(), true);
        }
        let trimmed_suffix = suffix.trim_start_matches('\n');
        let end = floor_char_boundary(trimmed_suffix, max_bytes);
        return (trimmed_suffix[..end].to_string(), true);
    }
    let mut out = String::with_capacity(clean.len() + suffix.len());
    out.push_str(clean);
    out.push_str(suffix);
    debug_assert!(out.len() <= max_bytes);
    debug_assert!(open_fence_at(&out, out.len()).is_none());
    (out, true)
}

/// Returns `Some((fence_char, fence_len, open_line_start, open_line_end))` if `cut`
/// lies inside an unclosed fenced code block.
pub fn open_fence_at(md: &str, cut: usize) -> Option<(char, usize, usize, usize)> {
    let mut fence: Option<(char, usize, usize, usize)> = None;
    let mut offset = 0usize;

    for raw_line in md.split_inclusive('\n') {
        let line_start = offset;
        let line_end = offset + raw_line.len();
        offset = line_end;

        if line_start >= cut {
            break;
        }

        let line = raw_line.trim_end_matches(['\r', '\n']);
        match fence {
            None => {
                if let Some((fc, fl)) = parse_opening_fence(line) {
                    fence = Some((fc, fl, line_start, line_end));
                }
            }
            Some((fc, fl, _, _)) => {
                if is_closing_fence(line, fc, fl) && line_end <= cut {
                    fence = None;
                }
            }
        }
    }

    fence
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
    let mut escaped = false;
    for (idx, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '|' => {
                cells.push(&inner[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
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
