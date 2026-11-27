use anyhow::Result;

use super::{
    apply_format_limit, apply_utf8_limit, dom::collapse_inline_whitespace, ExtractMode,
    ExtractOptions, ExtractedDocument, OutputFormat,
};

pub fn is_transcript_document(mime: Option<&str>, body: &str) -> bool {
    if matches!(
        mime,
        Some("text/vtt" | "application/x-subrip" | "application/srt" | "text/srt")
    ) {
        return true;
    }

    let stripped = body.strip_prefix('\u{FEFF}').unwrap_or(body);
    if let Some(rest) = stripped.strip_prefix("WEBVTT") {
        return rest.is_empty() || rest.starts_with([' ', '\t', '\r', '\n']);
    }

    false
}

pub fn extract_transcript(body: &str, options: &ExtractOptions) -> Result<ExtractedDocument> {
    if options.mode == ExtractMode::Full || options.format == OutputFormat::Html {
        let (content, truncated) = apply_utf8_limit(body, options.max_bytes);
        return Ok(ExtractedDocument {
            title: None,
            content,
            truncated,
            resources: Vec::new(),
            tools: Vec::new(),
        });
    }

    let cleaned = clean_transcript_text(body, options.format == OutputFormat::Markdown);
    let (content, truncated) = apply_format_limit(&cleaned, options.format, options.max_bytes);

    Ok(ExtractedDocument {
        title: None,
        content,
        truncated,
        resources: Vec::new(),
        tools: Vec::new(),
    })
}

fn clean_transcript_text(body: &str, is_markdown: bool) -> String {
    let stripped = body.strip_prefix('\u{FEFF}').unwrap_or(body);
    let lines: Vec<&str> = stripped.lines().collect();

    let mut out_lines: Vec<String> = Vec::new();
    let mut active_speaker: Option<String> = None;
    let mut last_emitted: Option<(Option<String>, String)> = None;

    let mut i = 0usize;
    let mut in_skip_block = false;

    // Skip optional WEBVTT header block at the top of the file.
    if let Some(&first) = lines.first() {
        let trimmed = first.trim();
        if trimmed == "WEBVTT"
            || trimmed
                .strip_prefix("WEBVTT")
                .is_some_and(|r| r.starts_with([' ', '\t']))
        {
            i = 1;
            while i < lines.len() {
                let line = lines[i].trim();
                if line.is_empty() {
                    i += 1;
                    break;
                }
                if is_timing_line(line)
                    || lines
                        .get(i + 1)
                        .is_some_and(|next| is_timing_line(next.trim()))
                {
                    break;
                }
                i += 1;
            }
        }
    }

    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();

        if trimmed.is_empty() {
            in_skip_block = false;
            i += 1;
            continue;
        }

        if in_skip_block {
            i += 1;
            continue;
        }

        if is_vtt_block_header(trimmed) {
            in_skip_block = true;
            i += 1;
            continue;
        }

        if is_timing_line(trimmed) {
            i += 1;
            continue;
        }

        // Skip cue identifier lines (SRT numeric IDs or WebVTT named cue IDs) directly preceding a timing line.
        if lines
            .get(i + 1)
            .is_some_and(|next| is_timing_line(next.trim()))
        {
            i += 1;
            continue;
        }

        let (speaker, without_voice) = extract_voice_tag(trimmed);
        let stripped_tags = strip_inline_cue_tags(&without_voice);
        let decoded = super::feed::decode_xml_entities(&stripped_tags);
        let normalized = collapse_inline_whitespace(&decoded);
        let text = normalized.trim();

        if text.is_empty() {
            i += 1;
            continue;
        }

        let speaker_changed = match speaker {
            Some(ref s) if active_speaker.as_deref() != Some(s.as_str()) => {
                active_speaker = Some(s.clone());
                true
            }
            _ => false,
        };

        let dedup_key = (active_speaker.clone(), text.to_string());
        if last_emitted.as_ref() == Some(&dedup_key) {
            i += 1;
            continue;
        }
        last_emitted = Some(dedup_key);

        if speaker_changed {
            if let Some(ref s) = active_speaker {
                if is_markdown {
                    out_lines.push(format!("**[{s}]:** {text}"));
                } else {
                    out_lines.push(format!("[{s}]: {text}"));
                }
            } else {
                out_lines.push(text.to_string());
            }
        } else {
            out_lines.push(text.to_string());
        }

        i += 1;
    }

    out_lines.join("\n")
}

fn is_vtt_block_header(trimmed: &str) -> bool {
    trimmed == "STYLE"
        || trimmed == "REGION"
        || trimmed == "NOTE"
        || trimmed
            .strip_prefix("NOTE")
            .is_some_and(|r| r.starts_with([' ', '\t']))
}

fn is_timing_line(trimmed: &str) -> bool {
    let Some((left, right)) = trimmed.split_once("-->") else {
        return false;
    };
    looks_like_timestamp(left.trim())
        && looks_like_timestamp(right.split_whitespace().next().unwrap_or(""))
}

fn looks_like_timestamp(tok: &str) -> bool {
    if !tok.contains(':') {
        return false;
    }
    let mut has_digit = false;
    for b in tok.bytes() {
        match b {
            b'0'..=b'9' => has_digit = true,
            b':' | b'.' | b',' => {}
            _ => return false,
        }
    }
    has_digit
}

fn extract_voice_tag(line: &str) -> (Option<String>, String) {
    let mut speaker: Option<String> = None;
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'<'
            && i + 2 < bytes.len()
            && (bytes[i + 1] == b'v' || bytes[i + 1] == b'V')
            && matches!(bytes[i + 2], b' ' | b'\t' | b'.')
        {
            if let Some(rel_end) = line[i..].find('>') {
                let tag_body = &line[i + 2..i + rel_end];
                // Skip optional `.class1.class2` annotations before the speaker name.
                let name_part = if tag_body.starts_with('.') {
                    tag_body
                        .find(|c: char| c.is_ascii_whitespace())
                        .map(|idx| &tag_body[idx..])
                        .unwrap_or("")
                } else {
                    tag_body
                };
                let cleaned = super::feed::decode_xml_entities(name_part.trim());
                let cleaned = cleaned.trim();
                if !cleaned.is_empty() && speaker.is_none() {
                    speaker = Some(cleaned.to_string());
                }
                i += rel_end + 1;
                continue;
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    (speaker, out)
}

fn strip_inline_cue_tags(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'<' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'/' || next.is_ascii_alphabetic() || next.is_ascii_digit() {
                if let Some(rel_end) = line[i + 1..].find('>') {
                    let inner = &line[i + 1..i + 1 + rel_end];
                    if !inner.contains('<') {
                        i += rel_end + 2;
                        continue;
                    }
                }
            }
        }
        let ch = line[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}
