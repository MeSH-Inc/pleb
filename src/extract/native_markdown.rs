use anyhow::Result;
use url::Url;

use super::markdown::{
    compact_markdown, format_destination, is_closing_fence, parse_opening_fence, truncate_markdown,
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
        });
    }

    let frontmatter = parse_leading_frontmatter(body);
    let preserve_frontmatter = options.mode == ExtractMode::Full && frontmatter.is_some();

    let (frontmatter_prefix, markdown_body) = match &frontmatter {
        Some(fm) if preserve_frontmatter => (&body[..fm.end_offset], &body[fm.end_offset..]),
        Some(fm) => ("", body[fm.end_offset..].trim_start_matches(['\r', '\n'])),
        None => ("", body),
    };

    let title = frontmatter
        .as_ref()
        .and_then(|fm| fm.title.clone())
        .or_else(|| first_atx_h1(markdown_body));

    let rewritten_body = rewrite_markdown_links(
        markdown_body,
        page_url,
        options.include_images,
        options.include_links,
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
                match &title {
                    Some(t) if !starts_with_title_heading(trimmed, t) => {
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
            })
        }
        OutputFormat::Html | OutputFormat::Raw => unreachable!(),
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

fn first_atx_h1(md: &str) -> Option<String> {
    let mut fence: Option<(char, usize)> = None;
    for raw_line in md.lines() {
        if let Some((fc, fl)) = fence {
            if is_closing_fence(raw_line, fc, fl) {
                fence = None;
            }
            continue;
        }
        if let Some(f) = parse_opening_fence(raw_line) {
            fence = Some(f);
            continue;
        }
        let trimmed = raw_line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let heading = strip_trailing_atx_hashes(rest.trim());
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
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
) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence: Option<(char, usize)> = None;

    for raw_line in md.split_inclusive('\n') {
        let (line, newline) = if let Some(stripped) = raw_line.strip_suffix("\r\n") {
            (stripped, "\r\n")
        } else if let Some(stripped) = raw_line.strip_suffix('\n') {
            (stripped, "\n")
        } else {
            (raw_line, "")
        };

        if let Some((fc, fl)) = fence {
            out.push_str(line);
            out.push_str(newline);
            if is_closing_fence(line, fc, fl) {
                fence = None;
            }
            continue;
        }

        if let Some(f) = parse_opening_fence(line) {
            fence = Some(f);
            out.push_str(line);
            out.push_str(newline);
            continue;
        }

        rewrite_inline_segment(line, base_url, include_images, include_links, &mut out);
        out.push_str(newline);
    }

    out
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
    let mut lines: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut blank_run = 0usize;

    for raw_line in md.lines() {
        if let Some((fc, fl)) = fence {
            if is_closing_fence(raw_line, fc, fl) {
                fence = None;
            } else {
                lines.push(raw_line.trim_end().to_string());
                blank_run = 0;
            }
            continue;
        }
        if let Some(f) = parse_opening_fence(raw_line) {
            fence = Some(f);
            continue;
        }

        let stripped = strip_line_markdown(raw_line.trim_end());
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

fn strip_line_markdown(line: &str) -> String {
    let trimmed = line.trim_start();
    // Strip ATX heading markers.
    let without_heading = {
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            strip_trailing_atx_hashes(trimmed[hashes + 1..].trim())
        } else {
            trimmed
        }
    };
    let without_quote = without_heading.trim_start_matches(['>', ' ']).trim_start();

    // Unwrap `[label](url)` and `![alt](url)` to label/alt, and strip backticks / emphasis markers.
    let mut out = String::with_capacity(without_quote.len());
    let bytes = without_quote.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let is_image = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let is_link = bytes[i] == b'[';
        if is_image || is_link {
            let bracket_start = if is_image { i + 1 } else { i };
            if let Some(bracket_end) = find_matching_bracket(without_quote, bracket_start) {
                if bracket_end + 1 < bytes.len() && bytes[bracket_end + 1] == b'(' {
                    if let Some(parsed) = parse_paren_destination(without_quote, bracket_end + 1) {
                        out.push_str(&without_quote[bracket_start + 1..bracket_end]);
                        i = parsed.close_paren + 1;
                        continue;
                    }
                }
            }
        }
        if bytes[i] == b'`' {
            i += 1;
            continue;
        }
        let ch = without_quote[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}
