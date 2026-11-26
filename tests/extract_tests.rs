use mesh_wick::extract::markdown::{compact_markdown, open_fence_at};
use mesh_wick::extract::url::{clean_url, resolve_and_clean_url, ResolvedUrl};
use mesh_wick::extract::{
    decode_text_bytes, detect_binary_content, extract_document, extract_document_bytes,
    format_resource_manifest_markdown, truncate_markdown, truncate_utf8, wrap_raw_code_document,
    ExtractMode, ExtractOptions, OutputFormat, ParsedHtml, ResourceKind, ResourceSalience,
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
    let doc_txt =
        extract_document(txt, &url, Some("text/plain"), &ExtractOptions::default()).unwrap();
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
    let doc = extract_document(
        html,
        &page_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(doc.content.contains(
        "[the **Rust** guide](https://cdn.example.com/v2/guide/getting-started?tab=rust)"
    ));
    assert!(doc.content.contains("[overview](#overview)"));
    assert!(doc.content.contains("expand details"));
    assert!(!doc.content.contains("javascript:"));
    assert!(!doc.content.contains("https://example.com/empty"));
    assert!(doc.content.contains(
        "[![Thumb](https://cdn.example.com/v2/guide/thumb.png)](https://example.com/big.jpg)"
    ));

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
    let signed = Url::parse(
        "https://s3.example.com/obj?X-Amz-Signature=a%2Bb%20c&response-content-type=text%2Fplain",
    )
    .unwrap();
    assert_eq!(clean_url(&signed).as_str(), signed.as_str());

    let tracked =
        Url::parse("https://example.com/post?UTM_SOURCE=news&fbclid=abc&gclid=xyz").unwrap();
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
    let json_with_backticks =
        r#"{"snippet":"```rust\nfn main() {}\n```","extra":"0123456789abcdef0123456789abcdef"}"#;
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
    assert!(
        detect_binary_content(Some("text/html; charset=iso-8859-1"), b"<p>Caf\xe9</p>").is_none()
    );
}

#[test]
fn table_compaction_handles_backslash_before_multibyte_utf8() {
    let md = "| a\\é | b\\|c |\n| --- | --- |\n| 1 | 2\\ |";
    let compacted = compact_markdown(md);
    assert_eq!(compacted, "| a\\é | b\\|c |\n| --- | --- |\n| 1 | 2\\ |");
}

#[test]
fn truncate_markdown_exhaustive_byte_sweep_preserves_fence_balance() {
    let inputs = [
        "```rust\nfn main(){}\n```",
        "```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n",
        "````markdown\n```rust\nfn nested() {}\n```\n````\n",
        "~~~python\ndef greet():\n    return 'hi'\n~~~\n",
        "Intro\n\n```rust\nlet crab = \"🦀🦀🦀\";\nlet accent = \"café naïve\";\n```\nAfter.",
        "```text\n```not-a-closing-fence\nline\n```\n",
        "```\n🦀\n```\n\n~~~sh\necho café\n~~~",
    ];

    for input in inputs {
        for max_bytes in 0..=input.len() {
            let (out, truncated) = truncate_markdown(input, max_bytes);
            assert!(
                out.len() <= max_bytes,
                "out.len()={} > max_bytes={} for input={:?} out={:?}",
                out.len(),
                max_bytes,
                input,
                out
            );
            assert_eq!(truncated, max_bytes < input.len());
            assert!(
                open_fence_at(&out, out.len()).is_none(),
                "unclosed fence at max_bytes={} for input={:?}: out={:?}",
                max_bytes,
                input,
                out
            );
        }
    }

    // Regression check for small budget (<= TRUNCATED_SUFFIX.len()) starting with a code fence.
    let (small, small_truncated) = truncate_markdown("```rust\nfn main(){}\n```", 10);
    assert!(small_truncated);
    assert_eq!(small, "");
    assert!(open_fence_at(&small, small.len()).is_none());
}

#[test]
fn charset_decoding_handles_windows1252_latin1_meta_and_boms() {
    // Header-declared ISO-8859-1 / Windows-1252 bytes: é (0xE9), “ (0x93), ” (0x94), € (0x80).
    let win1252_bytes = b"<p>Caf\xe9 costs \x8012 \x93today\x94</p>";
    for ct in [
        "text/html; charset=iso-8859-1",
        "text/html; charset=latin1",
        "text/html; charset=latin-1",
        "text/html; charset=cp1252",
        "text/html; charset=windows-1252",
        "text/html; CHARSET=\"Windows-1252\"",
    ] {
        let decoded = decode_text_bytes(win1252_bytes, Some(ct));
        assert_eq!(decoded, "<p>Café costs €12 “today”</p>", "failed for {ct}");
        assert!(!decoded.contains('\u{FFFD}'));
    }

    // Meta <meta charset="windows-1252"> sniffing when Content-Type omits charset.
    let meta_short = b"<!DOCTYPE html><html><head><meta charset=\"windows-1252\"><title>Caf\xe9</title></head><body><p>\x805 \x93ok\x94</p></body></html>";
    let decoded_short = decode_text_bytes(meta_short, Some("text/html"));
    assert!(decoded_short.contains("<title>Café</title>"));
    assert!(decoded_short.contains("<p>€5 “ok”</p>"));
    assert!(!decoded_short.contains('\u{FFFD}'));

    // Meta <meta http-equiv="Content-Type" content="text/html; charset=iso-8859-1"> sniffing.
    let meta_equiv = b"<html><head><!-- <meta charset=\"utf-8\"> --><meta http-equiv=\"Content-Type\" content=\"text/html; charset=iso-8859-1\"></head><body><p>Caf\xe9 \x93quote\x94 \x80</p></body></html>";
    let decoded_equiv = decode_text_bytes(meta_equiv, None);
    assert!(decoded_equiv.contains("Café “quote” €"));
    assert!(!decoded_equiv.contains('\u{FFFD}'));

    // UTF-8 BOM and UTF-16LE / UTF-16BE BOMs.
    let utf8_bom = b"\xEF\xBB\xBF<p>Caf\xC3\xA9</p>";
    assert_eq!(decode_text_bytes(utf8_bom, None), "<p>Café</p>");

    let utf16_le = b"\xFF\xFE<\x00p\x00>\x00\xAC\x20<\x00/\x00p\x00>\x00";
    assert_eq!(decode_text_bytes(utf16_le, None), "<p>€</p>");

    let utf16_be = b"\xFE\xFF\x00<\x00p\x00>\x20\xAC\x00<\x00/\x00p\x00>";
    assert_eq!(decode_text_bytes(utf16_be, None), "<p>€</p>");

    // Unknown charset falls back to lossy UTF-8 without panicking.
    let unknown = decode_text_bytes(b"Caf\xe9", Some("text/html; charset=x-unknown-999"));
    assert_eq!(unknown, "Caf\u{FFFD}");

    // End-to-end extract_document_bytes wiring.
    let url = Url::parse("https://example.com/cafe").unwrap();
    let doc = extract_document_bytes(
        meta_short,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(doc.title.as_deref(), Some("Café"));
    assert!(doc.content.contains("€5 “ok”"));
    assert!(!doc.content.contains('\u{FFFD}'));
}

#[test]
fn adversarial_charset_and_four_backtick_utf8_fence_boundaries() {
    use mesh_wick::extract::markdown::open_fence_at;

    // Odd-byte UTF-16LE/BE payloads after BOM emit U+FFFD cleanly without panicking.
    let odd_le = decode_text_bytes(b"\xFF\xFEa\x00\x42", None);
    assert_eq!(odd_le, "a\u{FFFD}");
    let odd_be = decode_text_bytes(b"\xFE\xFF\x00a\x42", None);
    assert_eq!(odd_be, "a\u{FFFD}");

    // Content-Type charset parameter with surrounding spaces, quotes, and prior params.
    let spaced_ct = decode_text_bytes(
        b"<p>Caf\xe9 \x97 \x93ok\x94</p>",
        Some("text/html; version=1; charset = \"Windows-1252\""),
    );
    assert_eq!(spaced_ct, "<p>Café — “ok”</p>");

    // Unclosed HTML comment before <meta> must not panic or hang.
    let unclosed_comment = decode_text_bytes(b"<!-- unclosed comment \xe9", Some("text/html"));
    assert!(unclosed_comment.contains('\u{FFFD}'));

    // 4-backtick fence starting at byte 0 with multibyte UTF-8 characters: exhaustive sweep.
    let md = "````rust\nlet price = \"€12 — café 🚀\";\nprintln!(\"{price}\");\n````\n";
    for max_bytes in 0..=md.len() {
        let (out, truncated) = truncate_markdown(md, max_bytes);
        assert!(
            out.len() <= max_bytes,
            "max_bytes={max_bytes}: out.len()={} exceeded budget",
            out.len()
        );
        assert_eq!(truncated, max_bytes < md.len());
        assert!(
            open_fence_at(&out, out.len()).is_none(),
            "max_bytes={max_bytes}: unclosed 4-backtick fence in {out:?}"
        );
    }
}

#[test]
fn node_metrics_scales_linearly_with_dom_size() {
    let build_doc = |count: usize| -> String {
        let mut html = String::from("<html><body><main>");
        for i in 0..count {
            html.push_str(&format!("<p><a href=\"/p/{i}\">item {i}</a> text</p>"));
        }
        html.push_str("</main></body></html>");
        html
    };

    let small = ParsedHtml::parse(&build_doc(250));
    let (_, visits_small) = small.compute_node_metrics();

    let large = ParsedHtml::parse(&build_doc(2_500));
    let (_, visits_large) = large.compute_node_metrics();

    let ratio = visits_large as f64 / visits_small.max(1) as f64;
    assert!(
        ratio <= 12.0,
        "expected O(N) scaling (ratio <= 12.0), got {ratio:.2} ({visits_large} / {visits_small})"
    );
}

#[test]
fn wave1_extraction_preserves_data_tables_prose_links_svg_diagrams_code_pipes_and_title_dedup() {
    let html = r##"<!DOCTYPE html>
<html>
<head><title>Quickstart | Acme Docs</title></head>
<body>
  <main>
    <h1>Quickstart</h1>
    <p>Learn <a href="#csharp">C#</a> and inspect the architecture:</p>
    <svg aria-label="Request Pipeline"><path d="M0 0 L10 10"/></svg>
    <button><svg aria-label="Ignored Icon"><path d="M0 0"/></svg>Click</button>
    <details><summary>More info</summary><p>Hidden detail text.</p></details>
    <table>
      <tr><th>Line</th><th>Code</th></tr>
      <tr><td>1</td><td><code>a | b</code></td></tr>
      <tr><td>2</td><td><code>c | d</code></td></tr>
      <tr><td>3</td><td><code>e | f</code></td></tr>
    </table>
  </main>
</body>
</html>"##;

    let url = Url::parse("https://docs.example.com/quickstart").unwrap();
    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();

    // Title prefix deduplication: only one `# Quickstart`, no `# Quickstart | Acme Docs`.
    assert!(!doc.content.contains("# Quickstart | Acme Docs"));
    assert_eq!(doc.content.matches("# Quickstart").count(), 1);

    // Prose `[C#](#csharp)` link is NOT stripped.
    assert!(doc.content.contains("[C#](#csharp)"));

    // Labeled SVG outside interactive controls becomes `[Diagram: Request Pipeline]`.
    assert!(doc.content.contains("[Diagram: Request Pipeline]"));
    assert!(!doc.content.contains("[Diagram: Ignored Icon]"));

    // 2-column data table WITH `<th>` stays a Markdown table (not unwrapped as code gutter).
    assert!(doc.content.contains("| Line | Code |"));
    assert!(doc.content.contains("| 1 |"));
    assert!(!doc.content.contains("```"));

    // `split_table_cells` ignores `|` inside inline backtick code spans during compaction.
    let raw_pipe_table = "| Col A      | Col B          |\n| ---------- | -------------- |\n| 1          | `a | b`        |";
    assert_eq!(
        compact_markdown(raw_pipe_table),
        "| Col A | Col B |\n| --- | --- |\n| 1 | `a | b` |"
    );

    // `<details>` / `<summary>` emit block breaks in plain text mode.
    let txt = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(txt.content.contains("More info\nHidden detail text."));
    assert!(txt.content.contains("[Diagram: Request Pipeline]"));
}

#[test]
fn adversarial_wave1_heading_link_code_span_and_thead_table_false_positives() {
    let html = r##"<!DOCTYPE html>
<html>
<head><title>Adversarial False Positives</title></head>
<body>
  <main>
    <h1>Adversarial False Positives</h1>
    <h2 id="csharp">Using <a href="#csharp">C#</a> Interop <a class="headerlink" href="#csharp">#</a></h2>
    <p>Syntax spans containing the word Copy inside code blocks must survive:</p>
    <pre><button type="button">Copy</button><code class="language-rust">let action = <span class="tok-str">"Copy"</span>;</code></pre>
    <table>
      <thead><tr><th>Step</th><th>Snippet</th></tr></thead>
      <tbody>
        <tr><td>1</td><td><code>cargo check</code></td></tr>
        <tr><td>2</td><td><code>cargo test</code></td></tr>
        <tr><td>3</td><td><code>cargo clippy</code></td></tr>
        <tr><td>4</td><td><code>cargo fmt --check</code></td></tr>
      </tbody>
    </table>
  </main>
</body>
</html>"##;

    let url = Url::parse("https://docs.example.com/adversarial").unwrap();
    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();

    // Meaningful `[C#](#csharp)` link inside `<h2>` survives while trailing `#` permalink is stripped.
    assert!(
        doc.content.contains("[C#](#csharp)"),
        "meaningful C# link inside heading must survive: {}",
        doc.content
    );
    assert!(
        !doc.content.contains("[#](#csharp)"),
        "trailing permalink # should be stripped: {}",
        doc.content
    );

    // Syntax `<span>` literal `"Copy"` survives inside `<pre><code>` while `<button>Copy</button>` is stripped.
    assert!(
        doc.content.contains("let action = \"Copy\";"),
        "syntax span inside <pre><code> must survive: {}",
        doc.content
    );

    // 4-row 2-column table with `<thead>`/`<th>` stays a Markdown table.
    assert!(doc.content.contains("| Step | Snippet |"));
    assert!(doc.content.contains("| 4 | `cargo fmt --check` |"));
}

#[test]
fn html_markdown_alternate_link_discovery_and_ssrf_guard() {
    let page = Url::parse("https://docs.example.com/guide/intro").unwrap();

    // Standard relative alternate markdown link.
    let html_rel = r#"<html><head>
        <link rel="Alternate" type="text/markdown; charset=utf-8" href="./intro.md">
    </head><body><p>Hello</p></body></html>"#;
    assert_eq!(
        ParsedHtml::parse(html_rel)
            .markdown_alternate_url(&page)
            .map(|u| u.to_string())
            .as_deref(),
        Some("https://docs.example.com/guide/intro.md")
    );

    // Same registrable parent domain (subdomain sibling) is allowed.
    let html_sub = r#"<html><head>
        <link rel="alternate" type="text/x-markdown" href="https://raw.example.com/guide/intro.md">
    </head><body></body></html>"#;
    assert_eq!(
        ParsedHtml::parse(html_sub)
            .markdown_alternate_url(&page)
            .map(|u| u.to_string())
            .as_deref(),
        Some("https://raw.example.com/guide/intro.md")
    );

    // Multi-part ccTLD registrable domain check (`co.uk`).
    let uk_page = Url::parse("https://docs.acme.co.uk/api").unwrap();
    let uk_same = r#"<html><head><link rel="alternate" type="text/markdown" href="https://raw.acme.co.uk/api.md"></head></html>"#;
    let uk_other = r#"<html><head><link rel="alternate" type="text/markdown" href="https://raw.evil.co.uk/api.md"></head></html>"#;
    assert!(ParsedHtml::parse(uk_same)
        .markdown_alternate_url(&uk_page)
        .is_some());
    assert!(ParsedHtml::parse(uk_other)
        .markdown_alternate_url(&uk_page)
        .is_none());

    // Blocked targets: third-party host, HTTPS->HTTP downgrade, self-link, loopback/private IPs, non-HTTP scheme.
    for blocked_href in [
        "https://evil.com/intro.md",
        "https://example.com.evil.com/intro.md",
        "http://docs.example.com/guide/intro.md",
        "https://docs.example.com/guide/intro",
        "https://localhost/intro.md",
        "https://127.0.0.1/intro.md",
        "https://[::1]/intro.md",
        "https://10.0.0.1/intro.md",
        "https://172.16.0.5/intro.md",
        "https://192.168.1.10/intro.md",
        "https://169.254.169.254/latest/meta-data/",
        "file:///etc/passwd",
        "javascript:alert(1)",
    ] {
        let html = format!(
            r#"<html><head><link rel="alternate" type="text/markdown" href="{blocked_href}"></head><body></body></html>"#
        );
        assert!(
            ParsedHtml::parse(&html)
                .markdown_alternate_url(&page)
                .is_none(),
            "expected {blocked_href} to be rejected"
        );
    }

    // Loopback page URL allows same-origin loopback alternate link.
    let loopback_page = Url::parse("http://127.0.0.1:8080/docs").unwrap();
    let html_loopback =
        r#"<html><head><link rel="alternate" type="text/markdown" href="/docs.md"></head></html>"#;
    assert_eq!(
        ParsedHtml::parse(html_loopback)
            .markdown_alternate_url(&loopback_page)
            .map(|u| u.to_string())
            .as_deref(),
        Some("http://127.0.0.1:8080/docs.md")
    );

    // `<link>` inside `<body>` is ignored.
    let html_body_link = r#"<html><head></head><body><link rel="alternate" type="text/markdown" href="/body.md"></body></html>"#;
    assert!(ParsedHtml::parse(html_body_link)
        .markdown_alternate_url(&page)
        .is_none());
}

#[test]
fn native_markdown_frontmatter_horizontal_rules_links_and_truncation() {
    let url = Url::parse("https://docs.example.com/guides/start.md").unwrap();
    let md = "---\r\ntitle: \"Getting Started\"\r\ndescription: Quick guide\r\n---\r\n\r\nSee [Install](./install.md?utm_source=nav&tab=1) and ![Arch](/img/arch.png?fbclid=9).\r\n\r\nInline code `[keep](./raw.md)` and `![keep](/raw.png)` stay untouched.\r\n\r\n```rust\n// [code](./ignored.md) and # Not Title\nlet x = 1;\n```\n\n---\n\nFooter section.\n";

    // Clean mode: strips frontmatter, extracts title, injects `# Getting Started`, resolves links/images.
    let clean =
        extract_document(md, &url, Some("text/markdown"), &ExtractOptions::default()).unwrap();
    assert_eq!(clean.title.as_deref(), Some("Getting Started"));
    assert!(clean.content.starts_with("# Getting Started\n\n"));
    assert!(!clean.content.contains("description: Quick guide"));
    assert!(clean
        .content
        .contains("[Install](https://docs.example.com/guides/install.md?tab=1)"));
    assert!(clean
        .content
        .contains("![Arch](https://docs.example.com/img/arch.png)"));
    assert!(clean.content.contains("`[keep](./raw.md)`"));
    assert!(clean.content.contains("// [code](./ignored.md)"));
    assert!(clean.content.contains("\n---\n\nFooter section."));

    // Full mode: preserves YAML frontmatter.
    let full = extract_document(
        md,
        &url,
        Some("text/x-markdown; charset=utf-8"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(full.title.as_deref(), Some("Getting Started"));
    assert!(full
        .content
        .starts_with("---\r\ntitle: \"Getting Started\""));
    assert!(full.content.contains("description: Quick guide"));

    // `--no-images` and `--no-links` in native Markdown.
    let stripped = extract_document(
        md,
        &url,
        Some("text/markdown"),
        &ExtractOptions {
            include_images: false,
            include_links: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(stripped.content.contains("See Install and ."));
    assert!(!stripped.content.contains("install.md"));
    assert!(!stripped.content.contains("arch.png"));

    // HTML output format keeps native Markdown verbatim.
    let as_html = extract_document(
        md,
        &url,
        Some("text/markdown"),
        &ExtractOptions {
            format: OutputFormat::Html,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(as_html.content, md);

    // Text output format strips Markdown syntax cleanly.
    let as_text = extract_document(
        md,
        &url,
        Some("text/markdown"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(as_text.title.as_deref(), Some("Getting Started"));
    assert!(as_text.content.contains("See Install and Arch."));
    assert!(!as_text.content.contains("```rust"));

    // Mid-document `---` horizontal rules are NEVER mistaken for frontmatter; fallback title skips code fences.
    let hr_md = "```sh\n# Shell Comment\n```\n\n# Real Heading\n\n---\n\nBody paragraph.\n\n---\n";
    let hr_doc = extract_document(
        hr_md,
        &url,
        Some("text/markdown"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(hr_doc.title.as_deref(), Some("Real Heading"));
    assert!(hr_doc.content.contains("# Shell Comment"));
    assert!(hr_doc.content.contains("Body paragraph."));

    // Fence-safe truncation under `--max-bytes`.
    let trunc = extract_document(
        md,
        &url,
        Some("text/markdown"),
        &ExtractOptions {
            max_bytes: Some(260),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(trunc.truncated);
    assert!(trunc.content.len() <= 260);
    assert!(open_fence_at(&trunc.content, trunc.content.len()).is_none());
}

#[test]
fn wrap_raw_code_document_escapes_backtick_fences_and_handles_formats() {
    let code_with_ticks = "let doc = \"```rust\\nfn hi() {}\\n```\";\n";
    let md_doc = wrap_raw_code_document(
        "example.rs",
        code_with_ticks,
        Some("rust"),
        &ExtractOptions::default(),
    );
    assert_eq!(md_doc.title.as_deref(), Some("example.rs"));
    assert!(md_doc.content.starts_with("# example.rs\n\n````rust\n"));
    assert!(md_doc.content.ends_with("\n````"));
    assert!(open_fence_at(&md_doc.content, md_doc.content.len()).is_none());

    // Truncated markdown code fence remains balanced.
    let trunc_md = wrap_raw_code_document(
        "example.rs",
        code_with_ticks,
        Some("rust"),
        &ExtractOptions {
            max_bytes: Some(50),
            ..Default::default()
        },
    );
    assert!(trunc_md.truncated);
    assert!(trunc_md.content.len() <= 50);
    assert!(open_fence_at(&trunc_md.content, trunc_md.content.len()).is_none());

    // Text format returns raw code verbatim.
    let txt_doc = wrap_raw_code_document(
        "example.rs",
        code_with_ticks,
        Some("rust"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    );
    assert_eq!(txt_doc.title.as_deref(), Some("example.rs"));
    assert_eq!(txt_doc.content, code_with_ticks);
}

#[test]
fn feed_extraction_handles_rss_atom_json_feed_and_full_passthrough() {
    let url = Url::parse("https://blog.example.com/feed.xml").unwrap();

    // RSS 2.0 via `application/xml` with BOM, XML decl, and comment before `<rss>`.
    let rss = "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- feed -->\n<rss version=\"2.0\" xmlns:content=\"http://purl.org/rss/1.0/modules/content/\">\
        <channel>\
          <title>Acme &amp; Co Blog</title>\
          <description>Release notes</description>\
          <item>\
            <title>Release 1.0</title>\
            <link>/posts/v1?utm_source=rss&amp;id=7</link>\
            <pubDate>Thu, 26 Nov 2026 10:00:00 GMT</pubDate>\
            <description><![CDATA[<p>Short summary</p>]]></description>\
            <content:encoded><![CDATA[<p>Full <strong>HTML</strong> with <a href=\"/changelog?fbclid=1\">changelog</a> and <img src=\"/pic.png\" alt=\"Pic\"></p>]]></content:encoded>\
          </item>\
        </channel>\
      </rss>";

    let rss_clean = extract_document(
        rss,
        &url,
        Some("application/xml"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(rss_clean.title.as_deref(), Some("Acme & Co Blog"));
    assert!(rss_clean.content.contains("# Acme & Co Blog"));
    assert!(rss_clean
        .content
        .contains("## [Release 1.0](https://blog.example.com/posts/v1?id=7)"));
    assert!(rss_clean
        .content
        .contains("*Thu, 26 Nov 2026 10:00:00 GMT*"));
    assert!(rss_clean
        .content
        .contains("[changelog](https://blog.example.com/changelog)"));
    assert!(rss_clean
        .content
        .contains("![Pic](https://blog.example.com/pic.png)"));

    // RSS Full mode preserves raw XML verbatim.
    let rss_full = extract_document(
        rss,
        &url,
        Some("application/rss+xml"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(rss_full.content, rss);

    // Atom 1.0 via `application/atom+xml`.
    let atom = r#"<?xml version="1.0" encoding="utf-8"?>
      <feed xmlns="http://www.w3.org/2005/Atom">
        <title>Rust Releases</title>
        <subtitle>Official updates</subtitle>
        <entry>
          <title>Rust 1.90</title>
          <link rel="self" href="https://blog.example.com/self" />
          <link rel="alternate" href="/rust-1-90?utm_medium=feed" />
          <published>2026-11-26T00:00:00Z</published>
          <summary type="html">&lt;p&gt;Faster &lt;em&gt;builds&lt;/em&gt;.&lt;/p&gt;</summary>
        </entry>
      </feed>"#;
    let atom_doc = extract_document(
        atom,
        &url,
        Some("application/atom+xml"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(atom_doc.title.as_deref(), Some("Rust Releases"));
    assert!(atom_doc
        .content
        .contains("## [Rust 1.90](https://blog.example.com/rust-1-90)"));
    assert!(atom_doc.content.contains("*2026-11-26T00:00:00Z*"));
    assert!(atom_doc.content.contains("Faster *builds*."));

    // JSON Feed 1.1 with `--no-links` and `--no-images`.
    let json_feed = r#"{
        "version": "https://jsonfeed.org/version/1.1",
        "title": "JSON News",
        "items": [
            {
                "id": "1",
                "title": "Hello JSON Feed",
                "url": "/news/1?utm_campaign=launch",
                "date_published": "2026-11-26T12:00:00Z",
                "content_html": "<p>Read <a href=\"/more\">more</a><img src=\"/hero.png\" alt=\"Hero\"></p>"
            }
        ]
    }"#;
    let jf_doc = extract_document(
        json_feed,
        &url,
        Some("application/feed+json"),
        &ExtractOptions {
            include_links: false,
            include_images: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(jf_doc.title.as_deref(), Some("JSON News"));
    assert!(jf_doc.content.contains("# JSON News"));
    assert!(jf_doc.content.contains("## Hello JSON Feed"));
    assert!(!jf_doc.content.contains("blog.example.com/news/1"));
    assert!(!jf_doc.content.contains("hero.png"));
    assert!(jf_doc.content.contains("Read more"));

    // Non-feed XML still passes through verbatim.
    let plain_xml = "<?xml version=\"1.0\"?><catalog><item>A</item></catalog>";
    let xml_doc = extract_document(
        plain_xml,
        &url,
        Some("application/xml"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(xml_doc.content, plain_xml);
}

#[test]
fn adversarial_m3_base_hijack_unclosed_frontmatter_five_tick_fence_and_feed_cap() {
    let page = Url::parse("https://docs.example.com/guide").unwrap();

    // 1. `<base href="https://evil.com/">` cannot hijack relative `<link rel="alternate" type="text/markdown">`
    //    past the same-origin / registrable-domain SSRF guard.
    let hijack_html = r#"<html><head>
        <base href="https://evil.com/">
        <link rel="alternate" type="text/markdown" href="/secret.md">
    </head><body><p>Body</p></body></html>"#;
    assert!(
        ParsedHtml::parse(hijack_html)
            .markdown_alternate_url(&page)
            .is_none(),
        "<base> redirect to third-party host must be rejected by markdown_alternate_url"
    );

    // 2. Leading `---` without a closing delimiter must NOT swallow the document as YAML frontmatter.
    let unclosed_hr = "---\n\n# Real Title\n\nUnclosed horizontal rule prose survives.\n";
    let md_doc = extract_document(
        unclosed_hr,
        &page,
        Some("text/markdown"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(
        md_doc
            .content
            .contains("Unclosed horizontal rule prose survives."),
        "unclosed leading --- swallowed document body: {}",
        md_doc.content
    );

    // 3. Raw code containing 4-backtick fences must be wrapped in a 5-backtick outer fence.
    let four_tick_code = "````markdown\n# Nested\n````\n";
    let wrapped = wrap_raw_code_document(
        "nested.md",
        four_tick_code,
        Some("markdown"),
        &ExtractOptions::default(),
    );
    assert!(
        wrapped.content.contains("`````markdown\n"),
        "expected 5-backtick outer fence: {}",
        wrapped.content
    );
    assert!(open_fence_at(&wrapped.content, wrapped.content.len()).is_none());

    // 4. Feed extraction caps oversized feeds at <= 50 items.
    let mut big_rss = String::from(
        "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Big Feed</title>",
    );
    for i in 1..=60 {
        big_rss.push_str(&format!(
            "<item><title>Entry {i}</title><link>https://docs.example.com/e/{i}</link><description>Item {i}</description></item>"
        ));
    }
    big_rss.push_str("</channel></rss>");
    let big_doc = extract_document(
        &big_rss,
        &page,
        Some("application/rss+xml"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(big_doc.content.contains("Entry 1"));
    assert!(big_doc.content.contains("Entry 50"));
    assert!(
        !big_doc.content.contains("Entry 55"),
        "feed should cap items at <= 50"
    );
}

#[test]
fn inline_svg_text_rescue_handles_labeled_unlabeled_and_chrome_filtering() {
    let long_segment = "x".repeat(85);
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><title>SVG Diagrams</title></head>
<body>
  <header>
    <h1>SVG Diagrams</h1>
    <svg aria-label="Header Logo" width="120" height="120"><text>Brand</text></svg>
  </header>
  <main>
    <svg aria-label="Write Path" width="400" height="200">
      <style>.label {{ fill: black; }}</style>
      <defs><text>Hidden Def</text></defs>
      <desc>Ignored Desc</desc>
      <text>Client</text>
      <text>→</text>
      <text>Sequencer</text>
      <text>Client</text>
      <foreignObject><div><span>WAL Storage</span></div></foreignObject>
      <text>{long_segment}</text>
    </svg>

    <svg width="500" height="240">
      <text>Ingest</text>
      <text>Index</text>
      <text>Query Engine</text>
    </svg>

    <svg width="200" height="100">
      <text>Too</text>
      <text>Few</text>
    </svg>

    <svg aria-label="Tiny Icon" width="16px" height="16px"><text>Should Skip</text></svg>
    <svg class="octicon octicon-mark-github" width="128" height="128" aria-label="GitHub Mark"><text>Octicon</text></svg>
    <svg class="lucide lucide-arrow" width="128" height="128" aria-label="Lucide"><text>Arrow</text></svg>
    <svg class="fa-solid fa-star" width="128" height="128" aria-label="Star"><text>Star</text></svg>
    <div role="button"><svg aria-label="Role Button Svg" width="128" height="128"><text>Click</text></svg></div>
    <a href="/home"><svg aria-label="Link Svg" width="128" height="128"><text>Home</text></svg>Home</a>
  </main>
</body>
</html>"#
    );

    let url = Url::parse("https://docs.example.com/svg").unwrap();
    let md = extract_document(&html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    let txt = extract_document(
        &html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();

    for out in [&md.content, &txt.content] {
        assert!(
            out.contains("[Diagram: Write Path — Client, Sequencer, WAL Storage]"),
            "missing labeled SVG with rescued segments in:\n{out}"
        );
        assert!(
            out.contains("[Diagram: Ingest, Index, Query Engine]"),
            "missing unlabeled >=3-node SVG rescue in:\n{out}"
        );
        assert!(!out.contains("Hidden Def"));
        assert!(!out.contains("Ignored Desc"));
        assert!(!out.contains(&long_segment));
        assert!(!out.contains("Too, Few"));
        assert!(!out.contains("Tiny Icon"));
        assert!(!out.contains("GitHub Mark"));
        assert!(!out.contains("Lucide"));
        assert!(!out.contains("Star"));
        assert!(!out.contains("Role Button Svg"));
        assert!(!out.contains("Link Svg"));
        assert!(!out.contains("Header Logo"));
    }
}

#[test]
fn resource_descriptor_salience_indexing_and_manifest_formatting() {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>System Design</title></head>
<body>
  <main>
    <h1>System Design</h1>
    <h2>Architecture</h2>
    <figure>
      <picture>
        <source srcset="/img/cluster-topology.webp?utm_source=x 1200w, /img/cluster-small.webp 400w">
        <img src="/img/cluster-fallback.png" width="1200" height="600" alt="Fallback">
      </picture>
      <figcaption>Cluster Raft Topology | v2</figcaption>
    </figure>

    <img src="/img/banner-photo.jpg" alt="Detailed server rack installation photo in datacenter">
    <img src="/img/decorative.png" alt="short">
    <img src="/img/config.png" alt="cfg">
    <img src="/img/tiny.png" width="48" height="48" alt="Tiny icon diagram">
    <img src="/img/pres.png" role="presentation" width="600" height="400" alt="Presentation diagram">
    <img src="https://img.shields.io/badge/build-passing-green.svg" width="320" height="180" alt="Build diagram">
    <img src="/img/user-avatar.png" width="320" height="200" alt="User avatar diagram">
    <img src="/favicon.ico" width="320" height="200" alt="Site icon diagram">
    <img src="data:image/png;base64,AAAA" width="400" height="300" alt="Inline diagram">

    <h2>Walkthrough Video</h2>
    <video controls src="/media/demo.mp4">
      <track kind="captions" srclang="en" label="English CC" src="/media/demo.vtt?fbclid=123">
      <track kind="subtitles" srclang="es" src="/media/demo.srt">
      <track kind="chapters" src="/media/chapters.txt">
    </video>

    <h2>Specs &amp; Data</h2>
    <p>Download the <a href="/specs/protocol.pdf?utm_medium=doc">Protocol RFC</a>,
       <a href="/specs/protocol.pdf">Duplicate RFC</a>,
       <a href="/data/benchmarks.csv">Benchmark Results</a>,
       <a href="/api/openapi.yaml">OpenAPI Spec</a>,
       <a href="/llms.txt">LLMs Index</a>, and
       <a href="/about">About Page</a>.</p>
  </main>
</body>
</html>"#;

    let url = Url::parse("https://docs.example.com/design").unwrap();
    let md_doc =
        extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    let txt_doc = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    let html_doc = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            format: OutputFormat::Html,
            ..Default::default()
        },
    )
    .unwrap();

    // All three output formats populate identical resources.
    assert_eq!(md_doc.resources, txt_doc.resources);
    assert_eq!(md_doc.resources, html_doc.resources);

    // `<track>` must not leak junk into Markdown output.
    assert!(!md_doc.content.contains("demo.vtt"));
    assert!(!md_doc.content.contains("<track"));

    let res = &md_doc.resources;
    assert_eq!(res.len(), 8, "unexpected resources: {res:#?}");

    // r1: figure picture diagram
    assert_eq!(res[0].identifier, "r1");
    assert_eq!(res[0].kind, ResourceKind::Diagram);
    assert_eq!(res[0].salience, ResourceSalience::High);
    assert_eq!(
        res[0].url,
        "https://docs.example.com/img/cluster-topology.webp"
    );
    assert_eq!(res[0].section_heading.as_deref(), Some("Architecture"));
    assert_eq!(res[0].label.as_deref(), Some("Cluster Raft Topology | v2"));
    assert_eq!(res[0].dimensions, Some((1200, 600)));

    // r2: descriptive prose image (Medium)
    assert_eq!(res[1].identifier, "r2");
    assert_eq!(res[1].kind, ResourceKind::Image);
    assert_eq!(res[1].salience, ResourceSalience::Medium);
    assert_eq!(res[1].url, "https://docs.example.com/img/banner-photo.jpg");

    // r3 & r4: video tracks
    assert_eq!(res[2].identifier, "r3");
    assert_eq!(res[2].kind, ResourceKind::Transcript);
    assert_eq!(res[2].salience, ResourceSalience::High);
    assert_eq!(res[2].url, "https://docs.example.com/media/demo.vtt");
    assert_eq!(res[2].label.as_deref(), Some("English CC"));
    assert_eq!(res[2].mime_hint.as_deref(), Some("text/vtt"));
    assert_eq!(res[2].section_heading.as_deref(), Some("Walkthrough Video"));

    assert_eq!(res[3].identifier, "r4");
    assert_eq!(res[3].kind, ResourceKind::Transcript);
    assert_eq!(res[3].url, "https://docs.example.com/media/demo.srt");
    assert_eq!(res[3].label.as_deref(), Some("es"));
    assert_eq!(res[3].mime_hint.as_deref(), Some("application/x-subrip"));

    // r5..r8: PDF, CSV, OpenAPI YAML, llms.txt (deduplicated protocol.pdf)
    assert_eq!(res[4].identifier, "r5");
    assert_eq!(res[4].kind, ResourceKind::Document);
    assert_eq!(res[4].url, "https://docs.example.com/specs/protocol.pdf");
    assert_eq!(res[4].mime_hint.as_deref(), Some("application/pdf"));
    assert_eq!(res[4].section_heading.as_deref(), Some("Specs & Data"));

    assert_eq!(res[5].identifier, "r6");
    assert_eq!(res[5].kind, ResourceKind::Dataset);
    assert_eq!(res[5].url, "https://docs.example.com/data/benchmarks.csv");

    assert_eq!(res[6].identifier, "r7");
    assert_eq!(res[6].kind, ResourceKind::Schema);
    assert_eq!(res[6].url, "https://docs.example.com/api/openapi.yaml");

    assert_eq!(res[7].identifier, "r8");
    assert_eq!(res[7].kind, ResourceKind::Schema);
    assert_eq!(res[7].url, "https://docs.example.com/llms.txt");

    // Manifest formatting
    assert_eq!(format_resource_manifest_markdown(&[]), "");
    let manifest = format_resource_manifest_markdown(res);
    assert!(manifest.starts_with(
        "## Linked Resources\n\n| ID | Kind | Salience | Section | Dimensions | Label | URL |\n"
    ));
    assert!(manifest.contains("| r1 | diagram | high | Architecture | 1200x600 | Cluster Raft Topology \\| v2 | https://docs.example.com/img/cluster-topology.webp |"));
    assert!(manifest.contains("| r5 | document | high | Specs & Data | - | Protocol RFC | https://docs.example.com/specs/protocol.pdf |"));
}

#[test]
fn transcript_extraction_handles_webvtt_srt_speakers_rolling_captions_and_passthrough() {
    let url = Url::parse("https://media.example.com/talk.vtt").unwrap();
    let vtt = "\u{FEFF}WEBVTT - Keynote Talk\nKind: captions\nLanguage: en\n\n\
STYLE\n::cue { color: white; }\n\n\
NOTE This is a comment block\nspanning two lines.\n\n\
REGION\nid:top\n\n\
intro-1\n\
00:00:01.000 --> 00:00:03.000 align:start\n\
<v Alice>Welcome &amp; <i>hello</i> <00:00:02.000><c.highlight>everyone</c>!\n\n\
00:00:03.000 --> 00:00:05.000\n\
<v Alice>Welcome &amp; hello everyone!\n\n\
00:00:05.000 --> 00:00:08.000\n\
<v Alice>It&#39;s &quot;great&quot;&nbsp;to be here.\n\n\
00:00:08.000 --> 00:00:11.000\n\
<v.loud Bob>Thanks &lt;Alice&gt;!</v>\n";

    // Markdown Clean mode (sniffed via BOM + WEBVTT even with None MIME)
    let md = extract_document(vtt, &url, None, &ExtractOptions::default()).unwrap();
    assert_eq!(
        md.content,
        "**[Alice]:** Welcome & hello everyone!\nIt's \"great\" to be here.\n**[Bob]:** Thanks <Alice>!"
    );

    // Text Clean mode
    let txt = extract_document(
        vtt,
        &url,
        Some("text/vtt; charset=utf-8"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        txt.content,
        "[Alice]: Welcome & hello everyone!\nIt's \"great\" to be here.\n[Bob]: Thanks <Alice>!"
    );

    // Full mode and Html format preserve verbatim passthrough
    let full = extract_document(
        vtt,
        &url,
        Some("text/vtt"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(full.content, vtt);

    let as_html = extract_document(
        vtt,
        &url,
        Some("text/vtt"),
        &ExtractOptions {
            format: OutputFormat::Html,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(as_html.content, vtt);

    // SubRip (.srt) extraction
    let srt = "1\r\n00:00:01,000 --> 00:00:02,500\r\n<b>First</b> line\r\n\r\n\
2\r\n00:00:02,500 --> 00:00:04,000\r\nFirst line\r\n\r\n\
3\r\n00:00:04,000 --> 00:00:06,000\r\nSecond &amp; final line\r\n";
    let srt_doc = extract_document(
        srt,
        &url,
        Some("application/x-subrip"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(srt_doc.content, "First line\nSecond & final line");
}

#[test]
fn adversarial_m4_svg_cap_non_http_resources_and_vtt_angle_brackets() {
    let url = Url::parse("https://docs.example.com/spec").unwrap();

    // 1. Inline SVG with 40 distinct text nodes caps at 32 segments, ignores <script>, and drops width=63.
    let mut nodes_40 = String::new();
    for i in 1..=40 {
        nodes_40.push_str(&format!("<text>Node{i}</text>"));
    }
    let svg_html = format!(
        r#"<!DOCTYPE html><html><head><title>Cap</title></head><body><main>
        <svg aria-label="Large Graph" width="800" height="400"><script>alert(1)</script>{nodes_40}</svg>
        <svg aria-label="Too Narrow" width="63" height="200"><text>NarrowA</text><text>NarrowB</text><text>NarrowC</text></svg>
        <img src="javascript:alert(1)" width="600" height="400" alt="Evil diagram">
        <img src="/img/borderline.png" width="99" height="80" alt="Borderline tiny diagram">
        <a href="javascript:void(0)/evil.pdf">Bad PDF</a>
        <a href="/schemas/user.schema.json">User Schema</a>
        <a href="/data/events.jsonl">Events JSONL</a>
        <a href="/rpc/raft.proto">Raft Proto</a>
        </main></body></html>"#
    );
    let doc = extract_document(
        &svg_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(doc.content.contains("Node1"));
    assert!(doc.content.contains("Node32"));
    assert!(
        !doc.content.contains("Node33"),
        "SVG rescue must cap segments at 32"
    );
    assert!(!doc.content.contains("alert(1)"));
    assert!(!doc.content.contains("Too Narrow"));
    assert!(!doc.content.contains("NarrowA"));

    let urls: Vec<&str> = doc.resources.iter().map(|r| r.url.as_str()).collect();
    assert!(!urls.iter().any(|u| u.starts_with("javascript:")));
    assert!(!urls.iter().any(|u| u.contains("borderline.png")));
    assert_eq!(
        urls,
        vec![
            "https://docs.example.com/schemas/user.schema.json",
            "https://docs.example.com/data/events.jsonl",
            "https://docs.example.com/rpc/raft.proto",
        ]
    );
    assert_eq!(doc.resources[0].kind, ResourceKind::Schema);
    assert_eq!(doc.resources[1].kind, ResourceKind::Dataset);
    assert_eq!(doc.resources[2].kind, ResourceKind::Schema);

    // 2. WebVTT literal `a < b` comparison survives tag stripping; `WEBVTTX` is not sniffed as VTT.
    let vtt_math = "WEBVTT\n\n00:00:01.000 --> 00:00:03.000\nCheck x < y and y > z carefully.\n";
    let vtt_doc =
        extract_document(vtt_math, &url, Some("text/vtt"), &ExtractOptions::default()).unwrap();
    assert_eq!(vtt_doc.content, "Check x < y and y > z carefully.");

    let not_vtt = "WEBVTTX header\n00:00:01.000 --> 00:00:02.000\nKeep raw text.";
    let plain_doc = extract_document(
        not_vtt,
        &url,
        Some("text/plain"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(plain_doc.content, not_vtt);
}

#[test]
fn adversarial_canary_inverse_false_positives_preserve_valid_content() {
    let url = Url::parse("https://docs.example.org/guide").unwrap();

    // 1. Ordinary application/json API payloads with version+items are NOT treated as JSON Feeds.
    let api_json = r#"{"version":"1.0","items":[{"id":1,"title":"Not a feed"}]}"#;
    let json_doc = extract_document(
        api_json,
        &url,
        Some("application/json"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(json_doc.content, api_json);
    assert!(json_doc.title.is_none());

    // 2. Visible inline styles, normal images, KaTeX inside display:none, regular checkboxes,
    // checked checkbox popups, inline prose skip links, and deduplicated/unsafe citation_pdf_url.
    let html = r##"<!DOCTYPE html><html><head>
      <title>Inverse Guard</title>
      <meta name="citation_title" content="Paper Title">
      <meta name="citation_pdf_url" content="https://docs.example.org/files/spec.pdf">
      <meta name="citation_pdf_url" content="javascript:alert(1)">
    </head><body><main>
      <h1>Inverse Guard</h1>
      <p style="display: block; visibility: visible;">Visible styled prose stays intact.</p>
      <p>Read the <a href="#appendix">Skip to appendix</a> note inside this sentence.</p>
      <img src="/img/chart.png" width="240" height="120" alt="Latency chart">
      <span style="display: none"><math><annotation encoding="application/x-tex">E = mc^2</annotation></math></span>
      <div class="checklist"><input type="checkbox"> Plain task item stays</div>
      <div class="open-menu"><input type="checkbox" aria-haspopup="true" checked> Checked panel stays</div>
      <form><input type="checkbox" aria-haspopup="true"> Form checkbox stays</form>
      <a href="/files/spec.pdf">Spec PDF</a>
      <pre>   </pre>
    </main></body></html>"##;
    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert!(doc.content.contains("Visible styled prose stays intact."));
    assert!(doc.content.contains("Skip to appendix"));
    assert!(doc
        .content
        .contains("https://docs.example.org/img/chart.png"));
    assert!(doc.content.contains("$E = mc^2$"));
    assert!(doc.content.contains("Plain task item stays"));
    assert!(doc.content.contains("Checked panel stays"));
    assert!(doc.content.contains("Form checkbox stays"));
    assert!(
        !doc.content.contains("```"),
        "whitespace-only <pre> must not emit an empty fence"
    );

    let pdf_resources: Vec<&str> = doc
        .resources
        .iter()
        .filter(|r| r.kind == ResourceKind::Document)
        .map(|r| r.url.as_str())
        .collect();
    assert_eq!(
        pdf_resources,
        vec!["https://docs.example.org/files/spec.pdf"],
        "citation_pdf_url must deduplicate against body link and reject javascript:"
    );

    // 3. Native Markdown HTML <h1> title, Setext H2 non-promotion, prose [Note]: safety, and --no-links/--no-images ref drop.
    let md_url = Url::parse("https://raw.example.org/org/repo/main/README.md").unwrap();
    let html_h1_md = "<div align=\"center\"><h1 class=\"title\">Mesh <code>Core</code></h1></div>\n\nSection Heading\n---\n\n[Note]: this is plain prose, not a link ref.\n[Guide]: ./GUIDE.md\n";
    let md_doc = extract_document(
        html_h1_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(md_doc.title.as_deref(), Some("Mesh Core"));
    assert!(
        !md_doc.content.starts_with("# Mesh Core"),
        "body HTML <h1> must not synthesize a duplicate ATX heading"
    );
    assert!(md_doc
        .content
        .contains("[Note]: this is plain prose, not a link ref."));
    assert!(md_doc
        .content
        .contains("[Guide]: https://raw.example.org/org/repo/main/GUIDE.md"));

    let h2_only_md = "Subheading Only\n---\n\nBody text.\n";
    let h2_doc = extract_document(
        h2_only_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(
        h2_doc.title, None,
        "Setext H2 (---) must not become H1 title"
    );

    let no_refs = extract_document(
        html_h1_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions {
            include_images: false,
            include_links: false,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!no_refs.content.contains("GUIDE.md"));

    // 4. Native Markdown --format text preserves literal brackets/identifiers and fenced code,
    // while unwrapping defined shortcut refs and stripping link-ref definitions.
    let txt_md = "Array arr[0] and [INFO] stay intact, plus `foo__bar` and [Defined].\n\n```md\n**fenced_bold** and [Keep]: ./keep.md\n```\n\n[Defined]: ./defined.md\n";
    let txt_doc = extract_document(
        txt_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(txt_doc
        .content
        .contains("Array arr[0] and [INFO] stay intact, plus foo__bar and Defined."));
    assert!(txt_doc
        .content
        .contains("**fenced_bold** and [Keep]: ./keep.md"));
    assert!(!txt_doc.content.contains("defined.md"));

    // 5. Native Markdown --format text HTML/MDX stripping preserves fenced HTML, autolinks,
    // generics, single-letter <T>, and unclosed tags, while stripping <script> bodies.
    let mdx_edge = concat!(
        "See <https://example.org>, Option<Card>, and generic <T> with <div unclosed.\n",
        "<script>alert(1)</script><style>.x{color:red}</style>Safe prose.\n\n",
        "```html\n<div class=\"keep\"><Note>verbatim</Note></div>\n```\n"
    );
    let mdx_doc = extract_document(
        mdx_edge,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(mdx_doc
        .content
        .contains("See <https://example.org>, Option<Card>, and generic <T> with <div unclosed."));
    assert!(mdx_doc.content.contains("Safe prose."));
    assert!(!mdx_doc.content.contains("alert(1)"));
    assert!(!mdx_doc.content.contains("color:red"));
    assert!(mdx_doc
        .content
        .contains("<div class=\"keep\"><Note>verbatim</Note></div>"));

    // 6. Inverse tests for button controls inside landmarks, Markdown mirror deduplication,
    // fenced/Full skip-link preservation, and `#`/nested inline formatting in `--format text`.
    let landmark_buttons_html = concat!(
        "<!doctype html><html><head><title>Controls • Docs</title></head><body>",
        "<button type=\"button\">Skip to content</button>",
        "<article><h1>Controls</h1><ul>",
        "<li><button type=\"button\">Alpha</button></li>",
        "<li><button type=\"button\">Beta</button></li>",
        "<li><button type=\"button\">Gamma</button></li>",
        "<li><button type=\"button\">Skip to step 2</button></li>",
        "</ul></article></body></html>"
    );
    let landmark_doc = extract_document(
        landmark_buttons_html,
        &Url::parse("https://docs.example.dev/controls").unwrap(),
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(!landmark_doc.content.contains("Skip to content"));
    assert!(!landmark_doc.content.contains("Controls • Docs"));
    assert!(landmark_doc.content.contains("Alpha"));
    assert!(landmark_doc.content.contains("Skip to step 2"));

    let mirror_md = concat!(
        "---\ntitle: CLI\n---\n\n",
        "[Skip to content](#main-content)\n\n",
        "> Index note\n\n",
        "# CLI\n\n",
        "Issue [#123](https://example.com/123) and [Skip to section](#s) inline.\n\n",
        "```md\n[Skip to content](#fenced)\n```\n"
    );
    let mirror_clean = extract_document(
        mirror_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(mirror_clean.content.matches("# CLI").count(), 1);
    assert!(!mirror_clean.content.contains("#main-content"));
    assert!(mirror_clean.content.contains("[Skip to content](#fenced)"));
    assert!(mirror_clean.content.contains("[Skip to section]"));

    let mirror_text = extract_document(
        mirror_md,
        &md_url,
        Some("text/markdown"),
        &ExtractOptions {
            format: OutputFormat::Text,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(mirror_text
        .content
        .contains("Issue #123 and Skip to section inline."));
}
