use std::collections::{HashMap, HashSet};

use super::markdown::{open_fence_at, truncate_markdown};

/// Default byte budget (~4,000 tokens) when `--focus` is used without explicit `--max-bytes`.
pub const DEFAULT_FOCUS_MAX_BYTES: usize = 16_000;

struct SectionBlock<'a> {
    heading: &'a str,
    level: u8,
    body: String,
    index: usize,
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
        return if is_markdown {
            truncate_markdown(content, max_bytes)
        } else {
            super::truncate_utf8(content, max_bytes)
        };
    }

    let (preamble, sections) = split_sections(content);
    if sections.is_empty() {
        let compressed = compress_blocks(content, &query_terms, max_bytes, false);
        let candidate = compressed.unwrap_or_else(|| content.to_string());
        if candidate.len() <= max_bytes
            && (!is_markdown || open_fence_at(&candidate, candidate.len()).is_none())
        {
            return (candidate, true);
        }
        return if is_markdown {
            truncate_markdown(&candidate, max_bytes)
        } else {
            super::truncate_utf8(&candidate, max_bytes)
        };
    }

    let doc_tokens: Vec<Vec<String>> = sections
        .iter()
        .map(|s| {
            let clean_h = strip_inline_markdown_links(s.heading);
            let mut toks = tokenize_terms(&clean_h);
            toks.extend(tokenize_terms(&s.body));
            toks
        })
        .collect();
    let heading_sets: Vec<HashSet<String>> = sections
        .iter()
        .map(|s| {
            tokenize_terms(&strip_inline_markdown_links(s.heading))
                .into_iter()
                .collect()
        })
        .collect();
    let scores = bm25_scores(&doc_tokens, &query_terms, Some(&heading_sets), 0.35);

    let toc_budget = (max_bytes / 10).clamp(120, 560);
    let mut ranked: Vec<usize> = (0..sections.len()).collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));

    let mut selected: HashSet<usize> = HashSet::new();
    let mut overrides: HashMap<usize, String> = HashMap::new();

    if sections[0].level == 1 {
        let mut init = HashSet::new();
        init.insert(0usize);
        let cand = render_selected(
            &preamble, &sections, &init, &overrides, max_bytes, toc_budget,
        );
        if cand.len() <= max_bytes / 3 {
            selected.insert(0);
        }
    }

    for &idx in &ranked {
        if selected.contains(&idx) {
            continue;
        }
        let mut trial = selected.clone();
        trial.insert(idx);
        if sections[idx].level > 1 {
            for prev in (0..idx).rev() {
                if sections[prev].level < sections[idx].level {
                    if sections[prev].body.len() < 300 {
                        trial.insert(prev);
                    }
                    break;
                }
            }
        }
        let rendered = render_selected(
            &preamble, &sections, &trial, &overrides, max_bytes, toc_budget,
        );
        if rendered.len() <= max_bytes {
            selected = trial;
            continue;
        }
        if scores[idx] > 0.0 {
            let current_len = render_selected(
                &preamble, &sections, &selected, &overrides, max_bytes, toc_budget,
            )
            .len();
            let heading_overhead = sections[idx].heading.len() + 48;
            let rem = max_bytes.saturating_sub(current_len + heading_overhead);
            if rem >= 400 {
                if let Some(compressed) =
                    compress_blocks(&sections[idx].body, &query_terms, rem, true)
                {
                    let mut trial_sel = selected.clone();
                    trial_sel.insert(idx);
                    let mut trial_ov = overrides.clone();
                    trial_ov.insert(idx, compressed);
                    let cand = render_selected(
                        &preamble, &sections, &trial_sel, &trial_ov, max_bytes, toc_budget,
                    );
                    if cand.len() <= max_bytes {
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

    let rendered = render_selected(
        &preamble, &sections, &selected, &overrides, max_bytes, toc_budget,
    );
    let was_collapsed = selected.len() < sections.len() || !overrides.is_empty();
    if rendered.len() <= max_bytes
        && (!is_markdown || open_fence_at(&rendered, rendered.len()).is_none())
    {
        return (rendered, was_collapsed);
    }

    let (capped, _) = if is_markdown {
        truncate_markdown(&rendered, max_bytes)
    } else {
        super::truncate_utf8(&rendered, max_bytes)
    };
    (capped, true)
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
    for sec in sections {
        if selected.contains(&sec.index) {
            let body = overrides
                .get(&sec.index)
                .map(String::as_str)
                .unwrap_or(&sec.body);
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

fn format_bounded_toc(
    sections: &[SectionBlock<'_>],
    selected: &HashSet<usize>,
    toc_budget: usize,
) -> String {
    let collapsed: Vec<String> = sections
        .iter()
        .filter(|s| !selected.contains(&s.index))
        .filter_map(|s| {
            let stripped = strip_inline_markdown_links(s.heading);
            let label = stripped
                .trim_start_matches('#')
                .trim()
                .replace("\\_", "_")
                .replace("\\*", "*");
            let norm = label.split_whitespace().collect::<Vec<_>>().join(" ");
            (!norm.is_empty()).then_some(norm)
        })
        .collect();
    if collapsed.is_empty() {
        return String::new();
    }
    let mut kept: Vec<&str> = Vec::new();
    for (idx, item) in collapsed.iter().enumerate() {
        let rem = collapsed.len() - idx - 1;
        let suffix = if rem > 0 {
            format!(" | ... (+{rem} more)")
        } else {
            String::new()
        };
        let joined = if kept.is_empty() {
            item.as_str().to_string()
        } else {
            format!("{} | {}", kept.join(" | "), item)
        };
        let candidate = format!("<!-- Collapsed sections: {joined}{suffix} -->");
        if candidate.len() > toc_budget {
            let rem_all = collapsed.len() - kept.len();
            return if kept.is_empty() {
                format!("<!-- Collapsed sections: {rem_all} omitted -->")
            } else {
                format!(
                    "<!-- Collapsed sections: {} | ... (+{rem_all} more) -->",
                    kept.join(" | ")
                )
            };
        }
        kept.push(item.as_str());
    }
    format!("<!-- Collapsed sections: {} -->", kept.join(" | "))
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
    let mut ranked: Vec<usize> = (0..blocks.len()).collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then_with(|| a.cmp(&b)));

    let mut chosen: Vec<usize> = Vec::new();
    for idx in ranked {
        let mut trial = chosen.clone();
        trial.push(idx);
        trial.sort_unstable();
        let rendered = join_subblocks(&blocks, &trial);
        if rendered.len() <= max_bytes {
            chosen = trial;
        }
    }
    if chosen.is_empty() {
        return None;
    }
    Some(join_subblocks(&blocks, &chosen))
}

fn join_subblocks(blocks: &[String], chosen: &[usize]) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(chosen.len() * 2);
    let mut prev: Option<usize> = None;
    for &idx in chosen {
        if let Some(p) = prev {
            if idx > p + 1 {
                parts.push("[...]");
            }
        }
        parts.push(blocks[idx].as_str());
        prev = Some(idx);
    }
    parts.join("\n\n")
}

fn split_sections(markdown: &str) -> (String, Vec<SectionBlock<'_>>) {
    let mut preamble: Vec<&str> = Vec::new();
    let mut sections: Vec<SectionBlock<'_>> = Vec::new();
    let mut cur_heading: Option<(&str, u8)> = None;
    let mut cur_lines: Vec<&str> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in markdown.lines() {
        let trimmed_start = line.trim_start();
        update_fence_state(trimmed_start, &mut fence);
        if fence.is_none() {
            if let Some(level) = parse_atx_heading_level(trimmed_start) {
                if let Some((h, lvl)) = cur_heading.take() {
                    sections.push(SectionBlock {
                        heading: h,
                        level: lvl,
                        body: cur_lines.join("\n").trim().to_string(),
                        index: sections.len(),
                    });
                    cur_lines.clear();
                }
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
    if let Some((h, lvl)) = cur_heading {
        sections.push(SectionBlock {
            heading: h,
            level: lvl,
            body: cur_lines.join("\n").trim().to_string(),
            index: sections.len(),
        });
    }
    (preamble.join("\n").trim().to_string(), sections)
}

fn split_subblocks(body: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for line in body.lines() {
        let trimmed_start = line.trim_start();
        update_fence_state(trimmed_start, &mut fence);
        if fence.is_none() && line.trim().is_empty() {
            if !cur.is_empty() {
                let text = cur.join("\n").trim().to_string();
                if !text.is_empty() {
                    blocks.push(text);
                }
                cur.clear();
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        let text = cur.join("\n").trim().to_string();
        if !text.is_empty() {
            blocks.push(text);
        }
    }
    blocks
}

fn update_fence_state(trimmed_start: &str, fence: &mut Option<(char, usize)>) {
    if let Some((fc, fl)) = *fence {
        let t = trimmed_start.trim();
        if t.len() >= fl && t.chars().all(|c| c == fc) {
            *fence = None;
        }
    } else if let Some(fc) = trimmed_start
        .chars()
        .next()
        .filter(|&c| c == '`' || c == '~')
    {
        let fl = trimmed_start.chars().take_while(|&c| c == fc).count();
        if fl >= 3 {
            *fence = Some((fc, fl));
        }
    }
}

fn parse_atx_heading_level(trimmed_start: &str) -> Option<u8> {
    let hashes = trimmed_start.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && trimmed_start[hashes..].starts_with(' ')
        && !trimmed_start[hashes..].trim().is_empty()
    {
        Some(hashes as u8)
    } else {
        None
    }
}

fn strip_inline_markdown_links(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(rel_close) = text[i + 1..].find(']') {
                let close_bracket = i + 1 + rel_close;
                if close_bracket + 1 < bytes.len() && bytes[close_bracket + 1] == b'(' {
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
    let mut terms: Vec<String> = Vec::new();
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
    if lower.len() >= 2 || lower.chars().all(|c| c.is_ascii_alphanumeric()) {
        out.push(lower.clone());
    }
    // Split snake_case, kebab-case, paths, and punctuation-joined identifiers into sub-tokens.
    for seg in trimmed.split(|c: char| !c.is_ascii_alphanumeric()) {
        if seg.is_empty() {
            continue;
        }
        let seg_lower = seg.to_ascii_lowercase();
        if seg_lower.len() >= 2 && seg_lower != lower {
            out.push(seg_lower.clone());
        }
        // Split camelCase segments (e.g. LockResult -> lock, result; PoolOptions -> pool, options).
        let mut start = 0usize;
        let bytes = seg.as_bytes();
        for idx in 1..bytes.len() {
            if bytes[idx].is_ascii_uppercase() && bytes[idx - 1].is_ascii_lowercase() {
                let part = seg[start..idx].to_ascii_lowercase();
                if part.len() >= 2 && part != seg_lower && part != lower {
                    out.push(part);
                }
                start = idx;
            }
        }
        if start > 0 {
            let tail = seg[start..].to_ascii_lowercase();
            if tail.len() >= 2 && tail != seg_lower && tail != lower {
                out.push(tail);
            }
        }
    }
}

fn bm25_scores(
    doc_tokens: &[Vec<String>],
    query_terms: &[String],
    heading_boost_sets: Option<&[HashSet<String>]>,
    lead_boost: f64,
) -> Vec<f64> {
    let n_docs = doc_tokens.len();
    if n_docs == 0 {
        return Vec::new();
    }
    let avgdl = doc_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n_docs as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for toks in doc_tokens {
        let unique: HashSet<&str> = toks.iter().map(String::as_str).collect();
        for term in unique {
            *df.entry(term).or_insert(0) += 1;
        }
    }

    let k1 = 1.5f64;
    let b = 0.75f64;
    let mut scores = Vec::with_capacity(n_docs);
    for (idx, toks) in doc_tokens.iter().enumerate() {
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for t in toks {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        let dl = toks.len() as f64;
        let mut score = 0.0f64;
        for q in query_terms {
            let Some(&f_usize) = tf.get(q.as_str()) else {
                continue;
            };
            let f = f_usize as f64;
            let df_q = *df.get(q.as_str()).unwrap_or(&0) as f64;
            let idf = ((n_docs as f64 - df_q + 0.5) / (df_q + 0.5) + 1.0).ln();
            let denom = f + k1 * (1.0 - b + b * dl / avgdl.max(1.0));
            let mut term_score = idf * (f * (k1 + 1.0)) / denom.max(1e-9);
            if let Some(sets) = heading_boost_sets {
                if sets[idx].contains(q.as_str()) {
                    term_score *= 1.6;
                }
            }
            score += term_score;
        }
        if idx == 0 {
            score += lead_boost;
        }
        scores.push(score);
    }
    scores
}
