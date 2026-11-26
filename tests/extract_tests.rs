use mesh_wick::extract::markdown::{compact_markdown, open_fence_at};
use mesh_wick::extract::url::{clean_url, resolve_and_clean_url, ResolvedUrl};
use mesh_wick::extract::{
    decode_text_bytes, detect_binary_content, extract_document, extract_document_bytes,
    truncate_markdown, truncate_utf8, ExtractMode, ExtractOptions, OutputFormat, ParsedHtml,
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
