// Basic markdown extraction smoke tests until test suite rewrite.

#[test]
fn test_html_to_markdown_basic() {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style"])
        .build();
    let md = converter.convert("<h1>Hello</h1><p>World</p>").unwrap();

    assert!(md.contains("Hello"), "should contain heading text");
    assert!(md.contains("World"), "should contain paragraph text");
}

#[test]
fn test_html_to_markdown_table() {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style"])
        .build();
    let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>";
    let md = converter.convert(html).unwrap();

    assert!(md.contains("|"), "should produce markdown table with pipes");
    assert!(md.contains("A"), "should contain header");
}

#[test]
fn test_html_to_markdown_links() {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style"])
        .build();
    let md = converter
        .convert(r#"<a href="https://example.com">click</a>"#)
        .unwrap();

    assert!(
        md.contains("[click](https://example.com)"),
        "should produce markdown link, got: {}",
        md
    );
}
