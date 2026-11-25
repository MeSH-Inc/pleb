use mesh_wick::extract::markdown::compact_markdown;
use mesh_wick::extract::{extract_document, ExtractMode, ExtractOptions, OutputFormat};
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
