use mesh_wick::search::{
    build_brave_search_url, build_ddg_search_url, build_effective_query, clean_ddg_url,
    format_results, format_urls_only, normalize_site, parse_brave_json, parse_ddg_html, Freshness,
    SearchResponse, SearchResult,
};

const DDG_FIXTURE: &str = r#"
<html>
<body>
<div class="results">
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/y.js?ad_domain=ad.example">
          Sponsored Result
        </a>
      </h2>
      <div class="result__snippet">Ad copy</div>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="result__body">
      <h2 class="result__title">
        <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc123">
          Rust   Programming
          Language
        </a>
      </h2>
      <div class="result__snippet">
        A   language empowering   everyone to build reliable software.
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
fn parse_brave_json_extracts_extra_snippets_and_limits_results() {
    let payload = r#"{
      "web": {
        "results": [
          {
            "title": "<strong>First</strong> &amp; Best",
            "url": "https://a.example",
            "description": "<strong>Rust</strong> &amp; <em>&#x27;Cargo&#x27;</em> guide",
            "extra_snippets": ["  First <b>extra</b> line  ", ""]
          },
          { "title": "<b></b>", "url": "https://empty-title.example", "description": "skip" },
          { "title": "Second", "url": "https://b.example", "description": "Snippet B" },
          { "title": "Third", "url": "https://c.example", "description": "Snippet C" }
        ]
      }
    }"#;

    let results = parse_brave_json(payload, 2).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First & Best");
    assert_eq!(results[0].url, "https://a.example");
    assert_eq!(results[0].snippet, "Rust & 'Cargo' guide");
    assert_eq!(results[0].extra_snippets, vec!["First extra line"]);
    assert_eq!(results[1].title, "Second");
    assert!(results[1].extra_snippets.is_empty());

    let json = serde_json::to_string(&results).unwrap();
    assert!(json.contains("extra_snippets"));
    let second_only = serde_json::to_string(&results[1]).unwrap();
    assert!(!second_only.contains("extra_snippets"));
}

#[test]
fn parse_ddg_html_collapses_whitespace_filters_internal_links_and_unwraps_uddg() {
    let results = parse_ddg_html(DDG_FIXTURE, 5).unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Rust Programming Language");
    assert_eq!(results[0].url, "https://rust-lang.org/");
    assert_eq!(
        results[0].snippet,
        "A language empowering everyone to build reliable software."
    );
    assert_eq!(results[1].url, "https://doc.rust-lang.org/book/");

    assert_eq!(
        clean_ddg_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_%28programming_language%29&rut=xyz"),
        "https://en.wikipedia.org/wiki/Rust_(programming_language)"
    );
}

#[test]
fn search_query_normalization_freshness_urls_and_formatting() {
    assert_eq!(
        normalize_site("  HTTPS://docs.rs/tokio/ "),
        Some("docs.rs/tokio".to_string())
    );
    assert_eq!(normalize_site("  "), None);
    assert_eq!(
        build_effective_query("async spawn", Some("https://docs.rs/")),
        "site:docs.rs async spawn"
    );

    let brave_url = build_brave_search_url("rust", 5, Some(Freshness::Week), true);
    assert!(brave_url.contains("count=5"));
    assert!(brave_url.contains("freshness=pw"));
    assert!(brave_url.contains("extra_snippets=true"));

    let ddg_url = build_ddg_search_url("rust", Some(Freshness::Month));
    assert!(ddg_url.contains("df=m"));

    let empty = SearchResponse {
        provider: "brave",
        results: vec![],
    };
    assert_eq!(format_results(&empty), "No results found.");
    assert_eq!(format_urls_only(&empty), "");

    let populated = SearchResponse {
        provider: "brave",
        results: vec![
            SearchResult {
                title: "Example".to_string(),
                url: "https://example.com".to_string(),
                snippet: "A sample result".to_string(),
                extra_snippets: vec!["More context".to_string()],
            },
            SearchResult {
                title: "Two".to_string(),
                url: "https://two.example".to_string(),
                snippet: "Second".to_string(),
                extra_snippets: vec![],
            },
        ],
    };
    let formatted = format_results(&populated);
    assert!(formatted.contains("1. [Example](https://example.com)"));
    assert!(formatted.contains("A sample result"));
    assert!(formatted.contains("> More context"));
    assert_eq!(
        format_urls_only(&populated),
        "https://example.com\nhttps://two.example"
    );
}
