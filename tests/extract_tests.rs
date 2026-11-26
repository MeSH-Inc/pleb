use mesh_wick::extract::markdown::compact_markdown;
use mesh_wick::extract::url::{clean_url, resolve_and_clean_url, ResolvedUrl};
use mesh_wick::extract::{
    detect_binary_content, extract_document, truncate_markdown, truncate_utf8, ExtractMode,
    ExtractOptions, OutputFormat, ParsedHtml,
};
use url::Url;

#[test]
fn table_compaction_shrinks_padded_tables_and_preserves_fences() {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>Table Page</title></head>
<body>
<main>
<table>
  <tr><th>Rank</th><th>Story</th><th>Points</th></tr>
  <tr><td>1.</td><td><a href="https://example.com/1">A very long headline title that causes column padding in naive converters</a></td><td>420</td></tr>
  <tr><td>2.</td><td>Short</td><td>12</td></tr>
</table>
</main>
</body>
</html>"#;

    let url = Url::parse("https://news.ycombinator.com/").unwrap();
    let opts = ExtractOptions::default();
    let doc = extract_document(html, &url, Some("text/html"), &opts).unwrap();

    assert!(doc.content.contains("| Rank | Story | Points |"));
    assert!(doc.content.contains("| --- | --- | --- |"));
    assert!(!doc.content.contains("       "));

    let raw_md = "\
| A          | B                    |
| :--------- | -------------------: |
| 1          | two                  |

```text
|   keep     |   padded   |
|------------|------------|
```
";
    let compacted = compact_markdown(raw_md);
    assert!(compacted.contains("| A | B |"));
    assert!(compacted.contains("| :--- | ---: |"));
    assert!(compacted.contains("| 1 | two |"));
    assert!(compacted.contains("|   keep     |   padded   |"));
    assert!(compacted.contains("|------------|------------|"));
}

#[test]
fn clean_vs_article_vs_full_modes() {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>Site Title</title></head>
<body>
<nav><a href="/">Home</a><a href="/pricing">Pricing</a></nav>
<header><div class="brand">Global Nav Header</div></header>
<div class="cookie-banner">Accept cookies</div>
<aside>Sidebar Promo</aside>
<main>
  <div class="intro">Site Announcement Banner</div>
  <article>
    <h1>Deep Dive Article</h1>
    <p>First real paragraph of the article with enough text to easily exceed the threshold for selection in article extraction mode.</p>
    <p>Second paragraph discussing Rust ownership and memory safety details.</p>
  </article>
</main>
<footer>Copyright 2026</footer>
<script>window.__SECRET = "do_not_leak";</script>
<style>.hidden { display: none; }</style>
</body>
</html>"#;

    let url = Url::parse("https://example.com/post").unwrap();

    let clean = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Clean,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(clean.content.contains("Deep Dive Article"));
    assert!(clean.content.contains("Site Announcement Banner"));
    assert!(!clean.content.contains("Pricing"));
    assert!(!clean.content.contains("Global Nav Header"));
    assert!(!clean.content.contains("Accept cookies"));
    assert!(!clean.content.contains("Sidebar Promo"));
    assert!(!clean.content.contains("Copyright 2026"));
    assert!(!clean.content.contains("do_not_leak"));

    let article = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Article,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(article.content.contains("Deep Dive Article"));
    assert!(!article.content.contains("Site Announcement Banner"));
    assert!(!article.content.contains("Pricing"));

    let full = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(full.content.contains("Pricing"));
    assert!(full.content.contains("Copyright 2026"));
    assert!(!full.content.contains("do_not_leak"));
}

#[test]
fn plain_text_mode_omits_scripts_and_decodes_entities() {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>Plain &amp; Simple</title></head>
<body>
<script>const x = "secret_js_code";</script>
<style>body { color: red; }</style>
<main>
  <h1>Heading &amp; Title</h1>
  <p>It&#x27;s working &lt;cleanly&gt;.</p>
</main>
</body>
</html>"#;

    let url = Url::parse("https://example.com").unwrap();
    let doc = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Text,
            mode: ExtractMode::Clean,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(doc.title.as_deref(), Some("Plain & Simple"));
    assert!(!doc.content.contains("secret_js_code"));
    assert!(!doc.content.contains("color: red"));
    assert!(doc.content.contains("Heading & Title"));
    assert!(doc.content.contains("It's working <cleanly>."));
}

#[test]
fn unicode_uppercase_i_before_title_does_not_panic() {
    let html = "<html><head><!-- İ --> <title>İstanbul &amp; Ankara</title></head><body><p>İ test</p></body></html>";
    let url = Url::parse("https://example.com").unwrap();
    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert_eq!(doc.title.as_deref(), Some("İstanbul & Ankara"));
    assert!(doc.content.contains("İ test"));
}

#[test]
fn no_images_no_links_data_uri_and_utf8_max_bytes() {
    let html = r#"<html><body>
<img src="https://example.com/photo.jpg" alt="Photo">
<img src="data:image/png;base64,AAAA" alt="InlineData">
<p>Read <a href="https://example.com/docs">the docs</a> for details.</p>
<p>🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀🦀</p>
</body></html>"#;

    let url = Url::parse("https://example.com").unwrap();
    let doc = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Markdown,
            mode: ExtractMode::Clean,
            include_images: false,
            include_links: false,
            max_bytes: None,
        },
    )
    .unwrap();

    assert!(!doc.content.contains("photo.jpg"));
    assert!(!doc.content.contains("data:image/png"));
    assert!(doc.content.contains("Read the docs for details."));
    assert!(!doc.content.contains("https://example.com/docs"));

    let with_images = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            include_images: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(with_images.content.contains("photo.jpg"));
    assert!(!with_images.content.contains("data:image/png"));

    let truncated = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            max_bytes: Some(45),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(truncated.truncated);
    assert!(truncated.content.len() <= 45);
}

#[test]
fn non_html_passthrough_json_and_plain_text() {
    let url = Url::parse("https://api.example.com/v1/items").unwrap();
    let json_body = r#"{"items":[1,2,3],"title":"<not html>"}"#;
    let doc = extract_document(
        json_body,
        &url,
        Some("application/json; charset=utf-8"),
        &ExtractOptions::default(),
    )
    .unwrap();

    assert_eq!(doc.content, json_body);
    assert!(doc.title.is_none());

    let txt = "Line 1\nLine 2\n| raw | pipe |";
    let doc_txt = extract_document(
        txt,
        &url,
        Some("text/plain"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(doc_txt.content, txt);
}

fn assert_send<T: Send>() {}

#[test]
fn parsed_html_is_send_and_resolves_base_href_and_links() {
    assert_send::<ParsedHtml>();

    let html = r##"<!DOCTYPE html>
<html>
<head>
  <title>Docs Guide</title>
  <base href="https://cdn.example.com/v2/guide/">
</head>
<body>
  <main>
    <p>See <a href="getting-started?utm_source=x&amp;tab=rust">the <strong>Rust</strong> guide</a>.</p>
    <p>Jump to <a href="#overview">overview</a> or <a href="java&#x09;script:void(0)">expand details</a>.</p>
    <p>Empty <a href="https://example.com/empty">   </a> and image link <a href="https://example.com/big.jpg"><img src="thumb.png?fbclid=123" alt="Thumb"></a>.</p>
  </main>
</body>
</html>"##;

    let page_url = Url::parse("https://example.com/old/path").unwrap();
    let doc = extract_document(html, &page_url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert!(doc.content.contains("[the **Rust** guide](https://cdn.example.com/v2/guide/getting-started?tab=rust)"));
    assert!(doc.content.contains("[overview](#overview)"));
    assert!(doc.content.contains("expand details"));
    assert!(!doc.content.contains("javascript:"));
    assert!(!doc.content.contains("https://example.com/empty"));
    assert!(doc.content.contains("[![Thumb](https://cdn.example.com/v2/guide/thumb.png)](https://example.com/big.jpg)"));

    let no_img_doc = extract_document(
        html,
        &page_url,
        Some("text/html"),
        &ExtractOptions {
            include_images: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!no_img_doc.content.contains("big.jpg"));
    assert!(!no_img_doc.content.contains("thumb.png"));
}

#[test]
fn tracking_param_stripping_preserves_signed_urls_and_strips_case_insensitively() {
    let signed = Url::parse("https://s3.example.com/obj?X-Amz-Signature=a%2Bb%20c&response-content-type=text%2Fplain").unwrap();
    assert_eq!(clean_url(&signed).as_str(), signed.as_str());

    let tracked = Url::parse("https://example.com/post?UTM_SOURCE=news&fbclid=abc&gclid=xyz").unwrap();
    assert_eq!(clean_url(&tracked).as_str(), "https://example.com/post");

    let base = Url::parse("https://example.com/docs/").unwrap();
    assert_eq!(
        resolve_and_clean_url("page?utm_medium=social&id=42", &base),
        ResolvedUrl::Resolved("https://example.com/docs/page?id=42".to_string())
    );
    assert_eq!(
        resolve_and_clean_url("java\tscript:alert(1)", &base),
        ResolvedUrl::UnwrapText
    );
}

#[test]
fn truncate_markdown_closes_fences_without_contaminating_non_markdown() {
    let json_with_backticks = r#"{"snippet":"```rust\nfn main() {}\n```","extra":"0123456789abcdef0123456789abcdef"}"#;
    let (json_cut, json_truncated) = truncate_utf8(json_with_backticks, 50);
    assert!(json_truncated);
    assert!(json_cut.len() <= 50);
    assert!(json_cut.ends_with("\n\n[... truncated]"));
    assert!(!json_cut.contains("\n```\n\n[... truncated]"));

    let md_four_ticks = "````markdown\n# Inside\nline two\nline three\nline four\n````\n";
    let (md_cut, md_truncated) = truncate_markdown(md_four_ticks, 55);
    assert!(md_truncated);
    assert!(md_cut.len() <= 55);
    assert!(md_cut.ends_with("\n````\n\n[... truncated]"));

    let md_cut_on_opening = "Intro paragraph.\n\n```rust\nfn very_long_function_name() {}\n```\n";
    let (rewound, rewound_truncated) = truncate_markdown(md_cut_on_opening, 42);
    assert!(rewound_truncated);
    assert!(rewound.len() <= 42);
    assert!(!rewound.contains("```rust"));
    assert!(rewound.contains("Intro paragraph."));
}

#[test]
fn binary_detection_handles_mimes_magic_svg_and_latin1() {
    assert!(detect_binary_content(Some("application/pdf"), b"%PDF-1.7").is_some());
    assert!(detect_binary_content(None, b"%PDF-1.7\n...").is_some());
    assert!(detect_binary_content(None, b"abc\x00def").is_some());
    assert!(detect_binary_content(Some("image/png"), b"\x89PNG\r\n\x1a\n").is_some());

    assert!(detect_binary_content(Some("image/svg+xml"), b"<svg></svg>").is_none());
    assert!(detect_binary_content(Some("text/html; charset=iso-8859-1"), b"<p>Caf\xe9</p>").is_none());
}

#[test]
fn table_compaction_handles_backslash_before_multibyte_utf8() {
    let md = "| a\\é | b\\|c |\n| --- | --- |\n| 1 | 2\\ |";
    let compacted = compact_markdown(md);
    assert_eq!(compacted, "| a\\é | b\\|c |\n| --- | --- |\n| 1 | 2\\ |");
}
