use pleb::extract::markdown::{compact_markdown, open_fence_at};
use pleb::extract::url::{clean_url, resolve_and_clean_url, ResolvedUrl};
use pleb::extract::{
    decode_text_bytes, detect_binary_content, extract_document, extract_document_bytes,
    format_resource_manifest_markdown, resource_identifier, truncate_markdown, truncate_utf8,
    wrap_raw_code_document, ExtractMode, ExtractOptions, OutputFormat, ParsedHtml, ResourceKind,
    ResourceSalience,
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
            ..Default::default()
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
    use pleb::extract::markdown::open_fence_at;

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
    for item in res {
        assert_eq!(item.identifier, resource_identifier(&item.url));
        assert!(item.identifier.starts_with("r_") && item.identifier.len() == 10);
    }

    // figure picture diagram
    assert_eq!(res[0].kind, ResourceKind::Diagram);
    assert_eq!(res[0].salience, ResourceSalience::High);
    assert_eq!(
        res[0].url,
        "https://docs.example.com/img/cluster-topology.webp"
    );
    assert_eq!(res[0].section_heading.as_deref(), Some("Architecture"));
    assert_eq!(res[0].label.as_deref(), Some("Cluster Raft Topology | v2"));
    assert_eq!(res[0].dimensions, Some((1200, 600)));

    // descriptive prose image (Medium)
    assert_eq!(res[1].kind, ResourceKind::Image);
    assert_eq!(res[1].salience, ResourceSalience::Medium);
    assert_eq!(res[1].url, "https://docs.example.com/img/banner-photo.jpg");

    // video tracks
    assert_eq!(res[2].kind, ResourceKind::Transcript);
    assert_eq!(res[2].salience, ResourceSalience::High);
    assert_eq!(res[2].url, "https://docs.example.com/media/demo.vtt");
    assert_eq!(res[2].label.as_deref(), Some("English CC"));
    assert_eq!(res[2].mime_hint.as_deref(), Some("text/vtt"));
    assert_eq!(res[2].section_heading.as_deref(), Some("Walkthrough Video"));

    assert_eq!(res[3].kind, ResourceKind::Transcript);
    assert_eq!(res[3].url, "https://docs.example.com/media/demo.srt");
    assert_eq!(res[3].label.as_deref(), Some("es"));
    assert_eq!(res[3].mime_hint.as_deref(), Some("application/x-subrip"));

    // PDF, CSV, OpenAPI YAML, llms.txt (deduplicated protocol.pdf)
    assert_eq!(res[4].kind, ResourceKind::Document);
    assert_eq!(res[4].url, "https://docs.example.com/specs/protocol.pdf");
    assert_eq!(res[4].mime_hint.as_deref(), Some("application/pdf"));
    assert_eq!(res[4].section_heading.as_deref(), Some("Specs & Data"));

    assert_eq!(res[5].kind, ResourceKind::Dataset);
    assert_eq!(res[5].url, "https://docs.example.com/data/benchmarks.csv");

    assert_eq!(res[6].kind, ResourceKind::Schema);
    assert_eq!(res[6].url, "https://docs.example.com/api/openapi.yaml");

    assert_eq!(res[7].kind, ResourceKind::Schema);
    assert_eq!(res[7].url, "https://docs.example.com/llms.txt");

    // Manifest formatting
    assert_eq!(format_resource_manifest_markdown(&[]), "");
    let manifest = format_resource_manifest_markdown(res);
    assert!(manifest.starts_with(
        "## Linked Resources\n\n| ID | Kind | Salience | Section | Dimensions | Label | URL |\n"
    ));
    assert!(manifest.contains(&format!(
        "| {} | diagram | high | Architecture | 1200x600 | Cluster Raft Topology \\| v2 | https://docs.example.com/img/cluster-topology.webp |",
        res[0].identifier
    )));
    assert!(manifest.contains(&format!(
        "| {} | document | high | Specs & Data | - | Protocol RFC | https://docs.example.com/specs/protocol.pdf |",
        res[4].identifier
    )));
}

#[test]
fn resource_identifiers_stay_stable_across_reordering_and_filter_avatar_noise() {
    let url = Url::parse("https://feed.example.com/r/rust").unwrap();
    let html_order_a = r#"<!doctype html><html><body><main>
        <h1>r/rust</h1>
        <img src="https://styles.redditmedia.com/t5_1/styles/profileIcon_abc.webp" alt="Profile badge">
        <img src="https://styles.redditmedia.com/t5_2/styles/communityIcon_xyz.png" alt="Community icon long label">
        <img src="https://styles.redditmedia.com/t5_3/styles/userIcon_def.png" alt="Moderator user icon">
        <img src="https://cdn.example.com/u/custom.jpg" alt="u/alice_developer avatar">
        <figure><img src="/img/arch-diagram.png" alt="System diagram"><figcaption>System diagram</figcaption></figure>
        <a href="/specs/protocol.pdf">Protocol PDF</a>
    </main></body></html>"#;
    let html_order_b = r#"<!doctype html><html><body><main>
        <h1>r/rust</h1>
        <a href="/specs/new-other.pdf">Inserted Newer Post PDF</a>
        <a href="/specs/protocol.pdf">Protocol PDF</a>
        <figure><img src="/img/arch-diagram.png" alt="System diagram"><figcaption>System diagram</figcaption></figure>
    </main></body></html>"#;

    let doc_a = extract_document(
        html_order_a,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    let doc_b = extract_document(
        html_order_b,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();

    assert_eq!(
        doc_a.resources.len(),
        2,
        "avatars/icons must be excluded: {:#?}",
        doc_a.resources
    );
    let diag_a = doc_a
        .resources
        .iter()
        .find(|r| r.url.ends_with("/arch-diagram.png"))
        .unwrap();
    let pdf_a = doc_a
        .resources
        .iter()
        .find(|r| r.url.ends_with("/protocol.pdf"))
        .unwrap();
    let diag_b = doc_b
        .resources
        .iter()
        .find(|r| r.url.ends_with("/arch-diagram.png"))
        .unwrap();
    let pdf_b = doc_b
        .resources
        .iter()
        .find(|r| r.url.ends_with("/protocol.pdf"))
        .unwrap();

    assert_eq!(diag_a.identifier, diag_b.identifier);
    assert_eq!(pdf_a.identifier, pdf_b.identifier);
    assert_eq!(diag_a.identifier, resource_identifier(&diag_a.url));
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

#[test]
fn webmcp_declarative_forms_extract_schemas_prune_chrome_and_preserve_normal_forms() {
    use pleb::extract::{format_webmcp_cue, format_webmcp_manifest_markdown, WebMcpToolSource};
    use serde_json::json;

    let html = r#"<!doctype html>
<html>
<head>
  <title>Catalog &amp; Tools</title>
  <base href="https://hijack.example.org/wrong/">
</head>
<body>
  <nav>
    <form toolname="nav_search" tooldescription="Search documentation" tooltitle="Docs Search" action="/search#ignored" method="post" toolautosubmit>
      <label for="nav.q[0]:id">Query string</label>
      <input id="nav.q[0]:id" name="q" required>
      <button type="submit">Go</button>
    </form>
  </nav>
  <main>
    <h1>Catalog</h1>
    <p>Browse our public hardware catalog and run read-only queries.</p>

    <form toolname="filter_items" tooldescription="Filter   catalog   items">
      <input type="text" name="keyword" toolparamdescription="Primary keyword">
      <input type="email" name="contact" toolparamdescription="Reply email" required>
      <input type="checkbox" name="in_stock" toolparamdescription="Only in stock">
      <input type="number" name="min_price" min="10" max="999.5" toolparamdescription="Price floor">
      <input type="range" name="rating" min="1" max="5">
      <textarea name="notes" toolparamdescription="Extra filter notes"></textarea>

      <input type="radio" name="plan">
      <input type="radio" name="plan" value="" toolparamdescription="Billing tier" required>
      <input type="radio" name="plan" value="pro" toolparamdescription="Should not overwrite">
      <input type="radio" name="plan" value="on">
      <input type="radio" name="plan" value="disabled_opt" disabled>

      <select name="region" required>
        <option value="us">United States</option>
        <option>EU</option>
        <option value="us">Duplicate US</option>
        <option value="dis" disabled>Disabled Opt</option>
        <optgroup label="Blocked" disabled>
          <option value="blocked">Blocked Opt</option>
        </optgroup>
      </select>

      <select name="tags" multiple toolparamdescription="Item tags">
        <option value="">Any tag</option>
        <option>rust</option>
        <option>wasm</option>
      </select>

      <select name="empty_sel">
        <option value="nope" disabled>Only disabled</option>
      </select>

      <input type="hidden" name="csrf" value="secret">
      <input type="password" name="pw">
      <input type="file" name="upload">
      <input type="button" name="btn" value="Click">
      <input type="submit" name="sub" value="Submit">
      <input type="reset" name="rst" value="Reset">
      <input type="image" name="img_sub" src="/go.png">
      <input type="text" name="disabled_direct" disabled>
      <fieldset disabled>
        <input type="text" name="inside_disabled_fieldset">
      </fieldset>
      <input type="text">
      <span>WebMCP Chrome Noise Should Disappear</span>
    </form>

    <form toolname="empty_tool" tooldescription="Empty form tool" method="dialog">
      <button type="submit">Close</button>
    </form>

    <form action="/login" method="post">
      <label for="ordinary_user">Ordinary form prose stays</label>
      <input id="ordinary_user" name="user" type="text">
    </form>
  </main>
</body>
</html>"#;

    let page_url = Url::parse("https://user:pass@docs.example.com/catalog?v=1#frag").unwrap();
    let doc = extract_document(
        html,
        &page_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();

    // Clean Markdown detaches WebMCP form chrome while preserving surrounding prose & normal forms.
    assert!(doc
        .content
        .contains("Browse our public hardware catalog and run read-only queries."));
    assert!(doc.content.contains("Ordinary form prose stays"));
    assert!(!doc.content.contains("WebMCP Chrome Noise Should Disappear"));
    assert!(!doc.content.contains("Query string"));

    // Full mode retains all DOM content.
    let full_doc = extract_document(
        html,
        &page_url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(full_doc
        .content
        .contains("WebMCP Chrome Noise Should Disappear"));

    assert_eq!(doc.tools.len(), 3);

    // 1. nav_search (inside <nav>, resolved against <base href> because non-empty action, POST, autosubmit, label[for] with CSS metacharacters)
    let nav = &doc.tools[0];
    assert_eq!(nav.name, "nav_search");
    assert_eq!(nav.description, "Search documentation");
    assert_eq!(nav.title.as_deref(), Some("Docs Search"));
    assert_eq!(nav.source, WebMcpToolSource::Declarative);
    assert_eq!(
        nav.action_url.as_deref(),
        Some("https://hijack.example.org/search")
    );
    assert_eq!(nav.method.as_deref(), Some("POST"));
    assert_eq!(
        nav.annotations.as_ref().and_then(|a| a.autosubmit),
        Some(true)
    );
    assert_eq!(
        nav.annotations.as_ref().and_then(|a| a.untrusted_content),
        Some(true)
    );
    assert_eq!(
        nav.input_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "q": { "type": "string", "description": "Query string" }
            },
            "required": ["q"]
        }))
    );

    // 2. filter_items (absent action defaults to page_url ignoring <base href>, strips credentials/fragment)
    let filter = &doc.tools[1];
    assert_eq!(filter.name, "filter_items");
    assert_eq!(filter.description, "Filter catalog items");
    assert_eq!(
        filter.action_url.as_deref(),
        Some("https://docs.example.com/catalog?v=1")
    );
    assert_eq!(filter.method.as_deref(), Some("GET"));
    assert_eq!(
        filter.input_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "keyword": { "type": "string", "description": "Primary keyword" },
                "contact": { "type": "string", "format": "email", "description": "Reply email" },
                "in_stock": { "type": "boolean", "description": "Only in stock" },
                "min_price": { "type": "number", "minimum": 10, "maximum": 999.5, "description": "Price floor" },
                "rating": { "type": "number", "minimum": 1, "maximum": 5 },
                "notes": { "type": "string", "description": "Extra filter notes" },
                "plan": { "type": "string", "enum": ["on", "", "pro"], "description": "Billing tier" },
                "region": { "type": "string", "enum": ["us", "EU"] },
                "tags": {
                    "type": "array",
                    "items": { "type": "string", "enum": ["", "rust", "wasm"] },
                    "description": "Item tags"
                },
                "empty_sel": { "type": "string" }
            },
            "required": ["contact", "plan", "region"]
        }))
    );

    // 3. empty_tool (empty properties without "required"; method="dialog" omitted -> None)
    let empty = &doc.tools[2];
    assert_eq!(empty.name, "empty_tool");
    assert_eq!(empty.method, None);
    assert_eq!(
        empty.input_schema,
        Some(json!({ "type": "object", "properties": {} }))
    );

    // Manifest and cue formatters
    let manifest = format_webmcp_manifest_markdown(&doc.tools);
    assert!(manifest.starts_with("## Page Tools (WebMCP)\n\n"));
    assert!(manifest.contains("| nav_search | declarative | POST | https://hijack.example.org/search | Docs Search | Search documentation |"));
    assert!(manifest.contains("```json\n{\n  "));
    assert_eq!(format_webmcp_manifest_markdown(&[]), "");

    let cue = format_webmcp_cue(&doc.tools).unwrap();
    assert_eq!(
        cue,
        "Tools: 3 WebMCP tools (nav_search, filter_items, empty_tool) — pass --tools manifest or --json"
    );
    assert_eq!(
        format_webmcp_cue(&doc.tools[..1]).as_deref(),
        Some("Tools: 1 WebMCP tool (nav_search) — pass --tools manifest or --json")
    );
    assert_eq!(format_webmcp_cue(&[]), None);
}

#[test]
fn webmcp_adversarial_filtering_caps_sanitization_and_injection_guards() {
    use pleb::extract::{
        format_webmcp_manifest_markdown, merge_webmcp_tools, sanitize_webmcp_tool,
        webmcp::MAX_WEBMCP_TOOLS, WebMcpToolAnnotations, WebMcpToolDescriptor, WebMcpToolSource,
    };
    use serde_json::json;

    let overlong_name = "a".repeat(129);
    let overlong_param_name = "p".repeat(129);
    let mut forms_40 = String::new();
    for i in 1..=40 {
        forms_40.push_str(&format!(
            r#"<form toolname="bulk_{i}" tooldescription="Bulk tool {i}"></form>"#
        ));
    }

    let html = format!(
        r#"<!doctype html><html><body>
        <form toolname=" bad_ws " tooldescription="Should reject"></form>
        <form toolname="bad/slash" tooldescription="Should reject"></form>
        <form toolname="{overlong_name}" tooldescription="Should reject"></form>
        <form toolname="empty_desc" tooldescription="   &#x1b;  "></form>

        <template><form toolname="in_template" tooldescription="Hidden"></form></template>
        <noscript><form toolname="in_noscript" tooldescription="Hidden"></form></noscript>
        <div hidden><form toolname="in_hidden_anc" tooldescription="Hidden"></form></div>
        <form toolname="self_hidden" tooldescription="Hidden" hidden></form>
        <div aria-hidden="TRUE"><form toolname="in_aria_hidden" tooldescription="Hidden"></form></div>
        <div style="display: none"><form toolname="in_display_none" tooldescription="Hidden"></form></div>
        <div style="visibility: hidden"><form toolname="in_vis_hidden" tooldescription="Hidden"></form></div>

        <form toolname="dup_tool" tooldescription="First wins" action="javascript:alert(1)">
          <div style="display:none"><input name="secret_hidden_field" type="text"></div>
          <template><input name="tpl_field" type="text"></template>
          <input name="{overlong_param_name}" type="text">
        </form>
        <form toolname="dup_tool" tooldescription="Second ignored"></form>

        <form toolname="ssrf_ip" tooldescription="Private IP" action="http://169.254.169.254/latest/meta-data"></form>
        <form toolname="downgrade" tooldescription="Downgrade" action="http://docs.example.com/submit"></form>
        <form toolname="ansi_tool" tooldescription="Search&#x1b;[31m docs&#x07;&#x7f;&#x202E; now" tooltitle="Title | pipe&#x1b;[1m">
          <label for="hidden_lbl" hidden>Hidden label</label>
          <label for="vis_lbl">Visible label</label>
          <input id="hidden_lbl" name="h">
          <input id="vis_lbl" name="v" toolparamdescription="   ">
        </form>
        {forms_40}
    </body></html>"#
    );

    let page_url = Url::parse("https://docs.example.com/start").unwrap();
    let doc = extract_document(
        &html,
        &page_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();

    assert_eq!(doc.tools.len(), MAX_WEBMCP_TOOLS);
    let names: Vec<&str> = doc.tools.iter().map(|t| t.name.as_str()).collect();
    for forbidden in [
        " bad_ws ",
        "bad_ws",
        "bad/slash",
        overlong_name.as_str(),
        "empty_desc",
        "in_template",
        "in_noscript",
        "in_hidden_anc",
        "self_hidden",
        "in_aria_hidden",
        "in_display_none",
        "in_vis_hidden",
        "bulk_30",
    ] {
        assert!(!names.contains(&forbidden), "unexpected tool {forbidden}");
    }

    let dup = doc.tools.iter().find(|t| t.name == "dup_tool").unwrap();
    assert_eq!(dup.description, "First wins");
    assert_eq!(dup.action_url, None);
    assert_eq!(
        dup.input_schema,
        Some(json!({ "type": "object", "properties": {} }))
    );

    let ssrf = doc.tools.iter().find(|t| t.name == "ssrf_ip").unwrap();
    assert_eq!(ssrf.action_url, None);
    let downgrade = doc.tools.iter().find(|t| t.name == "downgrade").unwrap();
    assert_eq!(downgrade.action_url, None);

    let ansi = doc.tools.iter().find(|t| t.name == "ansi_tool").unwrap();
    assert_eq!(ansi.description, "Search[31m docs now");
    assert_eq!(ansi.title.as_deref(), Some("Title | pipe[1m"));
    assert_eq!(
        ansi.input_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "h": { "type": "string" },
                "v": { "type": "string", "description": "Visible label" }
            }
        }))
    );

    // `sanitize_webmcp_tool` overrides untrusted_content: Some(false) and drops oversized/deep schemas.
    let mut too_many_props = serde_json::Map::new();
    for i in 0..65 {
        too_many_props.insert(format!("p{i}"), json!({ "type": "string" }));
    }
    let bad_props_tool = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "imp_tool".to_string(),
        description: "Imperative\n\x1b[31m\u{202E} tool".to_string(),
        title: Some("Title\x07\u{2066}".to_string()),
        source: WebMcpToolSource::Imperative,
        input_schema: Some(json!({ "type": "object", "properties": too_many_props })),
        annotations: Some(WebMcpToolAnnotations {
            read_only: Some(true),
            autosubmit: None,
            untrusted_content: Some(false),
        }),
        action_url: Some("https://user:pw@example.com/run#frag".to_string()),
        method: Some("put".to_string()),
    })
    .unwrap();
    assert_eq!(bad_props_tool.description, "Imperative [31m tool");
    assert_eq!(bad_props_tool.title.as_deref(), Some("Title"));
    assert_eq!(bad_props_tool.input_schema, None);
    assert_eq!(
        bad_props_tool
            .annotations
            .as_ref()
            .and_then(|a| a.untrusted_content),
        Some(true)
    );
    assert_eq!(
        bad_props_tool.action_url.as_deref(),
        Some("https://example.com/run")
    );
    assert_eq!(bad_props_tool.method, None);

    // Deep schema (> 8) & oversized enum (> 64 or > 256 bytes) & oversized bytes (> 16 KiB) drop to None.
    let mut deep = json!({ "type": "string" });
    for _ in 0..9 {
        deep = json!({ "nested": deep });
    }
    let deep_sanitized = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "deep".to_string(),
        description: "Deep".to_string(),
        title: None,
        source: WebMcpToolSource::Imperative,
        input_schema: Some(deep),
        annotations: None,
        action_url: None,
        method: None,
    })
    .unwrap();
    assert_eq!(deep_sanitized.input_schema, None);

    let enum_65: Vec<String> = (0..65).map(|i| format!("v{i}")).collect();
    let enum_sanitized = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "enum_cap".to_string(),
        description: "Enum cap".to_string(),
        title: None,
        source: WebMcpToolSource::Imperative,
        input_schema: Some(json!({ "type": "object", "properties": { "k": { "type": "string", "enum": enum_65 } } })),
        annotations: None,
        action_url: None,
        method: None,
    })
    .unwrap();
    assert_eq!(enum_sanitized.input_schema, None);

    // Oversized enum item (> 256 bytes) and oversized schema (> 16 KiB) drop to None.
    let long_enum_sanitized = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "long_enum".to_string(),
        description: "Long enum".to_string(),
        title: None,
        source: WebMcpToolSource::Imperative,
        input_schema: Some(
            json!({ "type": "object", "properties": { "k": { "type": "string", "enum": ["x".repeat(257)] } } }),
        ),
        annotations: None,
        action_url: None,
        method: None,
    })
    .unwrap();
    assert_eq!(long_enum_sanitized.input_schema, None);

    let big_schema_sanitized = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "big_schema".to_string(),
        description: "Big schema".to_string(),
        title: None,
        source: WebMcpToolSource::Imperative,
        input_schema: Some(
            json!({ "type": "object", "properties": { "k": { "type": "string", "description": "d".repeat(17 * 1024) } } }),
        ),
        annotations: None,
        action_url: None,
        method: None,
    })
    .unwrap();
    assert_eq!(big_schema_sanitized.input_schema, None);

    // Imperative overrides declarative by name even when declarative already fills the 32-tool cap.
    let mut decl_32 = Vec::new();
    for i in 0..MAX_WEBMCP_TOOLS {
        decl_32.push(WebMcpToolDescriptor {
            name: format!("tool_{i}"),
            description: format!("Decl {i}"),
            title: None,
            source: WebMcpToolSource::Declarative,
            input_schema: None,
            annotations: None,
            action_url: None,
            method: None,
        });
    }
    let imp = vec![
        WebMcpToolDescriptor {
            name: "tool_5".to_string(),
            description: "Imperative override".to_string(),
            title: None,
            source: WebMcpToolSource::Imperative,
            input_schema: Some(
                json!({ "type": "object", "properties": { "code": { "type": "string", "description": "```rust\nfn x(){}\n```" } } }),
            ),
            annotations: Some(WebMcpToolAnnotations {
                read_only: Some(true),
                autosubmit: None,
                untrusted_content: Some(false),
            }),
            action_url: None,
            method: None,
        },
        WebMcpToolDescriptor {
            name: "tool_33".to_string(),
            description: "Should be capped".to_string(),
            title: None,
            source: WebMcpToolSource::Imperative,
            input_schema: None,
            annotations: None,
            action_url: None,
            method: None,
        },
    ];
    let merged = merge_webmcp_tools(decl_32, imp);
    assert_eq!(merged.len(), MAX_WEBMCP_TOOLS);
    let overridden = merged.iter().find(|t| t.name == "tool_5").unwrap();
    assert_eq!(overridden.source, WebMcpToolSource::Imperative);
    assert_eq!(overridden.description, "Imperative override");
    assert_eq!(
        overridden
            .annotations
            .as_ref()
            .and_then(|a| a.untrusted_content),
        Some(true)
    );
    assert!(!merged.iter().any(|t| t.name == "tool_33"));

    // Markdown injection escaping: pipe escaping in table + 4-backtick dynamic fence for ``` in schema.
    let manifest = format_webmcp_manifest_markdown(&[ansi.clone(), overridden.clone()]);
    assert!(manifest.contains("Title \\| pipe[1m"));
    assert!(manifest.contains("````json\n"));
    assert!(manifest.contains("\n````"));
    assert!(open_fence_at(&manifest, manifest.len()).is_none());
}

#[test]
fn axtree_formatter_promotes_containers_formats_values_and_escapes_injections() {
    use pleb::extract::format_axtree_from_cdp_json;

    let raw_cdp = r#"{
      "nodes": [
        {
          "nodeId": "root",
          "ignored": false,
          "role": { "type": "internalRole", "value": "RootWebArea" },
          "name": { "type": "computedString", "value": "Page \"Title\"\nLine2\\path\u001b[31m\u007f" },
          "childIds": ["gen", "ign", "list"]
        },
        {
          "nodeId": "gen",
          "ignored": false,
          "role": { "type": "role", "value": "generic" },
          "childIds": ["gc"]
        },
        {
          "nodeId": "gc",
          "ignored": false,
          "role": { "type": "role", "value": "GenericContainer" },
          "childIds": ["h1", "chk", "btn", "txt"]
        },
        {
          "nodeId": "h1",
          "ignored": false,
          "role": { "type": "role", "value": "heading" },
          "name": { "type": "computedString", "value": "Docs" },
          "properties": [
            { "name": "level", "value": { "type": "integer", "value": 2 } }
          ],
          "childIds": ["inline"]
        },
        {
          "nodeId": "inline",
          "ignored": false,
          "role": { "type": "internalRole", "value": "InlineTextBox" },
          "name": { "type": "computedString", "value": "Docs" }
        },
        {
          "nodeId": "chk",
          "ignored": false,
          "role": { "type": "role", "value": "checkbox" },
          "name": { "type": "computedString", "value": "Agree\r\nTerms" },
          "properties": [
            { "name": "checked", "value": { "type": "tristate", "value": "mixed" } },
            { "name": "required", "value": { "type": "boolean", "value": true } }
          ]
        },
        {
          "nodeId": "btn",
          "ignored": false,
          "role": { "type": "role", "value": "button" },
          "name": { "type": "computedString", "value": "Menu" },
          "properties": [
            { "name": "disabled", "value": { "type": "boolean", "value": true } },
            { "name": "expanded", "value": { "type": "boolean", "value": false } },
            { "name": "pressed", "value": { "type": "tristate", "value": "false" } },
            { "name": "selected", "value": { "type": "boolean", "value": true } },
            { "name": "focused", "value": { "type": "boolean", "value": true } }
          ]
        },
        {
          "nodeId": "txt",
          "ignored": false,
          "role": { "type": "role", "value": "textbox" },
          "name": { "type": "computedString", "value": "Email" },
          "value": { "type": "string", "value": "a@b.com\u001b[0m" }
        },
        {
          "nodeId": "ign",
          "ignored": true,
          "role": { "type": "internalRole", "value": "none" },
          "childIds": ["ign_child"]
        },
        {
          "nodeId": "ign_child",
          "ignored": true,
          "role": { "type": "internalRole", "value": "StaticText" },
          "name": { "type": "computedString", "value": "Secret hidden" }
        },
        {
          "nodeId": "list",
          "ignored": false,
          "role": { "type": "role", "value": "Ignored" },
          "childIds": ["none_node"]
        },
        {
          "nodeId": "none_node",
          "ignored": false,
          "role": { "type": "role", "value": "none" },
          "childIds": ["link"]
        },
        {
          "nodeId": "link",
          "ignored": false,
          "role": { "type": "role", "value": "link" },
          "name": { "type": "computedString", "value": "Home" },
          "properties": [
            { "name": "url", "value": { "type": "string", "value": "https://example.com/?q=\"1\"\n" } }
          ]
        }
      ]
    }"#;

    let formatted = format_axtree_from_cdp_json(raw_cdp).unwrap();
    let expected = concat!(
        "RootWebArea \"Page \\\"Title\\\"\\nLine2\\\\path[31m\"\n",
        "  heading \"Docs\" level=2\n",
        "  checkbox \"Agree\\r\\nTerms\" checked=mixed required=true\n",
        "  button \"Menu\" disabled=true expanded=false pressed=false selected=true focused=true\n",
        "  textbox \"Email\" value=\"a@b.com[0m\"\n",
        "  link \"Home\" url=\"https://example.com/?q=\\\"1\\\"\\n\""
    );
    assert_eq!(formatted, expected);
    assert!(!formatted.contains("Secret hidden"));
    assert!(!formatted.contains('\x1b'));
    assert!(!formatted.contains('\x7f'));

    // Parent-id-only & cyclic CDP envelopes terminate safely and promote skipped containers.
    let parent_cycle_cdp = r#"{
      "result": {
        "nodes": [
          { "nodeId": "r", "role": { "value": "RootWebArea" }, "childIds": ["a"] },
          { "nodeId": "a", "parentId": "r", "role": { "value": "generic" }, "childIds": ["b", "r"] },
          { "nodeId": "b", "parentId": "a", "role": { "value": "button" }, "name": { "value": "Click" } }
        ]
      }
    }"#;
    assert_eq!(
        format_axtree_from_cdp_json(parent_cycle_cdp).unwrap(),
        "RootWebArea\n  button \"Click\""
    );

    // Empty tree and invalid payloads
    assert_eq!(format_axtree_from_cdp_json(r#"{"nodes":[]}"#).unwrap(), "");
    assert!(format_axtree_from_cdp_json(r#"{"bad":1}"#).is_err());
    assert!(format_axtree_from_cdp_json("not json").is_err());
}

#[test]
fn webmcp_and_axtree_boundary_and_counterexample_guards() {
    use pleb::extract::webmcp::{MAX_ENUM_VALUES, MAX_SCHEMA_PROPERTIES, MAX_WEBMCP_TOOLS};
    use pleb::extract::{
        format_axtree_from_cdp_json, format_webmcp_cue, merge_webmcp_tools, sanitize_webmcp_tool,
        WebMcpToolDescriptor, WebMcpToolSource,
    };
    use serde_json::json;

    // 1. Explicit empty/whitespace `action` attributes target `page_url` (never `<base href>`),
    //    nested hidden spans inside `<label for>` are omitted, non-finite `min`/`max` are ignored,
    //    and radio fallback skips whitespace-only `toolparamdescription` without overwriting valid ones.
    let html = r#"<!doctype html><html><head>
      <title>Boundary</title>
      <base href="https://hijack.example.org/wrong/">
    </head><body><main>
      <form toolname="empty_action_tool" tooldescription="Empty action" action="">
        <label for="lbl_hid">Visible <span style="display:none">Hidden</span> Label</label>
        <input id="lbl_hid" name="q">
        <input type="number" name="n" min="NaN" max="Infinity">
        <input type="range" name="r" min="abc" max="5">
        <input type="radio" name="mode" toolparamdescription="   ">
        <input type="radio" name="mode" value="fast" toolparamdescription="Fast mode">
        <input type="radio" name="mode" value="slow" toolparamdescription="Should not overwrite">
      </form>
      <form toolname="ws_action_tool" tooldescription="Whitespace action" action="   "></form>
    </main></body></html>"#;
    let page_url = Url::parse("https://portal.example.com/app/tools?a=1#frag").unwrap();
    let doc = extract_document(
        html,
        &page_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(doc.tools.len(), 2);
    assert_eq!(
        doc.tools[0].action_url.as_deref(),
        Some("https://portal.example.com/app/tools?a=1")
    );
    assert_eq!(
        doc.tools[1].action_url.as_deref(),
        Some("https://portal.example.com/app/tools?a=1")
    );
    assert_eq!(
        doc.tools[0].input_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "q": { "type": "string", "description": "Visible Label" },
                "n": { "type": "number" },
                "r": { "type": "number", "maximum": 5 },
                "mode": { "type": "string", "enum": ["on", "fast", "slow"], "description": "Fast mode" }
            }
        }))
    );

    // 2. Verify adversarial fixture caps directly (input_schema dropped for `en`/`pr`/`by`,
    //    `dp` keeps first duplicate, `w1..w6` action URLs sanitized, `ds` <= 1024).
    let adv_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/corpus/frontier_m6_webmcp_adversarial_forms.html");
    let adv_raw = std::fs::read_to_string(&adv_path).unwrap();
    let adv_body = adv_raw.split_once('\n').unwrap().1;
    let adv_url = Url::parse("https://portal.example.com/app/tools").unwrap();
    let adv_doc = extract_document(
        adv_body,
        &adv_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert_eq!(adv_doc.tools.len(), MAX_WEBMCP_TOOLS);
    for (name, want_action) in [
        ("w1", Some("https://portal.example.com/app/tools")),
        ("w2", Some("https://portal.example.com/app/tools")),
        ("w3", None),
        ("w4", None),
        ("w5", None),
        ("w6", None),
    ] {
        let tool = adv_doc.tools.iter().find(|t| t.name == name).unwrap();
        assert_eq!(
            tool.action_url.as_deref(),
            want_action,
            "unexpected action_url for {name}"
        );
    }
    let dup = adv_doc.tools.iter().find(|t| t.name == "dp").unwrap();
    assert_eq!(dup.description, "1st");
    assert_eq!(dup.action_url.as_deref(), Some("https://e.org/1"));
    assert_eq!(
        dup.input_schema,
        Some(json!({ "type": "object", "properties": {} }))
    );
    for capped in ["en", "pr", "by"] {
        let tool = adv_doc.tools.iter().find(|t| t.name == capped).unwrap();
        assert_eq!(
            tool.input_schema, None,
            "{capped} schema should drop to None"
        );
    }
    let desc_tool = adv_doc.tools.iter().find(|t| t.name == "ds").unwrap();
    assert!(desc_tool.description.len() <= 1024);

    // 3. Exact boundary passes (64 props, 64 enum values, 256-byte item, depth 8) and multi-byte UTF-8 truncation.
    let mut props_64 = serde_json::Map::new();
    for i in 0..MAX_SCHEMA_PROPERTIES {
        props_64.insert(format!("p{i}"), json!({ "type": "string" }));
    }
    let enum_64: Vec<String> = (0..MAX_ENUM_VALUES).map(|i| format!("v{i}")).collect();
    props_64.insert(
        "p0".to_string(),
        json!({ "type": "string", "enum": enum_64 }),
    );
    let mut depth_8 = json!({ "type": "string" });
    for _ in 0..3 {
        depth_8 = json!({ "nested": depth_8 });
    }
    props_64.insert("p1".to_string(), depth_8);

    let boundary_tool = sanitize_webmcp_tool(WebMcpToolDescriptor {
        name: "boundary_ok".to_string(),
        description: "🦀".repeat(300),
        title: Some("🦀".repeat(100)),
        source: WebMcpToolSource::Imperative,
        input_schema: Some(json!({ "type": "object", "properties": props_64 })),
        annotations: None,
        action_url: None,
        method: None,
    })
    .unwrap();
    assert!(boundary_tool.description.len() <= 1024);
    assert!(boundary_tool.title.as_deref().unwrap().len() <= 256);
    assert!(boundary_tool.input_schema.is_some());

    // 4. `merge_webmcp_tools`: invalid imperative tool of same name does NOT clobber valid declarative tool,
    //    and `format_webmcp_cue` truncates at 8 names with `+N more`.
    let merged = merge_webmcp_tools(
        vec![WebMcpToolDescriptor {
            name: "keep_decl".to_string(),
            description: "Valid declarative".to_string(),
            title: None,
            source: WebMcpToolSource::Declarative,
            input_schema: None,
            annotations: None,
            action_url: None,
            method: None,
        }],
        vec![WebMcpToolDescriptor {
            name: "keep_decl".to_string(),
            description: "   \x1b  ".to_string(),
            title: None,
            source: WebMcpToolSource::Imperative,
            input_schema: None,
            annotations: None,
            action_url: None,
            method: None,
        }],
    );
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].source, WebMcpToolSource::Declarative);
    assert_eq!(merged[0].description, "Valid declarative");

    let cue = format_webmcp_cue(&adv_doc.tools).unwrap();
    assert!(cue.contains("+24 more"), "unexpected cue: {cue}");

    // 5. `format_axtree_from_cdp_json`: deep tree (> 512 depth) terminates cleanly, bidi controls stripped,
    //    `description` and numeric/array values escaped, non-tristate `checked` quoted.
    let mut deep_nodes = Vec::with_capacity(520);
    for i in 0..520 {
        let child = if i + 1 < 520 {
            vec![format!("n{}", i + 1)]
        } else {
            Vec::new()
        };
        deep_nodes.push(json!({
            "nodeId": format!("n{i}"),
            "role": { "value": if i == 0 { "RootWebArea" } else { "group" } },
            "name": { "value": if i == 0 { "Deep\u{202E}" } else { "" } },
            "childIds": child
        }));
    }
    let deep_out =
        format_axtree_from_cdp_json(&json!({ "nodes": deep_nodes }).to_string()).unwrap();
    assert!(deep_out.starts_with("RootWebArea \"Deep\"\n"));
    assert!(!deep_out.contains('\u{202E}'));
    assert_eq!(deep_out.lines().count(), 513);

    let prop_edge = json!({
        "nodes": [{
            "nodeId": "r",
            "role": { "value": "checkbox" },
            "name": { "value": "Accept\u{202E}" },
            "description": { "value": "Line1\nLine2" },
            "value": { "value": 42 },
            "properties": [
                { "name": "checked", "value": { "value": "invalid_state" } },
                { "name": "extra\u{202E}", "value": { "value": [1, "a"] } }
            ]
        }]
    });
    let prop_out = format_axtree_from_cdp_json(&prop_edge.to_string()).unwrap();
    assert_eq!(
        prop_out,
        "checkbox \"Accept\" value=42 description=\"Line1\\nLine2\" checked=\"invalid_state\" extra=\"[1,\\\"a\\\"]\""
    );
}

#[test]
fn concealed_channel_stripping_preserves_benign_styles_webmcp_and_full_mode() {
    let url = Url::parse("https://docs.example.com/security").unwrap();
    let html = r#"<!doctype html><html><head><title>Policy&#xE0041;</title></head><body><main>
        <h1>Policy</h1>
        <p style="opacity:0.05">Low-opacity caption preserved.</p>
        <p style="font-size:0.75rem">Small rem text preserved.</p>
        <p style="text-indent:-16px">Small negative indent preserved.</p>
        <span style="opacity:0">HIDDEN-OPACITY</span>
        <span style="font-size:0px !IMPORTANT">HIDDEN-FONTSIZE</span>
        <form toolname="cloaked_tool" tooldescription="Hidden" style="opacity:0"></form>
        <form toolname="visible_tool" tooldescription="Visible&#xE0049; action" style="opacity:0.9"></form>
        <p>Safe text&#xE0053;&#x2066; and emoji 👩&#x200D;💻.</p>
    </main></body></html>"#;

    let clean =
        extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert_eq!(clean.title.as_deref(), Some("Policy"));
    assert!(clean.content.contains("Low-opacity caption preserved."));
    assert!(clean.content.contains("Small rem text preserved."));
    assert!(clean.content.contains("Small negative indent preserved."));
    assert!(clean.content.contains("Safe text and emoji 👩\u{200D}💻."));
    assert!(!clean.content.contains("HIDDEN-OPACITY"));
    assert!(!clean.content.contains("HIDDEN-FONTSIZE"));
    assert!(!clean.content.contains('\u{E0053}'));
    assert!(!clean.content.contains('\u{2066}'));
    assert_eq!(clean.tools.len(), 1);
    assert_eq!(clean.tools[0].name, "visible_tool");
    assert_eq!(clean.tools[0].description, "Visible action");

    // ExtractMode::Full preserves verbatim text (including native markdown unicode) without mutating source.
    let full = extract_document(
        "Raw \u{E0041} text",
        &url,
        Some("text/markdown"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(full.content.contains('\u{E0041}'));
}

#[test]
fn focus_projection_recovers_late_sections_splits_identifiers_and_enforces_byte_and_fence_bounds() {
    use pleb::extract::focus::focus_document;
    use pleb::extract::markdown::open_fence_at;

    let mut md = String::from("# SDK Guide\n\n## Quick Start\n\nBasic setup notes.\n");
    for i in 1..=25 {
        md.push_str(&format!(
            "\n## Operational Section {i}\n\n{}\n",
            "Routine operational telemetry note explaining buffer tuning and keepalive heartbeats. "
                .repeat(12)
        ));
    }
    md.push_str(
        "\n## Migration Caveats\n\nCall `clear_poison` after recovery and pass `--tls-verify=strict`.\n\n```rust\nmutex.clear_poison();\n```\n",
    );

    // 1. Short budget recovers late section via identifier sub-token matching ("clear poison" -> clear_poison),
    // keeps code fence balanced, stays within max_bytes, and emits a bounded collapsed-sections map.
    let (focused, truncated) =
        focus_document(&md, "clear poison tls verify migration", 1_400, true);
    assert!(truncated);
    assert!(focused.len() <= 1_400);
    assert!(open_fence_at(&focused, focused.len()).is_none());
    assert!(focused.contains("clear_poison"));
    assert!(focused.contains("--tls-verify=strict"));
    assert!(focused.contains("<!-- Collapsed sections:"));

    // 2. Short document under budget is returned untouched with truncated=false.
    let short = "# Short\n\nFits easily.\n";
    let (same, trunc) = focus_document(short, "fits", 1_400, true);
    assert!(!trunc);
    assert_eq!(same, short);

    // 3. Tiny budgets never exceed max_bytes or leave unclosed fences.
    for cap in [32usize, 64, 128, 300, 700] {
        let (out, did_trunc) = focus_document(&md, "clear poison migration", cap, true);
        assert!(did_trunc);
        assert!(out.len() <= cap, "cap={cap}, got {}", out.len());
        assert!(
            open_fence_at(&out, out.len()).is_none(),
            "unclosed fence at cap={cap}"
        );
    }
}

#[test]
fn pilcrow_stripping_summary_heading_order_and_anchor_tooltip_compaction() {
    let url = Url::parse("https://docs.example.org/api").unwrap();
    let html = r##"<!doctype html><html><head><title>API</title></head><body><main>
        <h1>API</h1>
        <p id="p1">Body paragraph with <a href="/type.html" title="type std::Type">Type</a>.<a href="#p1" class="pilcrow">¶</a><a href="#p1">🔗</a></p>
        <p>Keep <a href="https://ext.example.org/doc">¶</a>, <a href="#s1">Section ¶</a>, <a href="#n">§</a>, and <img src="/fig.png" alt="Fig" title="Figure Caption">.</p>
        <details open><summary><section><span class="rightside">1.77.0 · <a href="/src.rs">Source</a></span><h4>pub fn clear_poison(&amp;self)</h4></section></summary><p>Clears poison.</p></details>
        <details open><summary><p>Block preface stays first.</p><h4>Block Summary</h4></summary><p>Body.</p></details>
    </main></body></html>"##;

    let md = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert!(md
        .content
        .contains("[Type](https://docs.example.org/type.html)"));
    assert!(!md.content.contains("type std::Type"));
    assert!(!md.content.contains("[¶](#p1)"));
    assert!(!md.content.contains("[🔗](#p1)"));
    assert!(md.content.contains("[¶](https://ext.example.org/doc)"));
    assert!(md.content.contains("[Section ¶](#s1)"));
    assert!(md.content.contains("[§](#n)"));
    assert!(md
        .content
        .contains("![Fig](https://docs.example.org/fig.png \"Figure Caption\")"));
    let clear_pos = md.content.find("#### pub fn clear\\_poison").unwrap();
    let since_pos = md.content.find("1.77.0 · [Source]").unwrap();
    assert!(
        clear_pos < since_pos,
        "expected heading before version badge:\n{}",
        md.content
    );
    let block_pre_pos = md.content.find("Block preface stays first.").unwrap();
    let block_h_pos = md.content.find("#### Block Summary").unwrap();
    assert!(
        block_pre_pos < block_h_pos,
        "block child inside <summary> must not reorder"
    );

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
    assert!(txt.content.find("pub fn clear_poison").unwrap() < txt.content.find("1.77.0").unwrap());
}

#[test]
fn clean_extract_preserves_article_prose_mentioning_ad_and_sidebar_tokens() {
    let url = Url::parse("https://docs.example.com/dom-patterns").unwrap();
    let html = r#"<!doctype html>
<html><head><title>DOM Patterns</title></head><body>
<div id="left-sidebar-container" class="left-sidebar"><button>Continue with Google</button></div>
<main><article>
  <h1>DOM Patterns</h1>
  <p>Classes such as promotedlink, sponsored-post, ad-slot, and left-sidebar frequently wrap feed promotions and side rails in modern web apps.</p>
</article>
<div class="thread-container"><p>Thread content stays intact.</p></div>
<div class="download-container"><p>Download binary details stay intact.</p></div>
<div class="lead-banner"><p>Lead summary stays intact.</p></div>
<div class="upload-slot"><p>Upload instructions stay intact.</p></div>
<shreddit-ad-post class="promotedlink"><div class="ad-link-bar"><a href="https://ads.example.com">Shop Now</a></div></shreddit-ad-post>
</main>
</body></html>"#;

    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    for expected in [
        "DOM Patterns",
        "promotedlink",
        "sponsored-post",
        "ad-slot",
        "left-sidebar",
        "Thread content stays intact.",
        "Download binary details stay intact.",
        "Lead summary stays intact.",
        "Upload instructions stay intact.",
    ] {
        assert!(
            doc.content.contains(expected),
            "prose token {expected:?} must be kept; got:\n{}",
            doc.content
        );
    }
    assert!(!doc.content.contains("Shop Now"));
    assert!(!doc.content.contains("Continue with Google"));
}

#[test]
fn clean_extract_preserves_sidebar_wrapper_containing_main_landmark_or_h1() {
    let url = Url::parse("https://docs.example.com/layout").unwrap();
    let wrapped_main = r#"<!doctype html>
<html><head><title>Wrapped Main</title></head><body>
<div class="left-sidebar"><main><h1>Real Title</h1><p>Body paragraph with substantial technical documentation content so clean extraction keeps it intact.</p></main></div>
</body></html>"#;
    let main_doc = extract_document(
        wrapped_main,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(main_doc.content.contains("Real Title"));
    assert!(main_doc
        .content
        .contains("substantial technical documentation"));

    let wrapped_h1 = r#"<!doctype html>
<html><head><title>Wrapped Heading</title></head><body>
<div class="right-sidebar"><h1>Standalone Heading</h1><p>Core article text inside a mislabeled wrapper must survive boilerplate attribute pruning.</p></div>
</body></html>"#;
    let h1_doc = extract_document(
        wrapped_h1,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(h1_doc.content.contains("Standalone Heading"));
    assert!(h1_doc.content.contains("Core article text"));
}

#[test]
fn compact_markdown_collapses_horizontal_rules_while_preserving_setext_and_fenced_blocks() {
    let input = "* * *\n\n# Top Section\n\n* * *\n\n* * *\n\nSetext Heading\n---\n\n```yaml\n---\nkey: value\n---\n```\n\nBody text.\n\n* * *\n\n---\n";
    let out = compact_markdown(input);

    assert!(
        out.starts_with("# Top Section"),
        "leading HR should be trimmed; got:\n{out}"
    );
    assert!(
        out.ends_with("Body text."),
        "trailing HRs should be trimmed; got:\n{out}"
    );
    assert_eq!(
        out.matches("* * *").count(),
        1,
        "consecutive outside-fence HRs should collapse to one; got:\n{out}"
    );
    assert!(
        out.contains("Setext Heading\n---"),
        "Setext H2 underline must remain intact; got:\n{out}"
    );
    assert!(
        out.contains("```yaml\n---\nkey: value\n---\n```"),
        "YAML frontmatter-style fenced block dividers must remain intact; got:\n{out}"
    );
}

#[test]
fn compact_markdown_collapses_duplicate_empty_alt_icons_and_keeps_distinct() {
    let input = "[![](https://cdn.example.com/a.png) ![](https://cdn.example.com/a.png) r/rust](https://example.com/r/rust)\n\n![](https://cdn.example.com/a.png) ![](https://cdn.example.com/b.png)\n\n![](https://en.wikipedia.org/wiki/Foo_(bar).png) ![](https://en.wikipedia.org/wiki/Foo_(bar).png) Wiki";
    let out = compact_markdown(input);

    assert!(out.contains("[![](https://cdn.example.com/a.png) r/rust](https://example.com/r/rust)"));
    assert!(
        out.contains("![](https://cdn.example.com/a.png) ![](https://cdn.example.com/b.png)"),
        "distinct icon images must both remain; got:\n{out}"
    );
    assert!(
        out.contains("![](https://en.wikipedia.org/wiki/Foo_(bar).png) Wiki"),
        "balanced-paren URL duplicate should collapse cleanly; got:\n{out}"
    );
    assert_eq!(out.matches("Foo_(bar).png").count(), 1);
}

#[test]
fn anchor_block_formatting_preserves_inline_hashes_code_and_balances_fences() {
    let url = Url::parse("https://docs.example.com/cards").unwrap();
    let html = r#"<!doctype html>
<html><head><title>Anchor Block Cards</title></head><body><main>
  <h1>Anchor Block Cards</h1>
  <p>
    <a href="/issue/123">#123</a>,
    <a href="/c">C#</a>,
    <a href="/fn"><code>fn main()</code></a>,
    <a href="/d">- 2026 release</a>
  </p>
  <a href="/p"><p>Single inline paragraph</p></a>
  <article>
    <a href="/card"><h2>Card Heading</h2><p>Card body</p></a>
  </article>
  <article>
    <a href="/code"><pre><code>let x = 1;</code></pre></a>
    <hr>
    <hr>
    <p>Trailing prose</p>
  </article>
</main></body></html>"#;

    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    for inline in [
        "[#123](https://docs.example.com/issue/123)",
        "[C#](https://docs.example.com/c)",
        "[`fn main()`](https://docs.example.com/fn)",
        "[Single inline paragraph](https://docs.example.com/p)",
        "2026 release](https://docs.example.com/d)",
    ] {
        assert!(
            doc.content.contains(inline),
            "inline anchor {inline:?} must stay inline; got:\n{}",
            doc.content
        );
    }
    assert!(doc
        .content
        .contains("## [Card Heading](https://docs.example.com/card)"));
    assert!(doc.content.contains("Card body"));
    assert!(!doc.content.contains("[## Card Heading"));
    assert!(open_fence_at(&doc.content, doc.content.len()).is_none());
    assert!(doc
        .content
        .contains("[https://docs.example.com/code](https://docs.example.com/code)"));
    assert!(doc.content.contains("let x = 1;"));
    assert!(doc.content.contains("Trailing prose"));
    assert_eq!(doc.content.matches("* * *").count(), 1);
}

#[test]
fn resource_sections_scope_to_articles_without_leaking_across_siblings() {
    let url = Url::parse("https://feed.example.com/r/rust/").unwrap();
    let feed_html = r#"<!doctype html>
<html><head><title>r/rust</title></head><body><main>
  <h1>r/rust</h1>
  <article>
    <h2>Hiring Thread</h2>
    <h3>Automotive / Hardware</h3>
    <p>Jobs list.</p>
    <img src="https://styles.redditmedia.com/t5_abc/styles/profileIcon_123.jpg?width=64&height=64" alt="u/alice avatar">
  </article>
  <article>
    <img src="https://i.redd.it/gpui-layout-diagram.png" width="800" height="450" alt="GPUI layout diagram">
    <h2>Building a GPUI Dashboard</h2>
    <p>UI notes.</p>
  </article>
  <article>
    <figure>
      <img src="https://i.redd.it/perf-chart.png" width="800" height="450" alt="Benchmark chart">
      <figcaption>Throughput comparison</figcaption>
    </figure>
    <p>No heading in this card.</p>
  </article>
</main></body></html>"#;

    let feed_doc = extract_document(
        feed_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    let gpui = feed_doc
        .resources
        .iter()
        .find(|r| r.url.ends_with("/gpui-layout-diagram.png"))
        .unwrap();
    let perf = feed_doc
        .resources
        .iter()
        .find(|r| r.url.ends_with("/perf-chart.png"))
        .unwrap();
    assert_eq!(
        gpui.section_heading.as_deref(),
        Some("Building a GPUI Dashboard")
    );
    assert_eq!(perf.section_heading.as_deref(), Some("r/rust"));

    // Single-article documentation page must still attribute each image to its nearest preceding heading.
    let doc_url = Url::parse("https://docs.example.com/arch").unwrap();
    let doc_html = r#"<!doctype html>
<html><head><title>Architecture</title></head><body><article>
  <h1>Architecture</h1>
  <h2>Section 1</h2>
  <figure><img src="/img/s1.png" width="800" height="400" alt="Section one diagram"><figcaption>S1</figcaption></figure>
  <h2>Section 2</h2>
  <figure><img src="/img/s2.png" width="800" height="400" alt="Section two diagram"><figcaption>S2</figcaption></figure>
</article></body></html>"#;

    let single_doc = extract_document(
        doc_html,
        &doc_url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    let s1 = single_doc
        .resources
        .iter()
        .find(|r| r.url.ends_with("/s1.png"))
        .unwrap();
    let s2 = single_doc
        .resources
        .iter()
        .find(|r| r.url.ends_with("/s2.png"))
        .unwrap();
    assert_eq!(s1.section_heading.as_deref(), Some("Section 1"));
    assert_eq!(s2.section_heading.as_deref(), Some("Section 2"));
}

#[test]
fn resource_sections_scope_list_items_and_recognize_slotted_or_aria_titles() {
    let url = Url::parse("https://feed.example.com/r/rust/").unwrap();
    let html = r#"<!doctype html>
<html><head><title>r/rust</title></head><body><main>
  <h1>r/rust</h1>
  <ul>
    <li><a href="/h"><h2>Pinned Highlight</h2></a></li>
  </ul>
  <article>
    <shreddit-post>
      <a slot="title" href="/p1">Slotted Post Title</a>
      <img width="800" height="450" src="/a.png" alt="Post diagram">
    </shreddit-post>
  </article>
  <article>
    <div role="heading">ARIA Post Title</div>
    <img width="800" height="450" src="/b.png" alt="ARIA diagram">
  </article>
  <article>
    <p>Headingless card falls back outside list highlight.</p>
    <figure><img src="/fallback.png" alt="Fallback diagram"><figcaption>Fallback Fig</figcaption></figure>
  </article>
  <section>
    <h2>Core Architecture</h2>
    <ul><li><h3>Sub-item</h3></li></ul>
    <figure><img src="/c.png" alt="Diagram"><figcaption>Fig 1</figcaption></figure>
  </section>
</main></body></html>"#;

    let doc = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    let find_sec = |suffix: &str| {
        doc.resources
            .iter()
            .find(|r| r.url.ends_with(suffix))
            .unwrap()
            .section_heading
            .clone()
    };
    assert_eq!(find_sec("/a.png").as_deref(), Some("Slotted Post Title"));
    assert_eq!(find_sec("/b.png").as_deref(), Some("ARIA Post Title"));
    assert_eq!(find_sec("/fallback.png").as_deref(), Some("r/rust"));
    assert_eq!(find_sec("/c.png").as_deref(), Some("Core Architecture"));
}

#[test]
fn clean_extract_prunes_hidden_lightbox_decorative_imgs_and_sr_custom_elements_with_responsive_guards(
) {
    let url = Url::parse("https://feed.example.com/r/rust/").unwrap();
    let html = r#"<!doctype html>
<html><head><title>r/rust</title></head><body>
  <div class="hidden"><main><h1>r/rust</h1>
    <article>
      <a slot="full-post-link" href="/p"><faceplate-screen-reader-content>Title</faceplate-screen-reader-content></a>
      <a slot="title" href="/p">Title</a>
      <img alt="" role="presentation" src="/bg.png">
      <img alt="Meaningful alt" role="presentation" src="/meaningful.png">
      <div class="lightboxed-content hidden"><img width="800" height="600" alt="Full post image" src="/full.jpeg"></div>
    </article>
    <div class="hidden md:block"><p>Responsive Tailwind kept.</p></div>
    <div class="d-none d-lg-flex"><p>Responsive Bootstrap kept.</p></div>
    <div class="overflow-hidden"><p>Overflow utility kept.</p></div>
    <div class="d-none"><p>Unconditional Bootstrap hidden pruned.</p></div>
  </main></div>
</body></html>"#;

    let clean =
        extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert_eq!(
        clean
            .content
            .matches("[Title](https://feed.example.com/p)")
            .count(),
        1,
        "screen-reader overlay duplicate link must be removed cleanly; got:\n{}",
        clean.content
    );
    assert!(!clean.content.contains("/bg.png"));
    assert!(clean
        .content
        .contains("![Meaningful alt](https://feed.example.com/meaningful.png)"));
    assert!(!clean.content.contains("/full.jpeg"));
    assert!(!clean
        .resources
        .iter()
        .any(|r| r.url.ends_with("/full.jpeg")));
    assert!(clean.content.contains("Responsive Tailwind kept."));
    assert!(clean.content.contains("Responsive Bootstrap kept."));
    assert!(clean.content.contains("Overflow utility kept."));
    assert!(!clean
        .content
        .contains("Unconditional Bootstrap hidden pruned."));

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
    assert!(full
        .content
        .contains("Unconditional Bootstrap hidden pruned."));
    assert!(full.content.contains("/bg.png"));
}

#[test]
fn block_anchor_dedups_repeated_permalink_while_preserving_first_and_inline_links() {
    let url = Url::parse("https://example.com/r/rust/").unwrap();
    let html = r#"<html><body><main>
        <article>
            <h2><a href="/r/rust/posts/1">First Post Title</a></h2>
            <a href="/r/rust/posts/1"><p>First paragraph body.</p><p>Second paragraph body.</p></a>
        </article>
        <article>
            <h2><a href="/r/rust/posts/2">Second Post Title</a></h2>
            <a href="/r/rust/posts/2"><p>Single paragraph summary.</p></a>
        </article>
        <article>
            <a href="/r/rust/posts/unseen"><p>Unseen block one.</p><p>Unseen block two.</p></a>
        </article>
        <p>Inline <a href="/r/rust/posts/inline">first inline</a> and <a href="/r/rust/posts/inline">second inline</a> stay linked.</p>
    </main></body></html>"#;

    let res = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(res
        .content
        .contains("[First Post Title](https://example.com/r/rust/posts/1)"));
    assert!(res.content.contains("First paragraph body."));
    assert!(res.content.contains("Second paragraph body."));
    assert!(!res
        .content
        .contains("[https://example.com/r/rust/posts/1](https://example.com/r/rust/posts/1)"));

    assert!(res
        .content
        .contains("[Second Post Title](https://example.com/r/rust/posts/2)"));
    assert!(res.content.contains("Single paragraph summary."));
    assert!(!res
        .content
        .contains("[Single paragraph summary.](https://example.com/r/rust/posts/2)"));

    assert!(res.content.contains(
        "[https://example.com/r/rust/posts/unseen](https://example.com/r/rust/posts/unseen)"
    ));
    assert!(res
        .content
        .contains("[first inline](https://example.com/r/rust/posts/inline)"));
    assert!(res
        .content
        .contains("[second inline](https://example.com/r/rust/posts/inline)"));
}

#[test]
fn section_selector_matches_exact_prefix_substring_and_subsections() {
    let html = r##"<!doctype html>
<html><head><title>Reference Docs</title></head>
<body><main>
  <h1>Reference Docs</h1>
  <h2>Overview</h2>
  <p>Introductory paragraph.</p>
  <h2>API Reference</h2>
  <p>Top-level API notes.</p>
  <h3><a href="#auth">Auth_Setup</a></h3>
  <p>Configure bearer tokens.</p>
  <h4>Token Scopes</h4>
  <p>Read and write scopes.</p>
  <h2>Changelog</h2>
  <p>Release history details.</p>
</main></body></html>"##;
    let url = Url::parse("https://example.com/docs").unwrap();

    // Exact match includes nested subsections and excludes sibling sections.
    let exact = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            section: Some("API Reference".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!exact.truncated);
    assert!(exact.next_offset.is_none());
    assert!(exact.warning.is_none());
    assert!(exact.content.contains("## API Reference"));
    assert!(exact.content.contains("Top-level API notes."));
    assert!(exact.content.contains("Auth"));
    assert!(exact.content.contains("Read and write scopes."));
    assert!(!exact.content.contains("Introductory paragraph."));
    assert!(!exact.content.contains("Release history details."));

    // Prefix and substring matching work case-insensitively.
    let prefix = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            section: Some("api".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(prefix.content.contains("## API Reference"));

    let substr = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            section: Some("Setup".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(substr.content.contains("Configure bearer tokens."));
    assert!(!substr.content.contains("## API Reference"));

    // Unknown or punctuation-only selector returns available headings and a warning.
    for missing_sel in ["Nonexistent Section", "###"] {
        let missing = extract_document(
            html,
            &url,
            Some("text/html"),
            &ExtractOptions {
                section: Some(missing_sel.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!missing.truncated);
        assert!(missing.next_offset.is_none());
        assert!(missing.warning.is_some());
        assert!(missing.content.contains("not found"));
        assert!(missing.content.contains("Available sections:"));
        assert!(missing.content.contains("API Reference"));
    }
}

#[test]
fn offset_pagination_preserves_fences_and_completes_monotonically() {
    let html = r#"<!doctype html>
<html><head><title>Async Guide</title></head>
<body><main>
  <h1>Async Guide</h1>
  <p>Walkthrough introduction covering task scheduling fundamentals cleanly.</p>
  <h2>Example Pipeline</h2>
  <pre><code class="language-rust">fn step_one() -> u32 { 1 }
fn step_two() -> u32 { 2 }
fn step_three() -> u32 { 3 }
fn step_four() -> u32 { 4 }
fn step_five() -> u32 { 5 }
fn step_six() -> u32 { 6 }
fn step_seven() -> u32 { 7 }
fn step_eight() -> u32 { 8 }
</code></pre>
  <h2>Closing Notes</h2>
  <p>Final summary paragraph after code fence.</p>
</main></body></html>"#;
    let url = Url::parse("https://example.com/async").unwrap();
    let full = extract_document(html, &url, Some("text/html"), &ExtractOptions::default()).unwrap();
    assert!(full.content.len() > 240);

    let mut offset = 0usize;
    let mut chunks = Vec::new();
    for _ in 0..24 {
        let page = extract_document(
            html,
            &url,
            Some("text/html"),
            &ExtractOptions {
                max_bytes: Some(120),
                offset,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            page.content.len() <= 120,
            "chunk exceeded max_bytes: {}",
            page.content.len()
        );
        assert!(
            pleb::extract::markdown::open_fence_at(&page.content, page.content.len()).is_none(),
            "chunk ended with unclosed fence:\n{}",
            page.content
        );
        chunks.push(page.content);
        match page.next_offset {
            Some(next) => {
                assert!(
                    next > offset,
                    "offset must strictly increase: {offset} -> {next}"
                );
                offset = next;
            }
            None => break,
        }
    }

    let joined = chunks.join("\n");
    assert!(
        chunks.iter().filter(|c| c.contains("```rust")).count() >= 2,
        "expected resumed chunk inside code fence to reopen ```rust fence:\n{joined}"
    );
    assert!(joined.contains("fn step_one()"));
    assert!(joined.contains("fn step_eight()"));
    assert!(joined.contains("Final summary paragraph after code fence."));
}

#[test]
fn tiny_fence_budget_stops_and_oob_offset_warns() {
    let html = r#"<!doctype html>
<html><head><title>Code</title></head>
<body><main>
  <pre><code class="language-rust">let alpha = 100;
let beta = 200;
let gamma = 300;
</code></pre>
</main></body></html>"#;
    let url = Url::parse("https://example.com/code").unwrap();
    let first = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            max_bytes: Some(50),
            ..Default::default()
        },
    )
    .unwrap();
    let mid_offset = first
        .next_offset
        .expect("expected first page to have next_offset");

    // `fence_prefix` (`"```rust\n"`, 8 bytes) + closing (`"\n```"`, 4 bytes) + `TRUNCATED_SUFFIX` (17 bytes)
    // leaves zero room inside 20 bytes, so pagination must not emit a looping `next_offset`.
    let tiny = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            max_bytes: Some(20),
            offset: mid_offset,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(tiny.next_offset.is_none());

    // Even when `max_bytes` <= `fp.len() + 1`, offset strictly advances or terminates cleanly.
    let sub_prefix = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            max_bytes: Some(9),
            offset: mid_offset,
            ..Default::default()
        },
    )
    .unwrap();
    if let Some(next) = sub_prefix.next_offset {
        assert!(next > mid_offset);
    }

    let oob = extract_document(
        html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            offset: 99_999,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(oob.content.is_empty());
    assert!(!oob.truncated);
    assert!(oob.next_offset.is_none());
    assert!(oob
        .warning
        .as_deref()
        .is_some_and(|w| w.contains("exceeds extracted content length")));
}

#[test]
fn section_with_offset_paginates_within_section_and_text_paginates() {
    let html = r#"<!doctype html>
<html><head><title>Handbook</title></head>
<body><main>
  <h2>Target Section</h2>
  <p>Sentence one of target section with details. Sentence two of target section continues.</p>
  <p>Sentence three of target section concludes nicely here.</p>
  <h2>Excluded Trailing</h2>
  <p>Should never be emitted inside section slice.</p>
</main></body></html>"#;
    let url = Url::parse("https://example.com/handbook").unwrap();

    let mut offset = 0usize;
    let mut collected = String::new();
    for _ in 0..10 {
        let slice = extract_document(
            html,
            &url,
            Some("text/html"),
            &ExtractOptions {
                section: Some("Target Section".to_string()),
                max_bytes: Some(90),
                offset,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!slice.content.contains("Excluded Trailing"));
        assert!(!slice.content.contains("Should never be emitted"));
        collected.push_str(&slice.content);
        collected.push('\n');
        match slice.next_offset {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert!(collected.contains("Sentence one"));
    assert!(collected.contains("Sentence three"));

    // Plain-text pagination uses byte boundaries cleanly without Markdown truncation markers/fences.
    let mut txt_offset = 0usize;
    let mut txt_all = String::new();
    for _ in 0..10 {
        let slice = extract_document(
            html,
            &url,
            Some("text/html"),
            &ExtractOptions {
                format: OutputFormat::Text,
                max_bytes: Some(80),
                offset: txt_offset,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(slice.content.len() <= 80);
        let raw_piece = slice
            .content
            .strip_suffix("\n\n[... truncated]")
            .unwrap_or(&slice.content);
        txt_all.push_str(raw_piece);
        match slice.next_offset {
            Some(next) => {
                assert!(next > txt_offset);
                txt_offset = next;
            }
            None => break,
        }
    }
    assert!(txt_all.contains("Target Section"));
    assert!(txt_all.contains("Should never be emitted"));
}

#[test]
fn milestone2_inverse_false_positives_preserve_valid_content() {
    let url = Url::parse("https://example.com/inverse").unwrap();

    // 1. `<pre><code class="language-rust">` preserves `rust`; `paintbrush: red` is ignored; `brush:cpp;` extracts `cpp`.
    let code_html = r#"<!doctype html>
<html><head><title>Code</title></head>
<body><main>
  <pre><code class="language-rust">fn main() { println!("hi"); }
</code></pre>
  <pre class="paintbrush: red"><code>plain text block
</code></pre>
  <pre class="brush:cpp; notranslate"><code>int x = 1;
int y = 2;
</code></pre>
</main></body></html>"#;
    let code_doc = extract_document(
        code_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(code_doc.content.contains("```rust\nfn main()"));
    assert!(!code_doc.content.contains("```red"));
    assert!(code_doc
        .content
        .contains("```cpp\nint x = 1;\nint y = 2;\n```"));

    // 2. `<dialog open>` preserved in Clean mode; closed `<dialog>` stripped in Clean and preserved in Full.
    let dialog_html = r#"<!doctype html>
<html><head><title>Dialogs</title></head>
<body>
  <dialog id="closed-modal"><p>Secret modal prompt.</p></dialog>
  <main>
    <h1>Dialogs</h1>
    <dialog open><p>Visible notice dialog.</p></dialog>
    <p>Main article body.</p>
  </main>
</body></html>"#;
    let clean_dialog = extract_document(
        dialog_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(clean_dialog.content.contains("Visible notice dialog."));
    assert!(!clean_dialog.content.contains("Secret modal prompt."));
    let full_dialog = extract_document(
        dialog_html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(full_dialog.content.contains("Secret modal prompt."));

    // 3. Headerless 2-column math table with prose cell and `<thead>` math table remain Markdown pipe tables.
    let table_html = r#"<!doctype html>
<html><head><title>Tables</title></head>
<body><main>
  <h1>Tables</h1>
  <table>
    <tbody>
      <tr><td>Cross-entropy</td><td><math alttext="H(p,q)"><semantics><annotation encoding="application/x-tex">-\sum y \log p</annotation></semantics></math></td></tr>
    </tbody>
  </table>
  <table>
    <thead><tr><th>Metric</th><th>Expr</th></tr></thead>
    <tbody><tr><td>KL</td><td><math alttext="D_{KL}"><semantics><annotation encoding="application/x-tex">D_{KL}</annotation></semantics></math></td></tr></tbody>
  </table>
</main></body></html>"#;
    let table_doc = extract_document(
        table_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(table_doc.content.contains("| Cross-entropy |"));
    assert!(table_doc.content.contains("| Metric | Expr |"));

    // 4. Empty ATX heading pruning preserves `## Real Title`, `#######`, and bare `#` / `##` inside code fences,
    // while removing bare `##` / `###` outside fences without creating double blank lines.
    let heading_html = r#"<!doctype html>
<html><head><title>Headings</title></head>
<body><main>
  <h2><a href="" rel="bookmark"></a></h2>
  <h1>Headings</h1>
  <p>First paragraph.</p>
  <h3>   </h3>
  <p>Second paragraph.</p>
  <h2>Real Title</h2>
  <p>#######</p>
  <pre><code class="language-bash">#
##
echo still here
</code></pre>
</main></body></html>"#;
    let heading_doc = extract_document(
        heading_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(heading_doc.content.starts_with("# Headings\n"));
    let outside_fence = heading_doc
        .content
        .split_once("```")
        .map_or(heading_doc.content.as_str(), |(before, _)| before);
    assert!(!outside_fence
        .lines()
        .any(|l| l.trim() == "##" || l.trim() == "###"));
    assert!(heading_doc.content.contains("## Real Title"));
    assert!(heading_doc.content.contains("#######"));
    assert!(heading_doc
        .content
        .contains("```bash\n#\n##\necho still here\n```"));
    assert!(!heading_doc.content.contains("\n\n\n"));
}

#[test]
fn canary_fixes_preserve_inverse_false_positives_across_clean_and_full_modes() {
    let url = Url::parse("https://docs.example.com/guide").unwrap();

    // 1. Collapsed ARIA disclosure panels: prune outside landmarks (or in chrome header),
    //    preserve inside <main>, expanded panels, landmark-less bare FAQs, <main id=...>, and Full mode.
    let aria_html = r#"<!doctype html>
<html><head><title>Guide | Docs</title></head><body>
  <section class="banner">
    <header><button aria-expanded="false" aria-controls="banner-panel">Toggle</button></header>
    <div id="banner-panel"><p>Chrome disclosure boilerplate.</p></div>
  </section>
  <button aria-expanded="true" aria-controls="open-panel">Open</button>
  <div id="open-panel"><p>Expanded outside panel stays.</p></div>
  <button aria-expanded="false" aria-controls="main-landmark">Main Toggle</button>
  <main id="main-landmark">
    <h1>Guide</h1>
    <button aria-expanded="false" aria-controls="faq-in-main">FAQ</button>
    <div id="faq-in-main"><p>Accordion inside main stays.</p></div>
  </main>
</body></html>"#;
    let aria_clean = extract_document(
        aria_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(!aria_clean
        .content
        .contains("Chrome disclosure boilerplate."));
    assert!(aria_clean.content.contains("Expanded outside panel stays."));
    assert!(aria_clean.content.contains("Accordion inside main stays."));

    let bare_faq_html = r#"<!doctype html><html><head><title>FAQ</title></head><body>
      <button aria-expanded="false" aria-controls="a1">Question 1</button>
      <div id="a1"><p>Bare page FAQ answer stays.</p></div>
    </body></html>"#;
    let bare_faq_doc = extract_document(
        bare_faq_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(bare_faq_doc.content.contains("Bare page FAQ answer stays."));

    let aria_full = extract_document(
        aria_html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(aria_full.content.contains("Chrome disclosure boilerplate."));

    // 2. Empty-alt small/button UI icons are pruned while unsized <img>, large <img>, meaningful alt, and Full stay.
    let icons_html = r#"<!doctype html><html><head><title>Icons</title></head><body><main>
      <h1>Icons</h1>
      <button><img alt="" src="/btn.svg"></button>
      <p><img width="24" height="24" alt="" src="/small.svg"> <img alt="" src="/unsized.png"> <img width="24" height="24" alt="Alert" src="/alert.svg"> <img width="800" height="400" alt="" src="/hero.png"></p>
    </main></body></html>"#;
    let icons_clean = extract_document(
        icons_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(!icons_clean.content.contains("/btn.svg"));
    assert!(!icons_clean.content.contains("/small.svg"));
    assert!(icons_clean.content.contains("/unsized.png"));
    assert!(icons_clean.content.contains("/alert.svg"));
    assert!(icons_clean.content.contains("/hero.png"));

    let icons_full = extract_document(
        icons_html,
        &url,
        Some("text/html"),
        &ExtractOptions {
            mode: ExtractMode::Full,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(icons_full.content.contains("/btn.svg"));
    assert!(icons_full.content.contains("/small.svg"));

    // 3. Nested layout tables unwrap while standalone headerless tables and outer tables with <thead>/<th> stay tables.
    let tables_html = r#"<!doctype html><html><head><title>Tables</title></head><body><main>
      <h1>Tables</h1>
      <table><tr><td>Key</td><td>Val</td></tr><tr><td>Foo</td><td>Bar</td></tr></table>
      <table><thead><tr><th>Outer</th><th>Nested</th></tr></thead><tbody><tr><td>A</td><td><table><tr><td>X</td><td>Y</td></tr></table></td></tr></tbody></table>
    </main></body></html>"#;
    let tables_clean = extract_document(
        tables_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(tables_clean.content.contains("| Key | Val |"));
    assert!(tables_clean.content.contains("| Outer | Nested |"));

    // 4. Title suffix + leading metadata H1 dedup and consecutive duplicate heading suppression preserve non-adjacent/fenced headings.
    let meta_h1_html = r#"<!doctype html><html><head><title>Cloudflare | Deep Dive</title></head><body><main>
      <p>February 12, 2026</p>
      <p>5 min read</p>
      <h1>Deep Dive</h1>
      <h1>Deep Dive</h1>
      <p>First body paragraph.</p>
      <h2>Example</h2>
      <p>First example.</p>
      <h2>Example</h2>
      <p>Second example separated by prose.</p>
      <pre><code>## Fenced
## Fenced</code></pre>
    </main></body></html>"#;
    let meta_h1_doc = extract_document(
        meta_h1_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    assert!(!meta_h1_doc.content.contains("# Cloudflare | Deep Dive"));
    assert_eq!(meta_h1_doc.content.matches("# Deep Dive").count(), 1);
    assert_eq!(meta_h1_doc.content.matches("## Example").count(), 2);
    assert!(meta_h1_doc.content.contains("## Fenced\n## Fenced"));
}

#[test]
fn merge_revealed_panels_sanitizes_dedups_merges_resources_and_reconciles_gates() {
    use pleb::extract::{merge_revealed_panels, ui_gates::UiGateKind, ui_gates::UiGatePresence};
    use pleb::transport::reveal::{RevealedPanelKind, RevealedPanelSnapshot};

    let url = Url::parse("https://example.com/docs").unwrap();
    let base_html = r#"<!doctype html><html><head><title>SDK</title></head><body><main>
      <div role="tablist">
        <button role="tab" aria-selected="true">Python</button>
        <button role="tab" aria-selected="false">Go</button>
      </div>
      <div role="tabpanel"><p>pip install sdk</p></div>
      <button aria-expanded="false">Troubleshooting</button>
    </main></body></html>"#;

    let panels = vec![
        // Duplicate of already-extracted active tab panel -> omitted
        RevealedPanelSnapshot {
            kind: RevealedPanelKind::Tab,
            label: "Python".into(),
            section_heading: None,
            heading_level: 3,
            html: "<div role=\"tabpanel\"><p>pip install sdk</p></div>".into(),
        },
        // Newly mounted panel containing concealed text + legitimate resource link
        RevealedPanelSnapshot {
            kind: RevealedPanelKind::Tab,
            label: "Go".into(),
            section_heading: None,
            heading_level: 3,
            html: "<div><p>go get sdk</p><div style=\"display:none\">SECRET_HIDDEN</div><img src=\"/diagrams/flow.png\" width=\"600\" height=\"300\" alt=\"Flow\"><script>alert(1)</script></div>".into(),
        },
        // Expanded disclosure
        RevealedPanelSnapshot {
            kind: RevealedPanelKind::Disclosure,
            label: "Troubleshooting".into(),
            section_heading: None,
            heading_level: 3,
            html: "<div><p>Check timeout settings.</p></div>".into(),
        },
    ];

    // 1. Markdown mode: sanitizes hidden DOM, deduplicates, appends Go & Troubleshooting, resolves ui_gates.
    let mut md_doc = extract_document(
        base_html,
        &url,
        Some("text/html"),
        &ExtractOptions::default(),
    )
    .unwrap();
    merge_revealed_panels(&mut md_doc, &panels, true, &url, &ExtractOptions::default());
    assert!(md_doc.content.contains("### Tab: Go"));
    assert!(md_doc.content.contains("go get sdk"));
    assert!(md_doc.content.contains("### Disclosure: Troubleshooting"));
    assert!(md_doc.content.contains("Check timeout settings."));
    assert!(!md_doc.content.contains("SECRET_HIDDEN"));
    assert!(!md_doc.content.contains("alert(1)"));
    assert_eq!(md_doc.content.matches("pip install sdk").count(), 1);
    assert!(md_doc
        .resources
        .iter()
        .any(|r| r.url == "https://example.com/diagrams/flow.png"));
    // Both Python/Go tablist and Troubleshooting disclosure are now resolved -> removed (or none unmounted)!
    assert!(!md_doc
        .ui_gates
        .iter()
        .any(|g| g.kind == UiGateKind::Tablist && g.presence == UiGatePresence::Unmounted));

    // 2. HTML mode: revealed sections must also run through pruned sanitization (no <script> or display:none leak).
    let html_opts = ExtractOptions {
        format: OutputFormat::Html,
        ..Default::default()
    };
    let mut html_doc = extract_document(base_html, &url, Some("text/html"), &html_opts).unwrap();
    merge_revealed_panels(&mut html_doc, &panels, false, &url, &html_opts);
    assert!(html_doc.content.contains("go get sdk"));
    assert!(!html_doc.content.contains("SECRET_HIDDEN"));
    assert!(!html_doc.content.contains("alert(1)"));
}
