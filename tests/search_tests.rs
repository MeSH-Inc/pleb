use mesh_wick::search::{
    clean_ddg_url, format_results, parse_brave_json, parse_ddg_html, SearchResponse, SearchResult,
};

const DDG_FIXTURE: &str = r#"
<html>
<body>
<div class="results">
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc123">
          Rust Programming Language
        </a>
      </h2>
      <div class="result__snippet">
        A language empowering everyone to build reliable software.
      </div>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="https://doc.rust-lang.org/book/">
          The Rust Programming Language - Book
        </a>
      </h2>
      <div class="result__snippet">
        The official book on the Rust programming language.
      </div>
    </div>
  </div>
</div>
</body>
</html>
"#;

#[test]
fn parse_brave_json_extracts_and_limits_results() {
    let payload = r#"{
      "web": {
        "results": [
          { "title": "First", "url": "https://a.example", "description": "Snippet A" },
          { "title": "", "url": "https://empty-title.example", "description": "skip" },
          { "title": "Second", "url": "https://b.example", "description": "Snippet B" },
          { "title": "Third", "url": "https://c.example", "description": "Snippet C" }
        ]
      }
    }"#;

    let results = parse_brave_json(payload, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First");
    assert_eq!(results[0].url, "https://a.example");
    assert_eq!(results[0].snippet, "Snippet A");
    assert_eq!(results[1].title, "Second");
}

#[test]
fn parse_ddg_html_and_unwrap_uddg() {
    let results = parse_ddg_html(DDG_FIXTURE, 5).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Rust Programming Language");
    assert_eq!(results[0].url, "https://rust-lang.org/");
    assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");

    assert_eq!(
        clean_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_%28programming_language%29&rut=xyz"),
        "https://en.wikipedia.org/wiki/Rust_(programming_language)"
    );
}

#[test]
fn format_results_handles_empty_and_non_empty() {
    let empty = SearchResponse {
        provider: "brave",
        results: vec![],
    };
    assert_eq!(format_results(&empty), "No results found.");

    let populated = SearchResponse {
        provider: "brave",
        results: vec![SearchResult {
            title: "Example".to_string(),
            url: "https://example.com".to_string(),
            snippet: "A sample result".to_string(),
        }],
    };
    let formatted = format_results(&populated);
    assert!(formatted.contains("1. [Example](https://example.com)"));
    assert!(formatted.contains("A sample result"));
}
