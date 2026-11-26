use serde::Deserialize;
use std::collections::HashSet;
use url::Url;

use super::resources::{ResourceDescriptor, ResourceKind, ResourceSalience};
use super::{apply_format_limit, ExtractOptions, ExtractedDocument, OutputFormat};

const MARKERS: &[&str] = &[
    "var ytInitialPlayerResponse =",
    "var ytInitialPlayerResponse=",
    "window[\"ytInitialPlayerResponse\"] =",
    "window[\"ytInitialPlayerResponse\"]=",
];

#[derive(Debug, Deserialize)]
struct PlayerResponse {
    #[serde(rename = "videoDetails")]
    video_details: Option<VideoDetails>,
    captions: Option<CaptionsWrapper>,
}

#[derive(Debug, Deserialize)]
struct VideoDetails {
    title: Option<String>,
    author: Option<String>,
    #[serde(rename = "lengthSeconds")]
    length_seconds: Option<String>,
    #[serde(rename = "shortDescription")]
    short_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CaptionsWrapper {
    #[serde(rename = "playerCaptionsTracklistRenderer")]
    tracklist: Option<CaptionTracklist>,
}

#[derive(Debug, Deserialize)]
struct CaptionTracklist {
    #[serde(rename = "captionTracks", default)]
    caption_tracks: Vec<CaptionTrack>,
}

#[derive(Debug, Deserialize)]
struct CaptionTrack {
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(rename = "languageCode")]
    language_code: Option<String>,
    kind: Option<String>,
    name: Option<CaptionName>,
}

#[derive(Debug, Deserialize)]
struct CaptionName {
    #[serde(rename = "simpleText")]
    simple_text: Option<String>,
}

pub fn has_embedded_youtube_metadata(body: &str) -> bool {
    parse_player_response(body)
        .and_then(|p| p.video_details)
        .and_then(|v| v.title)
        .is_some_and(|t| !t.trim().is_empty())
}

pub fn extract_youtube_watch(
    body: &str,
    page_url: &Url,
    options: &ExtractOptions,
) -> Option<ExtractedDocument> {
    let parsed = parse_player_response(body)?;
    let details = parsed.video_details?;
    let title = details.title?.trim().to_string();
    if title.is_empty() {
        return None;
    }

    let author = details
        .author
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let duration = details
        .length_seconds
        .as_deref()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map(format_duration);
    let description = details
        .short_description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut out = String::new();
    if options.format == OutputFormat::Markdown {
        out.push_str(&format!("# {title}\n"));
        let mut meta_parts = Vec::new();
        if let Some(ch) = author {
            meta_parts.push(format!("**Channel:** {ch}"));
        }
        if let Some(dur) = duration.as_deref() {
            meta_parts.push(format!("**Duration:** {dur}"));
        }
        if !meta_parts.is_empty() {
            out.push('\n');
            out.push_str(&meta_parts.join(" | "));
            out.push('\n');
        }
        if let Some(desc) = description {
            out.push_str("\n## Description\n\n");
            out.push_str(desc);
        }
    } else {
        out.push_str(&title);
        if let Some(ch) = author {
            out.push_str(&format!("\nChannel: {ch}"));
        }
        if let Some(dur) = duration.as_deref() {
            out.push_str(&format!("\nDuration: {dur}"));
        }
        if let Some(desc) = description {
            out.push_str("\n\n");
            out.push_str(desc);
        }
    }

    let resources = extract_caption_resources(parsed.captions, page_url);
    let (content, truncated) = apply_format_limit(out.trim(), options.format, options.max_bytes);
    Some(ExtractedDocument {
        title: Some(title),
        content,
        truncated,
        resources,
    })
}

fn extract_caption_resources(
    captions: Option<CaptionsWrapper>,
    page_url: &Url,
) -> Vec<ResourceDescriptor> {
    let mut tracks = captions
        .and_then(|c| c.tracklist)
        .map(|t| t.caption_tracks)
        .unwrap_or_default();

    // Prefer manual (non-ASR) and English tracks first.
    tracks.sort_by_key(|t| {
        let is_asr = t.kind.as_deref() == Some("asr");
        let is_en = t
            .language_code
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("en") || l.starts_with("en-"));
        (!is_en, is_asr)
    });

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for track in tracks {
        if out.len() >= 5 {
            break;
        }
        let Some(raw_url) = track
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Ok(mut url) = page_url.join(raw_url) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        if !url.query_pairs().any(|(k, _)| k == "fmt") {
            url.query_pairs_mut().append_pair("fmt", "vtt");
        }
        let url_str = url.to_string();
        if !seen.insert(url_str.clone()) {
            continue;
        }
        let lang = track.language_code.as_deref().unwrap_or("unknown");
        let label = match track
            .name
            .as_ref()
            .and_then(|n| n.simple_text.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(name) => format!("{name} ({lang})"),
            None => format!("Captions ({lang})"),
        };
        out.push(ResourceDescriptor {
            identifier: format!("r{}", out.len() + 1),
            url: url_str,
            kind: ResourceKind::Transcript,
            salience: ResourceSalience::High,
            label: Some(label),
            section_heading: Some("Captions".to_string()),
            mime_hint: Some("text/vtt".to_string()),
            staged_path: None,
            dimensions: None,
            byte_count: None,
            content_type: None,
        });
    }
    out
}

fn parse_player_response(body: &str) -> Option<PlayerResponse> {
    if !body.contains("ytInitialPlayerResponse") {
        return None;
    }
    let json_slice = MARKERS
        .iter()
        .find_map(|marker| extract_balanced_json_after(body, marker))?;
    serde_json::from_str(json_slice).ok()
}

fn extract_balanced_json_after<'a>(body: &'a str, marker: &str) -> Option<&'a str> {
    let idx = body.find(marker)?;
    let rest = &body[idx + marker.len()..];
    let brace_offset = rest.find('{')?;
    let slice = &rest[brace_offset..];

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in slice.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&slice[..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{rem:02}")
    } else {
        format!("{mins}:{rem:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_youtube_metadata_description_and_caption_tracks() {
        let html = r#"<!doctype html><html><head><title>YouTube</title></head><body>
            <script>var ytInitialPlayerResponse = {"videoDetails":{"title":"Systems Demo {v2}","author":"Dev Channel","lengthSeconds":"213","shortDescription":"First line with } brace.\nSecond line."},"captions":{"playerCaptionsTracklistRenderer":{"captionTracks":[{"baseUrl":"https://www.youtube.com/api/timedtext?v=abc&lang=fr","languageCode":"fr","kind":"asr","name":{"simpleText":"French (auto)"}},{"baseUrl":"https://www.youtube.com/api/timedtext?v=abc&lang=en","languageCode":"en","name":{"simpleText":"English"}}]}}};</script>
        </body></html>"#;
        let url = Url::parse("https://www.youtube.com/watch?v=abc").unwrap();
        assert!(has_embedded_youtube_metadata(html));

        let doc = extract_youtube_watch(html, &url, &ExtractOptions::default()).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Systems Demo {v2}"));
        assert!(doc.content.contains("**Channel:** Dev Channel"));
        assert!(doc.content.contains("**Duration:** 3:33"));
        assert!(doc.content.contains("First line with } brace."));
        assert_eq!(doc.resources.len(), 2);
        assert_eq!(doc.resources[0].kind, ResourceKind::Transcript);
        assert_eq!(doc.resources[0].label.as_deref(), Some("English (en)"));
        assert!(doc.resources[0].url.contains("fmt=vtt"));
    }

    #[test]
    fn ignores_non_youtube_pages() {
        assert!(!has_embedded_youtube_metadata(
            "<html><body><p>Hello world</p></body></html>"
        ));
    }
}
