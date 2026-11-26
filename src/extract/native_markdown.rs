use anyhow::Result;
use std::collections::HashSet;
use url::Url;

use super::markdown::{
    compact_markdown, format_destination, truncate_markdown, FenceLine, FenceTracker,
};
use super::url::{resolve_and_clean_url, ResolvedUrl};
use super::{
    apply_utf8_limit, floor_char_boundary, starts_with_title_heading, ExtractMode, ExtractOptions,
    ExtractedDocument, OutputFormat,
};

const MAX_FRONTMATTER_BYTES: usize = 4096;

pub fn is_markdown_mime(mime: Option<&str>) -> bool {
    matches!(mime, Some("text/markdown" | "text/x-markdown"))
}

pub fn extract_native_markdown(
    body: &str,
    page_url: &Url,
    options: &ExtractOptions,
) -> Result<ExtractedDocument> {
    if options.format == OutputFormat::Html {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
            resources: Vec::new(),
            tools: Vec::new(),
        });
    }

    let frontmatter = parse_leading_frontmatter(body);
    let preserve_frontmatter = options.mode == ExtractMode::Full && frontmatter.is_some();

    let (frontmatter_prefix, markdown_body) = match &frontmatter {
        Some(fm) if preserve_frontmatter => (&body[..fm.end_offset], &body[fm.end_offset..]),
        Some(fm) => ("", body[fm.end_offset..].trim_start_matches(['\r', '\n'])),
        None => ("", body),
    };

    let frontmatter_title = frontmatter.as_ref().and_then(|fm| fm.title.clone());
    let first_h1 = first_markdown_h1(markdown_body);
    let title = frontmatter_title.clone().or_else(|| first_h1.clone());

    let rewritten_body = rewrite_markdown_links(
        markdown_body,
        page_url,
        options.include_images,
        options.include_links,
        options.mode != ExtractMode::Full,
    );

    match options.format {
        OutputFormat::Markdown => {
            let compacted_body = compact_markdown(&rewritten_body);
            let combined = if preserve_frontmatter {
                if compacted_body.is_empty() {
                    frontmatter_prefix.trim_end().to_string()
                } else {
                    format!(
                        "{}\n\n{}",
                        frontmatter_prefix.trim_end_matches(['\r', '\n']),
                        compacted_body
                    )
                }
            } else {
                let trimmed = compacted_body.trim();
                let has_matching_h1 = |t: &str| {
                    starts_with_title_heading(trimmed, t)
                        || first_h1
                            .as_deref()
                            .is_some_and(|h| h.trim().eq_ignore_ascii_case(t.trim()))
                };
                match &frontmatter_title {
                    Some(t) if !has_matching_h1(t) => {
                        if trimmed.is_empty() {
                            format!("# {t}")
                        } else {
                            format!("# {t}\n\n{trimmed}")
                        }
                    }
                    _ => trimmed.to_string(),
                }
            };

            let (content, truncated) = match options.max_bytes {
                Some(max) => truncate_markdown(&combined, max),
                None => (combined, false),
            };
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
                resources: Vec::new(),
                tools: Vec::new(),
            })
        }
        OutputFormat::Text => {
            let plain_body = markdown_to_plain_text(&rewritten_body);
            let combined = if preserve_frontmatter {
                let fm = frontmatter_prefix.trim_end_matches(['\r', '\n']);
                if plain_body.is_empty() {
                    fm.to_string()
                } else {
                    format!("{fm}\n\n{plain_body}")
                }
            } else {
                plain_body
            };
            let (content, truncated) = apply_utf8_limit(&combined, options.max_bytes);
            Ok(ExtractedDocument {
                title,
                content,
                truncated,
                resources: Vec::new(),
                tools: Vec::new(),
            })
        }
        OutputFormat::Html | OutputFormat::Raw | OutputFormat::Axtree => unreachable!(),
    }
}

struct Frontmatter {
    title: Option<String>,
    end_offset: usize,
}

/// Parse leading YAML frontmatter strictly at byte 0 (`---\n...\n---` or `---\r\n...\r\n---`),
/// capped to the first 4 KiB so mid-document `---` horizontal rules are never misclassified.
fn parse_leading_frontmatter(body: &str) -> Option<Frontmatter> {
    let content_start = if body.starts_with("---\n") {
        4
    } else if body.starts_with("---\r\n") {
        5
    } else {
        return None;
    };

    let cap = floor_char_boundary(body, MAX_FRONTMATTER_BYTES);
    if content_start > cap {
        return None;
    }

    let search_slice = &body[content_start..cap];
    let mut offset = content_start;
    let mut close_start = None;
    let mut end_offset = None;

    for raw_line in search_slice.split_inclusive('\n') {
        let line = raw_line.trim_end_matches(['\r', '\n']);
        if line.trim_end_matches([' ', '\t']) == "---" {
            close_start = Some(offset);
            end_offset = Some(offset + raw_line.len());
            break;
        }
        offset += raw_line.len();
    }

    let close_start = close_start?;
    let end_offset = end_offset?;
    let yaml_block = &body[content_start..close_start];
    let title = extract_yaml_title(yaml_block);

    Some(Frontmatter { title, end_offset })
}

fn extract_yaml_title(yaml: &str) -> Option<String> {
    for raw_line in yaml.lines() {
        // Only match top-level `title:` keys (column 0).
        if raw_line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, rest)) = raw_line.split_once(':') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("title") {
            continue;
        }
        let val = rest.trim();
        if val.is_empty() || val == "|" || val == ">" {
            continue;
        }
        let unquoted = unquote_yaml_scalar(val);
        let trimmed = unquoted.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

fn unquote_yaml_scalar(val: &str) -> String {
    if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        let inner = &val[1..val.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    match next {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        other => {
                            out.push('\\');
                            out.push(other);
                        }
                    }
                } else {
                    out.push('\\');
                }
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    if val.len() >= 2 && val.starts_with('\'') && val.ends_with('\'') {
        return val[1..val.len() - 1].replace("''", "'");
    }
    val.to_string()
}

fn first_markdown_h1(md: &str) -> Option<String> {
    let mut fence = FenceTracker::default();
    let mut prev_para: Option<&str> = None;
    for raw_line in md.lines() {
        if fence.step(raw_line) != FenceLine::Outside {
            prev_para = None;
            continue;
        }
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            prev_para = None;
            continue;
        }
        if let Some(prev) =
            prev_para.filter(|_| trimmed.len() >= 2 && trimmed.bytes().all(|b| b == b'='))
        {
            return Some(prev.to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let heading = strip_trailing_atx_hashes(rest.trim());
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
        if let Some(html_h1) = extract_inline_html_h1(trimmed) {
            return Some(html_h1);
        }
        let non_para = trimmed.starts_with(['#', '>', '<', '-'])
            || trimmed.starts_with("* ")
            || trimmed.starts_with("+ ");
        prev_para = (!non_para).then_some(trimmed);
    }
    None
}

fn extract_inline_html_h1(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let start = lower.find("<h1")?;
    matches!(lower.as_bytes().get(start + 3), Some(b'>' | b' ' | b'\t')).then_some(())?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</h1>")?;
    let (mut text, mut in_tag) = (String::new(), false);
    for ch in line[open_end..close].chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

fn strip_trailing_atx_hashes(s: &str) -> &str {
    let trimmed = s.trim_end();
    let without_hashes = trimmed.trim_end_matches('#');
    if without_hashes.len() < trimmed.len()
        && (without_hashes.is_empty() || without_hashes.ends_with([' ', '\t']))
    {
        without_hashes.trim_end()
    } else {
        trimmed
    }
}

fn rewrite_markdown_links(
    md: &str,
    base_url: &Url,
    include_images: bool,
    include_links: bool,
    strip_skip_lines: bool,
) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence = FenceTracker::default();

    for raw_line in md.split_inclusive('\n') {
        let (line, newline) = if let Some(stripped) = raw_line.strip_suffix("\r\n") {
            (stripped, "\r\n")
        } else if let Some(stripped) = raw_line.strip_suffix('\n') {
            (stripped, "\n")
        } else {
            (raw_line, "")
        };

        if fence.step(line) != FenceLine::Outside {
            out.push_str(line);
            out.push_str(newline);
            continue;
        }

        if strip_skip_lines && is_standalone_markdown_skip_link(line) {
            continue;
        }

        if let Some(rewritten_def) =
            rewrite_link_ref_definition(line, base_url, include_images, include_links)
        {
            if !rewritten_def.is_empty() {
                out.push_str(&rewritten_def);
                out.push_str(newline);
            }
            continue;
        }

        rewrite_inline_segment(line, base_url, include_images, include_links, &mut out);
        out.push_str(newline);
    }

    out
}

fn is_standalone_markdown_skip_link(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('[') || !t.ends_with(')') {
        return false;
    }
    let Some(close) = find_matching_bracket(t, 0) else {
        return false;
    };
    let label = t[1..close].trim();
    let Some(dest) = t
        .get(close + 1..)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
        .map(str::trim)
    else {
        return false;
    };
    label.len() <= 48
        && label.to_ascii_lowercase().starts_with("skip to ")
        && dest.starts_with('#')
        && dest.len() > 1
        && !dest.contains(|c: char| c.is_ascii_whitespace())
}

struct LinkRefDef<'a> {
    indent: &'a str,
    label: &'a str,
    dest: &'a str,
    title: &'a str,
}

fn parse_link_ref_definition(line: &str) -> Option<LinkRefDef<'_>> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 || line.as_bytes().get(indent) != Some(&b'[') {
        return None;
    }
    let close = find_matching_bracket(line, indent)?;
    let label = &line[indent + 1..close];
    let rest = line
        .get(close + 1..)?
        .strip_prefix(':')?
        .trim_start_matches([' ', '\t']);
    if label.trim().is_empty() || label.starts_with('^') || rest.is_empty() {
        return None;
    }
    let (dest, after_dest, angle) = if let Some(inner) = rest.strip_prefix('<') {
        let end = inner.find('>')?;
        (&inner[..end], &inner[end + 1..], true)
    } else {
        let end = rest
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        (&rest[..end], &rest[end..], false)
    };
    let title = after_dest.trim();
    let valid_title = title.is_empty()
        || (title.len() >= 2
            && matches!(
                (title.as_bytes()[0], title.as_bytes()[title.len() - 1]),
                (b'"', b'"') | (b'\'', b'\'') | (b'(', b')')
            ));
    if dest.is_empty() || (!angle && !dest.contains(['/', '.', '#', '?', ':'])) || !valid_title {
        return None;
    }
    Some(LinkRefDef {
        indent: &line[..indent],
        label,
        dest,
        title,
    })
}

fn rewrite_link_ref_definition(
    line: &str,
    base_url: &Url,
    include_images: bool,
    include_links: bool,
) -> Option<String> {
    let def = parse_link_ref_definition(line)?;
    if !include_links && !include_images {
        return Some(String::new());
    }
    match resolve_and_clean_url(def.dest, base_url) {
        ResolvedUrl::Resolved(cleaned) => Some(if def.title.is_empty() {
            format!(
                "{}[{}]: {}",
                def.indent,
                def.label,
                format_destination(&cleaned)
            )
        } else {
            format!(
                "{}[{}]: {} {}",
                def.indent,
                def.label,
                format_destination(&cleaned),
                def.title
            )
        }),
        ResolvedUrl::Drop | ResolvedUrl::UnwrapText => Some(String::new()),
    }
}

fn rewrite_inline_segment(
    s: &str,
    base_url: &Url,
    include_images: bool,
    include_links: bool,
    out: &mut String,
) {
    let bytes = s.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];

        // Preserve backslash escapes verbatim.
        if b == b'\\' && i + 1 < bytes.len() {
            let next_ch = s[i + 1..].chars().next().unwrap();
            let step = 1 + next_ch.len_utf8();
            out.push_str(&s[i..i + step]);
            i += step;
            continue;
        }

        // Preserve inline backtick code spans verbatim.
        if b == b'`' {
            let run = count_byte_run(bytes, i, b'`');
            if let Some(close_end) = find_closing_backticks(bytes, i + run, run) {
                out.push_str(&s[i..close_end]);
                i = close_end;
                continue;
            }
            out.push_str(&s[i..i + run]);
            i += run;
            continue;
        }

        let is_image = b == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let is_link = b == b'[';

        if is_image || is_link {
            let bracket_start = if is_image { i + 1 } else { i };
            if let Some(bracket_end) = find_matching_bracket(s, bracket_start) {
                if bracket_end + 1 < bytes.len() && bytes[bracket_end + 1] == b'(' {
                    if let Some(parsed) = parse_paren_destination(s, bracket_end + 1) {
                        let raw_label = &s[bracket_start + 1..bracket_end];
                        if is_image {
                            if include_images {
                                if let ResolvedUrl::Resolved(cleaned) =
                                    resolve_and_clean_url(parsed.dest, base_url)
                                {
                                    out.push_str("![");
                                    out.push_str(raw_label);
                                    out.push_str("](");
                                    out.push_str(&format_destination(&cleaned));
                                    out.push_str(parsed.title_suffix);
                                    out.push(')');
                                }
                            }
                        } else {
                            let mut rewritten_label = String::with_capacity(raw_label.len());
                            rewrite_inline_segment(
                                raw_label,
                                base_url,
                                include_images,
                                include_links,
                                &mut rewritten_label,
                            );
                            if !include_links {
                                out.push_str(&rewritten_label);
                            } else if rewritten_label.trim().is_empty()
                                && !raw_label.trim().is_empty()
                            {
                                // Label consisted solely of a dropped image; drop the empty link wrapper.
                            } else {
                                match resolve_and_clean_url(parsed.dest, base_url) {
                                    ResolvedUrl::Resolved(cleaned) => {
                                        out.push('[');
                                        out.push_str(&rewritten_label);
                                        out.push_str("](");
                                        out.push_str(&format_destination(&cleaned));
                                        out.push_str(parsed.title_suffix);
                                        out.push(')');
                                    }
                                    ResolvedUrl::Drop | ResolvedUrl::UnwrapText => {
                                        out.push_str(&rewritten_label);
                                    }
                                }
                            }
                        }
                        i = parsed.close_paren + 1;
                        continue;
                    }
                }
            }
        }

        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn count_byte_run(bytes: &[u8], start: usize, target: u8) -> usize {
    bytes[start..].iter().take_while(|&&b| b == target).count()
}

fn find_closing_backticks(bytes: &[u8], mut i: usize, expected_run: usize) -> Option<usize> {
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = count_byte_run(bytes, i, b'`');
            if run == expected_run {
                return Some(i + run);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

fn find_matching_bracket(s: &str, open_idx: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0usize;
    let mut i = open_idx;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                let next_ch = s[i + 1..].chars().next().unwrap();
                i += 1 + next_ch.len_utf8();
            }
            b'`' => {
                let run = count_byte_run(bytes, i, b'`');
                if let Some(close_end) = find_closing_backticks(bytes, i + run, run) {
                    i = close_end;
                } else {
                    i += run;
                }
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

struct ParenDestination<'a> {
    dest: &'a str,
    title_suffix: &'a str,
    close_paren: usize,
}

fn parse_paren_destination(s: &str, open_paren: usize) -> Option<ParenDestination<'_>> {
    let bytes = s.as_bytes();
    let mut i = open_paren + 1;

    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    let (dest, after_dest) = if bytes[i] == b'<' {
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
                continue;
            }
            if bytes[j] == b'>' {
                break;
            }
            if bytes[j] == b'\n' {
                return None;
            }
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'>' {
            return None;
        }
        (&s[start..j], j + 1)
    } else {
        let start = i;
        let mut j = start;
        let mut paren_depth = 0usize;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' if j + 1 < bytes.len() => {
                    j += 2;
                }
                b'(' => {
                    paren_depth += 1;
                    j += 1;
                }
                b')' => {
                    if paren_depth == 0 {
                        break;
                    }
                    paren_depth -= 1;
                    j += 1;
                }
                c if c.is_ascii_whitespace() => break,
                _ => j += 1,
            }
        }
        (&s[start..j], j)
    };

    let mut j = after_dest;
    let suffix_start = j;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    if bytes[j] == b')' {
        return Some(ParenDestination {
            dest,
            title_suffix: "",
            close_paren: j,
        });
    }

    // Optional link title: "...", '...', or (...)
    let close_quote = match bytes[j] {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    j += 1;
    while j < bytes.len() {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        if bytes[j] == close_quote {
            j += 1;
            break;
        }
        j += 1;
    }
    let title_end = j;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b')' {
        Some(ParenDestination {
            dest,
            title_suffix: &s[suffix_start..title_end],
            close_paren: j,
        })
    } else {
        None
    }
}

fn markdown_to_plain_text(md: &str) -> String {
    let defined_refs = collect_defined_ref_labels(md);
    let cleaned_tags = strip_markdown_html_mdx_tags(md);
    let mut lines: Vec<String> = Vec::new();
    let mut fence = FenceTracker::default();
    let mut blank_run = 0usize;

    for raw_line in cleaned_tags.lines() {
        match fence.step(raw_line) {
            FenceLine::Opening | FenceLine::Closing => continue,
            FenceLine::Inside => {
                lines.push(raw_line.trim_end().to_string());
                blank_run = 0;
                continue;
            }
            FenceLine::Outside => {}
        }
        if parse_link_ref_definition(raw_line).is_some() {
            continue;
        }

        let stripped = strip_line_markdown(raw_line.trim_end(), &defined_refs);
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !lines.is_empty() {
                lines.push(String::new());
            }
            continue;
        }
        blank_run = 0;
        lines.push(trimmed.to_string());
    }

    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    lines.join("\n")
}

fn strip_markdown_html_mdx_tags(md: &str) -> String {
    let bytes = md.as_bytes();
    let mut out = String::with_capacity(md.len());
    let mut fence = FenceTracker::default();
    let mut i = 0usize;
    let mut at_line_start = true;

    while i < bytes.len() {
        if at_line_start {
            let line_end = md[i..].find('\n').map(|n| i + n + 1).unwrap_or(bytes.len());
            let raw_line = md[i..line_end].trim_end_matches(['\r', '\n']);
            if fence.step(raw_line) != FenceLine::Outside {
                out.push_str(&md[i..line_end]);
                i = line_end;
                continue;
            }
            at_line_start = false;
        }

        if bytes[i] == b'\n' {
            out.push('\n');
            at_line_start = true;
            i += 1;
            continue;
        }
        if bytes[i] == b'`' {
            let run = count_byte_run(bytes, i, b'`');
            if let Some(close_end) = find_closing_backticks(bytes, i + run, run) {
                if !md[i..close_end].contains("\n\n") {
                    out.push_str(&md[i..close_end]);
                    i = close_end;
                    continue;
                }
            }
            out.push_str(&md[i..i + run]);
            i += run;
            continue;
        }
        if bytes[i] == b'<' {
            if md[i..].starts_with("<!--") {
                if let Some(end) = md[i + 4..].find("-->") {
                    i += 4 + end + 3;
                    continue;
                }
            }
            let prev_is_ident = i > 0
                && matches!(bytes[i - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b':');
            if !prev_is_ident {
                if let Some((tag_len, block_break)) = match_html_or_mdx_tag(&md[i..]) {
                    if block_break && !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    i += tag_len;
                    continue;
                }
            }
        }
        let ch = md[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn match_html_or_mdx_tag(s: &str) -> Option<(usize, bool)> {
    const INLINE_HTML_TAGS: &[&str] = &[
        "a", "abbr", "b", "code", "del", "em", "font", "i", "ins", "kbd", "mark", "q", "s",
        "small", "span", "strong", "sub", "sup", "u",
    ];
    const BLOCK_HTML_TAGS: &[&str] = &[
        "article",
        "aside",
        "audio",
        "blockquote",
        "br",
        "caption",
        "center",
        "col",
        "colgroup",
        "dd",
        "details",
        "div",
        "dl",
        "dt",
        "figcaption",
        "figure",
        "footer",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "iframe",
        "img",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "picture",
        "pre",
        "script",
        "section",
        "source",
        "style",
        "summary",
        "svg",
        "table",
        "tbody",
        "td",
        "tfoot",
        "th",
        "thead",
        "tr",
        "track",
        "ul",
        "video",
    ];

    let bytes = s.as_bytes();
    let is_close = bytes.get(1) == Some(&b'/');
    let name_start = if is_close { 2 } else { 1 };
    let first = *bytes.get(name_start)?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let mut j = name_start + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
        j += 1;
    }
    let name = &s[name_start..j];
    let after = *bytes.get(j)?;
    if !matches!(after, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n') {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let is_inline_html = INLINE_HTML_TAGS.binary_search(&lower.as_str()).is_ok();
    let is_block_html = !is_inline_html && BLOCK_HTML_TAGS.binary_search(&lower.as_str()).is_ok();
    let is_mdx = first.is_ascii_uppercase()
        && name.bytes().any(|b| b.is_ascii_lowercase())
        && !name.contains('-');
    if !is_inline_html && !is_block_html && !is_mdx {
        return None;
    }

    let limit = bytes.len().min(2048);
    let (mut quote, mut brace_depth) = (None, 0usize);
    while j < limit {
        let b = bytes[j];
        if quote.is_none() && brace_depth == 0 && b == b'\n' && bytes.get(j + 1) == Some(&b'\n') {
            return None;
        }
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'{' => brace_depth += 1,
            None if b == b'}' && brace_depth > 0 => brace_depth -= 1,
            None if brace_depth == 0 && b == b'<' => return None,
            None if brace_depth == 0 && b == b'>' => {
                let mut total = j + 1;
                if !is_close && matches!(lower.as_str(), "script" | "style") {
                    let close_pat = format!("</{lower}>");
                    if let Some(rel) = s[total..].to_ascii_lowercase().find(&close_pat) {
                        total += rel + close_pat.len();
                    }
                }
                let block_break = is_mdx || !is_inline_html;
                return Some((total, block_break));
            }
            None => {}
        }
        j += 1;
    }
    None
}

fn collect_defined_ref_labels(md: &str) -> HashSet<String> {
    let mut refs = HashSet::new();
    let mut fence = FenceTracker::default();
    for line in md.lines() {
        if fence.step(line) != FenceLine::Outside {
            continue;
        }
        if let Some(def) = parse_link_ref_definition(line) {
            refs.insert(def.label.trim().to_ascii_lowercase());
        }
    }
    refs
}

fn strip_line_markdown(line: &str, defined_refs: &HashSet<String>) -> String {
    let trimmed = line.trim_start();
    let without_heading = {
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            strip_trailing_atx_hashes(trimmed[hashes + 1..].trim())
        } else {
            trimmed
        }
    };
    let without_quote = without_heading.trim_start_matches(['>', ' ']).trim_start();
    let mut out = String::with_capacity(without_quote.len());
    strip_inline_markdown(without_quote, defined_refs, &mut out);
    out
}

fn strip_inline_markdown(s: &str, defined_refs: &HashSet<String>, out: &mut String) {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = count_byte_run(bytes, i, b'`');
            if let Some(close_end) = find_closing_backticks(bytes, i + run, run) {
                out.push_str(&s[i + run..close_end - run]);
                i = close_end;
                continue;
            }
            i += run;
            continue;
        }
        if matches!(bytes.get(i..i + 2), Some(b"**" | b"~~")) {
            let marker = &bytes[i..i + 2];
            if let Some(rel) = s[i + 2..].find(std::str::from_utf8(marker).unwrap()) {
                let inner = &s[i + 2..i + 2 + rel];
                if !inner.is_empty() {
                    strip_inline_markdown(inner, defined_refs, out);
                    i = i + 2 + rel + 2;
                    continue;
                }
            }
        }
        let is_image = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let is_link = bytes[i] == b'[';
        if is_image || is_link {
            let bracket_start = if is_image { i + 1 } else { i };
            if let Some(bracket_end) = find_matching_bracket(s, bracket_start) {
                let label = &s[bracket_start + 1..bracket_end];
                if bracket_end + 1 < bytes.len() && bytes[bracket_end + 1] == b'(' {
                    if let Some(parsed) = parse_paren_destination(s, bracket_end + 1) {
                        strip_inline_markdown(label, defined_refs, out);
                        i = parsed.close_paren + 1;
                        continue;
                    }
                } else if bracket_end + 1 < bytes.len() && bytes[bracket_end + 1] == b'[' {
                    if let Some(ref_end) = find_matching_bracket(s, bracket_end + 1) {
                        strip_inline_markdown(label, defined_refs, out);
                        i = ref_end + 1;
                        continue;
                    }
                } else if !is_image && defined_refs.contains(&label.trim().to_ascii_lowercase()) {
                    strip_inline_markdown(label, defined_refs, out);
                    i = bracket_end + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
}
