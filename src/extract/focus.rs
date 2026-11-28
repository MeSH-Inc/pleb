use std::collections::{HashMap, HashSet};

use super::markdown::{
    open_fence_at, split_atx_heading, truncate_markdown, FenceLine, FenceTracker,
};

/// Default byte budget (~4,000 tokens) when `--focus` is used without explicit `--max-bytes`.
pub const DEFAULT_FOCUS_MAX_BYTES: usize = 16_000;

struct SectionBlock<'a> {
    heading: &'a str,
    level: u8,
    body: String,
}

/// Result of extracting exact Markdown sections matching a heading selector.
pub struct SectionExtractResult {
    pub content: String,
    pub matched: bool,
    pub warning: Option<String>,
}

/// Extract exact Markdown section(s) matching `selector` (exact -> prefix -> substring,
/// case-insensitive after link/backslash/whitespace normalization), including nested subsections.
pub fn extract_section(markdown: &str, selector: &str) -> SectionExtractResult {
    let (_, sections) = split_sections(markdown);
    let query_lower = normalize_heading_label(selector).to_ascii_lowercase();
    let labels: Vec<String> = sections
        .iter()
        .map(|s| normalize_heading_label(s.heading))
        .collect();
    let lowers: Vec<String> = labels.iter().map(|l| l.to_ascii_lowercase()).collect();

    let matched_roots: Vec<usize> = if query_lower.is_empty() || sections.is_empty() {
        Vec::new()
    } else {
        let exact: Vec<usize> = lowers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| (l == &query_lower).then_some(i))
            .collect();
        if !exact.is_empty() {
            exact
        } else {
            let prefix: Vec<usize> = lowers
                .iter()
                .enumerate()
                .filter_map(|(i, l)| l.starts_with(&query_lower).then_some(i))
                .collect();
            if !prefix.is_empty() {
                prefix
            } else {
                lowers
                    .iter()
                    .enumerate()
                    .filter_map(|(i, l)| l.contains(&query_lower).then_some(i))
                    .collect()
            }
        }
    };

    if matched_roots.is_empty() {
        if labels.is_empty() {
            return SectionExtractResult {
                content: format!(
                    "<!-- Section {selector:?} not found (document has no headings). -->"
                ),
                matched: false,
                warning: Some(format!(
                    "requested section {selector:?} not found; document has no headings"
                )),
            };
        }
        let avail = format_bounded_label_list(&labels, 560, "sections");
        return SectionExtractResult {
            content: format!(
                "<!-- Section {selector:?} not found. Available sections: {avail} -->"
            ),
            matched: false,
            warning: Some(format!(
                "requested section {selector:?} not found; available sections: {avail}"
            )),
        };
    }

    let mut included = vec![false; sections.len()];
    for root_idx in matched_roots {
        if included[root_idx] {
            continue;
        }
        let root_level = sections[root_idx].level;
        included[root_idx] = true;
        for (j, sec) in sections.iter().enumerate().skip(root_idx + 1) {
            if sec.level <= root_level {
                break;
            }
            included[j] = true;
        }
    }

    let mut parts = Vec::new();
    for (idx, sec) in sections.iter().enumerate() {
        if !included[idx] {
            continue;
        }
        if sec.body.is_empty() {
            parts.push(sec.heading.to_string());
        } else {
            parts.push(format!("{}\n\n{}", sec.heading, sec.body));
        }
    }

    SectionExtractResult {
        content: parts.join("\n\n"),
        matched: true,
        warning: None,
    }
}

/// Project a Markdown or plain-text document onto the query-relevant sections within `max_bytes`,
/// preserving intact headings, code blocks, tables, and a bounded collapsed-sections map.
///
/// Returns `(projected, truncated)` where `projected.len() <= max_bytes` always holds and
/// `truncated` is `true` whenever content was collapsed or trimmed.
pub fn focus_document(
    content: &str,
    query: &str,
    max_bytes: usize,
    is_markdown: bool,
) -> (String, bool) {
    if content.len() <= max_bytes {
        return (content.to_string(), false);
    }
    let query_terms = tokenize_terms(query);
    if query_terms.is_empty() || max_bytes < 64 {
        return truncate_for_mode(content, max_bytes, is_markdown);
    }

    let (preamble, sections) = split_sections(content);
    if sections.is_empty() {
        let candidate = compress_blocks(content, &query_terms, max_bytes, false)
            .unwrap_or_else(|| content.to_string());
        if fits_safely(&candidate, max_bytes, is_markdown) {
            return (candidate, true);
        }
        return truncate_for_mode(&candidate, max_bytes, is_markdown);
    }

    let heading_tokens: Vec<Vec<String>> = sections
        .iter()
        .map(|s| tokenize_terms(&strip_inline_markdown_links(s.heading)))
        .collect();
    let doc_tokens: Vec<Vec<String>> = sections
        .iter()
        .zip(&heading_tokens)
        .map(|(s, h)| h.iter().cloned().chain(tokenize_terms(&s.body)).collect())
        .collect();
    let heading_sets: Vec<HashSet<&str>> = heading_tokens
        .iter()
        .map(|h| h.iter().map(String::as_str).collect())
        .collect();
    let scores = bm25_scores(&doc_tokens, &query_terms, Some(&heading_sets), 0.35);

    let toc_budget = (max_bytes / 10).clamp(120, 560);
    let render = |sel: &HashSet<usize>, ov: &HashMap<usize, String>| {
        render_selected(&preamble, &sections, sel, ov, max_bytes, toc_budget)
    };
    let ranked = rank_indices(&scores);
    let mut selected: HashSet<usize> = HashSet::new();
    let mut overrides: HashMap<usize, String> = HashMap::new();

    if sections[0].level == 1 && render(&HashSet::from([0usize]), &overrides).len() <= max_bytes / 3
    {
        selected.insert(0);
    }

    for &idx in &ranked {
        if selected.contains(&idx) {
            continue;
        }
        let mut trial = selected.clone();
        trial.insert(idx);
        if sections[idx].level > 1 {
            if let Some(prev) = (0..idx)
                .rev()
                .find(|&p| sections[p].level < sections[idx].level)
            {
                if sections[prev].body.len() < 300 {
                    trial.insert(prev);
                }
            }
        }
        if render(&trial, &overrides).len() <= max_bytes {
            selected = trial;
            continue;
        }
        if scores[idx] > 0.0 {
            let current_len = render(&selected, &overrides).len();
            let rem = max_bytes.saturating_sub(current_len + sections[idx].heading.len() + 48);
            if rem >= 400 {
                if let Some(compressed) =
                    compress_blocks(&sections[idx].body, &query_terms, rem, true)
                {
                    let mut trial_sel = selected.clone();
                    trial_sel.insert(idx);
                    let mut trial_ov = overrides.clone();
                    trial_ov.insert(idx, compressed);
                    if render(&trial_sel, &trial_ov).len() <= max_bytes {
                        selected = trial_sel;
                        overrides = trial_ov;
                    }
                }
            }
        }
    }

    if selected.is_empty() {
        let top = ranked[0];
        let body_budget = max_bytes.saturating_sub(sections[top].heading.len() + toc_budget + 48);
        if body_budget >= 200 {
            if let Some(compressed) =
                compress_blocks(&sections[top].body, &query_terms, body_budget, true)
            {
                overrides.insert(top, compressed);
            }
        }
        selected.insert(top);
    }

    let rendered = render(&selected, &overrides);
    let was_collapsed = selected.len() < sections.len() || !overrides.is_empty();
    if fits_safely(&rendered, max_bytes, is_markdown) {
        return (rendered, was_collapsed);
    }
    let (capped, _) = truncate_for_mode(&rendered, max_bytes, is_markdown);
    (capped, true)
}

fn truncate_for_mode(content: &str, max_bytes: usize, is_markdown: bool) -> (String, bool) {
    if is_markdown {
        truncate_markdown(content, max_bytes)
    } else {
        super::truncate_utf8(content, max_bytes)
    }
}

fn fits_safely(content: &str, max_bytes: usize, is_markdown: bool) -> bool {
    content.len() <= max_bytes && (!is_markdown || open_fence_at(content, content.len()).is_none())
}

fn rank_indices(scores: &[f64]) -> Vec<usize> {
    let mut ranked: Vec<usize> = (0..scores.len()).collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));
    ranked
}

fn render_selected(
    preamble: &str,
    sections: &[SectionBlock<'_>],
    selected: &HashSet<usize>,
    overrides: &HashMap<usize, String>,
    max_bytes: usize,
    toc_budget: usize,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !preamble.is_empty() && preamble.len() <= max_bytes / 5 {
        parts.push(preamble.to_string());
    }
    for (idx, sec) in sections.iter().enumerate() {
        if selected.contains(&idx) {
            let body = overrides.get(&idx).map(String::as_str).unwrap_or(&sec.body);
            if body.is_empty() {
                parts.push(sec.heading.to_string());
            } else {
                parts.push(format!("{}\n\n{}", sec.heading, body));
            }
        }
    }
    let toc = format_bounded_toc(sections, selected, toc_budget);
    if !toc.is_empty() {
        parts.push(toc);
    }
    parts.join("\n\n")
}

fn normalize_heading_label(raw_heading: &str) -> String {
    let label = strip_inline_markdown_links(raw_heading)
        .trim_start_matches('#')
        .trim()
        .replace("\\_", "_")
        .replace("\\*", "*");
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_bounded_label_list(labels: &[String], budget: usize, fallback_noun: &str) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut kept: Vec<&str> = Vec::new();
    for (idx, item) in labels.iter().enumerate() {
        let rem = labels.len() - idx - 1;
        let suffix = if rem > 0 {
            format!(" | ... (+{rem} more)")
        } else {
            String::new()
        };
        let joined = kept
            .iter()
            .copied()
            .chain(std::iter::once(item.as_str()))
            .collect::<Vec<_>>()
            .join(" | ");
        if joined.len() + suffix.len() > budget {
            let rem_all = labels.len() - kept.len();
            return if kept.is_empty() {
                format!("{rem_all} {fallback_noun}")
            } else {
                format!("{} | ... (+{rem_all} more)", kept.join(" | "))
            };
        }
        kept.push(item.as_str());
    }
    kept.join(" | ")
}

fn format_bounded_toc(
    sections: &[SectionBlock<'_>],
    selected: &HashSet<usize>,
    toc_budget: usize,
) -> String {
    let collapsed: Vec<String> = sections
        .iter()
        .enumerate()
        .filter(|(idx, _)| !selected.contains(idx))
        .filter_map(|(_, s)| {
            let norm = normalize_heading_label(s.heading);
            (!norm.is_empty()).then_some(norm)
        })
        .collect();
    if collapsed.is_empty() {
        return String::new();
    }
    let inner_budget = toc_budget.saturating_sub("<!-- Collapsed sections:  -->".len());
    let summary = format_bounded_label_list(&collapsed, inner_budget, "omitted");
    format!("<!-- Collapsed sections: {summary} -->")
}

fn compress_blocks(
    body: &str,
    query_terms: &[String],
    max_bytes: usize,
    lead_boost: bool,
) -> Option<String> {
    let blocks = split_subblocks(body);
    if blocks.len() <= 1 {
        return None;
    }
    let block_tokens: Vec<Vec<String>> = blocks.iter().map(|b| tokenize_terms(b)).collect();
    let scores = bm25_scores(
        &block_tokens,
        query_terms,
        None,
        if lead_boost { 0.25 } else { 0.0 },
    );

    let mut chosen: Vec<usize> = Vec::new();
    for idx in rank_indices(&scores) {
        let mut trial = chosen.clone();
        trial.push(idx);
        trial.sort_unstable();
        if join_subblocks(&blocks, &trial).len() <= max_bytes {
            chosen = trial;
        }
    }
    (!chosen.is_empty()).then(|| join_subblocks(&blocks, &chosen))
}

fn join_subblocks(blocks: &[String], chosen: &[usize]) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(chosen.len() * 2);
    let mut prev: Option<usize> = None;
    for &idx in chosen {
        if prev.is_some_and(|p| idx > p + 1) {
            parts.push("[...]");
        }
        parts.push(blocks[idx].as_str());
        prev = Some(idx);
    }
    parts.join("\n\n")
}

fn split_sections<'a>(markdown: &'a str) -> (String, Vec<SectionBlock<'a>>) {
    let mut preamble = Vec::new();
    let mut sections = Vec::new();
    let mut cur_heading: Option<(&'a str, u8)> = None;
    let mut cur_lines: Vec<&'a str> = Vec::new();
    let mut fence = FenceTracker::default();

    let flush = |heading: Option<(&'a str, u8)>,
                 lines: &mut Vec<&'a str>,
                 out: &mut Vec<SectionBlock<'a>>| {
        if let Some((heading, level)) = heading {
            out.push(SectionBlock {
                heading,
                level,
                body: lines.join("\n").trim().to_string(),
            });
            lines.clear();
        }
    };

    for line in markdown.lines() {
        if fence.step(line) == FenceLine::Outside {
            if let Some((level, _, _)) = split_atx_heading(line.trim_start()) {
                flush(cur_heading.take(), &mut cur_lines, &mut sections);
                cur_heading = Some((line, level));
                continue;
            }
        }
        if cur_heading.is_some() {
            cur_lines.push(line);
        } else {
            preamble.push(line);
        }
    }
    flush(cur_heading, &mut cur_lines, &mut sections);
    (preamble.join("\n").trim().to_string(), sections)
}

fn split_subblocks(body: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut cur = Vec::new();
    let mut fence = FenceTracker::default();
    let flush = |cur: &mut Vec<&str>, blocks: &mut Vec<String>| {
        if !cur.is_empty() {
            let text = cur.join("\n").trim().to_string();
            if !text.is_empty() {
                blocks.push(text);
            }
            cur.clear();
        }
    };

    for line in body.lines() {
        if fence.step(line) == FenceLine::Outside && line.trim().is_empty() {
            flush(&mut cur, &mut blocks);
        } else {
            cur.push(line);
        }
    }
    flush(&mut cur, &mut blocks);
    blocks
}

fn strip_inline_markdown_links(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(rel_close) = text[i + 1..].find(']') {
                let close_bracket = i + 1 + rel_close;
                if bytes.get(close_bracket + 1) == Some(&b'(') {
                    if let Some(rel_paren) = text[close_bracket + 2..].find(')') {
                        out.push_str(&text[i + 1..close_bracket]);
                        i = close_bracket + 2 + rel_paren + 1;
                        continue;
                    }
                }
            }
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn tokenize_terms(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/' | '+' | '=') {
            cur.push(ch);
        } else if !cur.is_empty() {
            emit_identifier_tokens(&cur, &mut terms);
            cur.clear();
        }
    }
    if !cur.is_empty() {
        emit_identifier_tokens(&cur, &mut terms);
    }
    terms
}

fn emit_identifier_tokens(raw: &str, out: &mut Vec<String>) {
    let trimmed = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if trimmed.is_empty() {
        return;
    }
    let lower = trimmed.to_ascii_lowercase();
    out.push(lower.clone());
    // Split snake_case, kebab-case, paths, and camelCase identifiers into sub-tokens.
    for seg in trimmed
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
    {
        let seg_lower = seg.to_ascii_lowercase();
        if seg_lower.len() >= 2 && seg_lower != lower {
            out.push(seg_lower.clone());
        }
        let bytes = seg.as_bytes();
        let mut start = 0usize;
        let mut push_part = |part: &str| {
            let p = part.to_ascii_lowercase();
            if p.len() >= 2 && p != seg_lower && p != lower {
                out.push(p);
            }
        };
        for idx in 1..bytes.len() {
            if bytes[idx].is_ascii_uppercase() && bytes[idx - 1].is_ascii_lowercase() {
                push_part(&seg[start..idx]);
                start = idx;
            }
        }
        if start > 0 {
            push_part(&seg[start..]);
        }
    }
}

fn bm25_scores(
    doc_tokens: &[Vec<String>],
    query_terms: &[String],
    heading_boost_sets: Option<&[HashSet<&str>]>,
    lead_boost: f64,
) -> Vec<f64> {
    let n_docs = doc_tokens.len();
    if n_docs == 0 {
        return Vec::new();
    }
    let avgdl = doc_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n_docs as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for toks in doc_tokens {
        for term in toks.iter().map(String::as_str).collect::<HashSet<_>>() {
            *df.entry(term).or_insert(0) += 1;
        }
    }

    let (k1, b) = (1.5f64, 0.75f64);
    doc_tokens
        .iter()
        .enumerate()
        .map(|(idx, toks)| {
            let mut tf: HashMap<&str, usize> = HashMap::new();
            for t in toks {
                *tf.entry(t.as_str()).or_insert(0) += 1;
            }
            let dl = toks.len() as f64;
            let mut score = if idx == 0 { lead_boost } else { 0.0 };
            for q in query_terms {
                let Some(&f_usize) = tf.get(q.as_str()) else {
                    continue;
                };
                let f = f_usize as f64;
                let df_q = *df.get(q.as_str()).unwrap_or(&0) as f64;
                let idf = ((n_docs as f64 - df_q + 0.5) / (df_q + 0.5) + 1.0).ln();
                let denom = f + k1 * (1.0 - b + b * dl / avgdl.max(1.0));
                let mut term_score = idf * (f * (k1 + 1.0)) / denom.max(1e-9);
                if heading_boost_sets.is_some_and(|sets| sets[idx].contains(q.as_str())) {
                    term_score *= 1.6;
                }
                score += term_score;
            }
            score
        })
        .collect()
}
