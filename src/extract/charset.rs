//! Zero-dependency charset-aware text decoding for HTML and text payloads.

/// WHATWG Windows-1252 mapping for byte range `0x80..=0x9F`.
/// Undefined code points (`0x81`, `0x8D`, `0x8F`, `0x90`, `0x9D`) map to their
/// corresponding C1 control characters per the WHATWG Encoding Standard.
const WINDOWS_1252_80_9F: [char; 32] = [
    '\u{20AC}', // 0x80 €
    '\u{0081}', // 0x81
    '\u{201A}', // 0x82 ‚
    '\u{0192}', // 0x83 ƒ
    '\u{201E}', // 0x84 „
    '\u{2026}', // 0x85 …
    '\u{2020}', // 0x86 †
    '\u{2021}', // 0x87 ‡
    '\u{02C6}', // 0x88 ˆ
    '\u{2030}', // 0x89 ‰
    '\u{0160}', // 0x8A Š
    '\u{2039}', // 0x8B ‹
    '\u{0152}', // 0x8C Œ
    '\u{008D}', // 0x8D
    '\u{017D}', // 0x8E Ž
    '\u{008F}', // 0x8F
    '\u{0090}', // 0x90
    '\u{2018}', // 0x91 ‘
    '\u{2019}', // 0x92 ’
    '\u{201C}', // 0x93 “
    '\u{201D}', // 0x94 ”
    '\u{2022}', // 0x95 •
    '\u{2013}', // 0x96 –
    '\u{2014}', // 0x97 —
    '\u{02DC}', // 0x98 ˜
    '\u{2122}', // 0x99 ™
    '\u{0161}', // 0x9A š
    '\u{203A}', // 0x9B ›
    '\u{0153}', // 0x9C œ
    '\u{009D}', // 0x9D
    '\u{017E}', // 0x9E ž
    '\u{0178}', // 0x9F Ÿ
];

const META_SNIFF_LIMIT: usize = 2048;

/// Decode raw HTTP response bytes into a `String` using BOMs, valid UTF-8 fast-path,
/// `Content-Type` `charset=` parameter, or `<meta>` charset sniffing (first 2048 bytes).
///
/// Per WHATWG encoding rules, `iso-8859-1`, `latin1`, `latin-1`, `cp1252`, and
/// `windows-1252` are decoded with the Windows-1252 table. Unknown encodings fall
/// back to lossy UTF-8 decoding.
pub fn decode_text_bytes(body: &[u8], content_type: Option<&str>) -> String {
    if let Some(rest) = body.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = body.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = body.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }

    if let Ok(s) = std::str::from_utf8(body) {
        return s.to_string();
    }

    let charset =
        extract_charset_from_content_type(content_type).or_else(|| sniff_meta_charset(body));

    match charset.as_deref() {
        Some(
            "windows-1252" | "cp1252" | "x-cp1252" | "iso-8859-1" | "iso_8859-1" | "latin1"
            | "latin-1" | "l1" | "us-ascii" | "ascii" | "ansi_x3.4-1968",
        ) => decode_windows_1252(body),
        Some("utf-16" | "utf-16le") => decode_utf16(body, u16::from_le_bytes),
        Some("utf-16be") => decode_utf16(body, u16::from_be_bytes),
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

fn decode_windows_1252(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 8);
    for &b in bytes {
        if (0x80..=0x9F).contains(&b) {
            out.push(WINDOWS_1252_80_9F[(b - 0x80) as usize]);
        } else {
            out.push(char::from(b));
        }
    }
    out
}

fn decode_utf16(bytes: &[u8], from_bytes: fn([u8; 2]) -> u16) -> String {
    let (chunks, rem) = bytes.as_chunks::<2>();
    let iter = chunks.iter().copied().map(from_bytes);
    let mut out: String = char::decode_utf16(iter)
        .map(|res| res.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !rem.is_empty() {
        out.push(char::REPLACEMENT_CHARACTER);
    }
    out
}

pub(crate) fn extract_charset_from_content_type(content_type: Option<&str>) -> Option<String> {
    let ct = content_type?;
    for param in ct.split(';').skip(1) {
        let Some((key, val)) = param.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("charset") {
            let unquoted = strip_quotes(val.trim()).trim();
            if !unquoted.is_empty() {
                return Some(unquoted.to_ascii_lowercase());
            }
        }
    }
    None
}

fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Sniff `<meta charset="...">` or `<meta http-equiv="Content-Type" content="...charset=...">`
/// within the first `META_SNIFF_LIMIT` bytes, skipping HTML comments.
fn sniff_meta_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(META_SNIFF_LIMIT)];
    let mut i = 0;

    while i < head.len() {
        if head[i] != b'<' {
            i += 1;
            continue;
        }

        if head[i..].starts_with(b"<!--") {
            if let Some(rel) = find_subslice(&head[i + 4..], b"-->") {
                i += 4 + rel + 3;
                continue;
            } else {
                break;
            }
        }

        if head.len() - i >= 5
            && head[i + 1..i + 5].eq_ignore_ascii_case(b"meta")
            && (i + 5 == head.len() || head[i + 5].is_ascii_whitespace() || head[i + 5] == b'/')
        {
            let tag_start = i + 5;
            let Some(tag_end_rel) = head[tag_start..].iter().position(|&b| b == b'>') else {
                break;
            };
            let tag_bytes = &head[tag_start..tag_start + tag_end_rel];
            if let Some(charset) = parse_meta_tag_charset(tag_bytes) {
                return Some(charset);
            }
            i = tag_start + tag_end_rel + 1;
            continue;
        }

        i += 1;
    }

    None
}

fn parse_meta_tag_charset(tag: &[u8]) -> Option<String> {
    let mut i = 0;
    let mut is_content_type = false;
    let mut content_value: Option<String> = None;

    while i < tag.len() {
        while i < tag.len() && (tag[i].is_ascii_whitespace() || tag[i] == b'/') {
            i += 1;
        }
        if i >= tag.len() {
            break;
        }

        let name_start = i;
        while i < tag.len() && !tag[i].is_ascii_whitespace() && tag[i] != b'=' && tag[i] != b'/' {
            i += 1;
        }
        let name = &tag[name_start..i];

        while i < tag.len() && tag[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() || tag[i] != b'=' {
            continue;
        }
        i += 1; // skip '='

        while i < tag.len() && tag[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= tag.len() {
            break;
        }

        let val_bytes = if tag[i] == b'"' || tag[i] == b'\'' {
            let quote = tag[i];
            i += 1;
            let start = i;
            while i < tag.len() && tag[i] != quote {
                i += 1;
            }
            let slice = &tag[start..i];
            if i < tag.len() {
                i += 1;
            }
            slice
        } else {
            let start = i;
            while i < tag.len() && !tag[i].is_ascii_whitespace() {
                i += 1;
            }
            &tag[start..i]
        };

        if name.eq_ignore_ascii_case(b"charset") {
            let val = std::str::from_utf8(val_bytes).ok()?.trim();
            if !val.is_empty() {
                return Some(val.to_ascii_lowercase());
            }
        } else if name.eq_ignore_ascii_case(b"http-equiv") {
            if let Ok(val) = std::str::from_utf8(val_bytes) {
                if val.trim().eq_ignore_ascii_case("content-type") {
                    is_content_type = true;
                }
            }
        } else if name.eq_ignore_ascii_case(b"content") {
            if let Ok(val) = std::str::from_utf8(val_bytes) {
                content_value = Some(val.to_string());
            }
        }
    }

    if is_content_type {
        if let Some(ref content) = content_value {
            return extract_charset_from_content_type(Some(content));
        }
    }

    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
