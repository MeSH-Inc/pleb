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
    let mut tags_to_skip = vec!["script", "style", "noscript", "template"];
    if !include_images {
        tags_to_skip.extend(["img", "picture", "source"]);
    }
    let mut builder = HtmlToMarkdown::builder().skip_tags(tags_to_skip);

    builder = builder.add_handler(vec!["svg"], |_handlers: &dyn Handlers, element: Element| {
        for attr in element.attrs.iter() {
            if attr.name.local.as_ref() == "aria-label" {
                let label = attr.value.trim();
                if !label.is_empty() {
                    return Some(format!("[Diagram: {label}]").into());
                }
            }
        }
        Some(String::new().into())
    });

    builder = builder.add_handler(
        vec!["math"],
        |_handlers: &dyn Handlers, element: Element| {
            let mut tex: Option<&str> = None;
            let mut display = false;
            for attr in element.attrs.iter() {
                let name: &str = attr.name.local.as_ref();
                if name == "data-tex" {
                    tex = Some(attr.value.trim());
                } else if name == "data-display" && attr.value.as_ref() == "1" {
                    display = true;
                }
            }
            let tex = tex.filter(|s| !s.is_empty())?;
            if display {
                Some(format!("\n\n$$\n{tex}\n$$\n\n").into())
            } else {
                Some(format!("${tex}$").into())
            }
        },
    );

    let anchor_base = base_url.clone();
    let seen_hrefs = std::sync::Mutex::new(std::collections::HashSet::<String>::new());
    builder = builder.add_handler(
        vec!["a"],
        move |handlers: &dyn Handlers, element: Element| {
            let href = element
                .attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "href")
                .map(|a| a.value.as_ref());

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
                    let wraps_block = leading_ws.contains("\n\n") || trailing_ws.contains("\n\n");
                    let already_linked = !seen_hrefs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(dest.clone());
                    let body = format_anchor_markdown(
                        core,
                        &dest,
                        &format_destination(&dest),
                        wraps_block,
                        already_linked,
                    );
                    Some(format!("{leading_ws}{body}{trailing_ws}").into())
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

pub(crate) fn split_atx_heading(trimmed_start: &str) -> Option<(u8, &str, &str)> {
    let hashes = trimmed_start.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) || !trimmed_start[hashes..].starts_with(' ') {
        return None;
    }
    let heading_text = trimmed_start[hashes + 1..].trim();
    (!heading_text.is_empty()).then_some((hashes as u8, &trimmed_start[..hashes + 1], heading_text))
}

fn format_anchor_markdown(
    core: &str,
    dest: &str,
    formatted_dest: &str,
    wraps_block: bool,
    already_linked: bool,
) -> String {
    let (first_line, rest) = core
        .split_once('\n')
        .map_or((core, None), |(f, r)| (f, Some(r)));
    if let Some((_, prefix, heading_text)) = split_atx_heading(first_line) {
        return match rest {
            Some(after) => format!("{prefix}[{heading_text}]({formatted_dest})\n{after}"),
            None => format!("{prefix}[{heading_text}]({formatted_dest})"),
        };
    }
    if core.contains("\n\n") || core.lines().any(|l| parse_opening_fence(l).is_some()) {
        if already_linked {
            core.to_string()
        } else {
            format!("{core}\n\n[{dest}]({formatted_dest})")
        }
    } else if already_linked && wraps_block {
        core.to_string()
    } else {
        format!("[{core}]({formatted_dest})")
    }
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

pub(crate) fn format_destination(dest: &str) -> String {
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

fn collapse_lines(raw: &str) -> String {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_title(title: Option<&str>) -> String {
    let Some(raw) = title else {
        return String::new();
    };
    let collapsed = collapse_lines(raw);
    if collapsed.is_empty() {
        return String::new();
    }
    let escaped = collapsed.replace('\\', "\\\\").replace('"', "\\\"");
    format!(" \"{escaped}\"")
}

fn clean_alt_text(raw: &str) -> String {
    let collapsed = collapse_lines(raw);
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
    let (out, truncated, _) = truncate_markdown_with_offset(md, max_bytes);
    (out, truncated)
}

pub(crate) fn advance_past_newline(text: &str, idx: usize) -> usize {
    if text.as_bytes().get(idx) == Some(&b'\n') {
        idx + 1
    } else {
        idx
    }
}

pub(crate) fn truncate_markdown_with_offset(md: &str, max_bytes: usize) -> (String, bool, usize) {
    if md.len() <= max_bytes {
        return (md.to_string(), false, md.len());
    }
    if max_bytes == 0 {
        return (String::new(), !md.is_empty(), 0);
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
            let (out, trunc) = rewind_before_fence(md, open_start, max_bytes, suffix);
            return (out, trunc, open_start);
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
            let (out, trunc) = rewind_before_fence(md, open_start, max_bytes, suffix);
            return (out, trunc, open_start);
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
        return (out, true, advance_past_newline(md, inner_cut));
    }

    let prefix = &md[..candidate_cut];
    let mut out = String::with_capacity(prefix.len() + suffix.len());
    out.push_str(prefix);
    out.push_str(suffix);
    debug_assert!(out.len() <= max_bytes);
    debug_assert!(open_fence_at(&out, out.len()).is_none());
    (out, true, advance_past_newline(md, candidate_cut))
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
/// - Collapse duplicate horizontal rules, trim leading/trailing ones, and dedup adjacent `![](u)` icons
/// - Trim trailing horizontal whitespace per line
pub fn compact_markdown(md: &str) -> String {
    let mut out_lines: Vec<String> = Vec::new();
    let mut fence = FenceTracker::default();
    let mut blank_run = 0usize;
    let mut last_was_hr = false;
    let mut last_heading: Option<String> = None;

    for raw_line in md.lines() {
        let step = fence.step(raw_line);
        if !matches!(step, FenceLine::Outside) {
            last_was_hr = false;
            blank_run = 0;
            last_heading = None;
            let kept = if matches!(step, FenceLine::Opening) {
                raw_line.trim_end()
            } else {
                raw_line
            };
            out_lines.push(kept.to_string());
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
        if is_empty_atx_heading(trimmed_end) {
            continue;
        }
        let allow_setext = !last_was_hr && out_lines.last().is_some_and(|p| !p.is_empty());
        if is_thematic_break(trimmed_end, allow_setext) {
            if out_lines.is_empty() || last_was_hr {
                continue;
            }
            last_was_hr = true;
            blank_run = 0;
            last_heading = None;
            out_lines.push(trimmed_end.to_string());
            continue;
        }
        last_was_hr = false;
        blank_run = 0;

        if split_atx_heading(trimmed_end).is_some() {
            let trimmed_h = trimmed_end.trim();
            if last_heading
                .as_deref()
                .is_some_and(|prev| prev.eq_ignore_ascii_case(trimmed_h))
            {
                continue;
            }
            last_heading = Some(trimmed_h.to_string());
        } else {
            last_heading = None;
        }

        let deduped = dedup_adjacent_empty_alt_images(trimmed_end);
        out_lines.push(compact_table_row(&deduped).unwrap_or_else(|| deduped.into_owned()));
    }

    while let Some(last) = out_lines.last() {
        if last.is_empty() || std::mem::take(&mut last_was_hr) {
            out_lines.pop();
        } else {
            break;
        }
    }

    out_lines.join("\n")
}

fn is_empty_atx_heading(line: &str) -> bool {
    let t = line.trim();
    let hashes = t.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && t[hashes..].trim().is_empty()
}

fn is_thematic_break(line: &str, allow_setext_dash: bool) -> bool {
    let t = line.trim();
    let Some(m) = t.bytes().find(|b| !b.is_ascii_whitespace()) else {
        return false;
    };
    matches!(m, b'*' | b'-' | b'_')
        && !(m == b'-' && allow_setext_dash)
        && t.bytes().all(|b| b == m || b.is_ascii_whitespace())
        && t.bytes().filter(|&b| b == m).count() >= 3
}

fn dedup_adjacent_empty_alt_images(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("![](") {
        return std::borrow::Cow::Borrowed(line);
    }
    let (mut out, mut i) = (String::with_capacity(line.len()), 0);
    while let Some(rel) = line[i..].find("![](") {
        let start = i + rel;
        out.push_str(&line[i..start]);
        let mut depth = 1usize;
        let Some(tok_end) = line[start + 4..].char_indices().find_map(|(k, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 1 => Some(start + 4 + k + 1),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        }) else {
            out.push_str("![](");
            i = start + 4;
            continue;
        };
        let tok = &line[start..tok_end];
        out.push_str(tok);
        i = tok_end;
        while let Some(rest) = line[i..].trim_start_matches([' ', '\t']).strip_prefix(tok) {
            i = line.len() - rest.len();
        }
    }
    out.push_str(&line[i..]);
    std::borrow::Cow::Owned(out)
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
            out.push_str(compact_separator_cell(cell_trimmed));
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
    let mut code_ticks: Option<usize> = None;
    let bytes = inner.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        let ch = inner[idx..].chars().next().unwrap();
        if escaped {
            escaped = false;
            idx += ch.len_utf8();
            continue;
        }
        if ch == '`' {
            let run = bytes[idx..].iter().take_while(|&&b| b == b'`').count();
            match code_ticks {
                None => code_ticks = Some(run),
                Some(open) if open == run => code_ticks = None,
                Some(_) => {}
            }
            idx += run;
            continue;
        }
        if code_ticks.is_none() {
            match ch {
                '\\' => escaped = true,
                '|' => {
                    cells.push(&inner[start..idx]);
                    start = idx + 1;
                }
                _ => {}
            }
        }
        idx += ch.len_utf8();
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

fn compact_separator_cell(cell: &str) -> &'static str {
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => ":---:",
        (true, false) => ":---",
        (false, true) => "---:",
        (false, false) => "---",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FenceLine {
    Outside,
    Opening,
    Inside,
    Closing,
}

#[derive(Default)]
pub(crate) struct FenceTracker(Option<(char, usize)>);

impl FenceTracker {
    pub(crate) fn step(&mut self, line: &str) -> FenceLine {
        if let Some((fc, fl)) = self.0 {
            if is_closing_fence(line, fc, fl) {
                self.0 = None;
                return FenceLine::Closing;
            }
            return FenceLine::Inside;
        }
        if let Some(f) = parse_opening_fence(line) {
            self.0 = Some(f);
            return FenceLine::Opening;
        }
        FenceLine::Outside
    }
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
