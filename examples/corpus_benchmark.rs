use pleb::extract::{
    decode_text_bytes, extract_document_parsed, is_html_content_type, ExtractMode, ExtractOptions,
    OutputFormat,
};
use pleb::policy::{classify_response_with_context, ResponseClassification};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Instant;
use url::Url;

const DEFAULT_ITERATIONS: usize = 50;
const MAX_ITERATIONS: usize = 10_000;

#[derive(Debug, Deserialize)]
struct FixtureSpec {
    url: String,
    final_url: Option<String>,
    #[serde(default = "default_status")]
    status_code: u16,
    content_type: Option<String>,
    #[serde(default)]
    expected_exit_code: i32,
    expected_response_classification: Option<String>,
    #[serde(default)]
    exact_passthrough: bool,
}

fn default_status() -> u16 {
    200
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    iterations: usize,
    total_fixtures: usize,
    extractable_fixtures: usize,
    token_estimate_formula: &'static str,
    throughput_mib_per_sec: f64,
    classification_gate: ClassificationGateSummary,
    categories: Vec<CategorySummary>,
    all_extractable: CategorySummary,
    fixtures: Vec<FixtureResult>,
    gate_cases: Vec<ClassificationGateResult>,
}

#[derive(Debug, Serialize)]
struct ClassificationGateSummary {
    count: usize,
    matched: usize,
}

#[derive(Debug, Serialize)]
struct CategorySummary {
    category: String,
    fixtures: usize,
    raw_bytes: usize,
    extracted_bytes: usize,
    reduction_percent: f64,
    raw_est_tokens: usize,
    extracted_est_tokens: usize,
    mean_us: f64,
    total_mean_ms: f64,
}

#[derive(Debug, Serialize)]
struct FixtureResult {
    fixture: String,
    category: String,
    content_type: String,
    raw_bytes: usize,
    extracted_bytes: usize,
    reduction_percent: f64,
    raw_est_tokens: usize,
    extracted_est_tokens: usize,
    resource_count: usize,
    mean_us: f64,
}

#[derive(Debug, Serialize)]
struct ClassificationGateResult {
    fixture: String,
    status_code: u16,
    raw_bytes: usize,
    classification: String,
    expected_classification: Option<String>,
    matched: bool,
}

struct Config {
    json: bool,
    iterations: usize,
}

fn parse_args() -> Result<Option<Config>, String> {
    let mut json = false;
    let mut iterations = DEFAULT_ITERATIONS;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => json = true,
            "--iterations" => {
                let val = args
                    .next()
                    .ok_or_else(|| "missing value for --iterations".to_string())?;
                iterations = parse_iterations(&val)?;
            }
            _ if arg.starts_with("--iterations=") => {
                let val = &arg["--iterations=".len()..];
                iterations = parse_iterations(val)?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }

    Ok(Some(Config { json, iterations }))
}

fn parse_iterations(raw: &str) -> Result<usize, String> {
    let n: usize = raw
        .parse()
        .map_err(|_| format!("invalid --iterations value: {raw}"))?;
    if !(1..=MAX_ITERATIONS).contains(&n) {
        return Err(format!(
            "--iterations must be between 1 and {MAX_ITERATIONS}, got {n}"
        ));
    }
    Ok(n)
}

fn print_usage() {
    println!(
        "Usage: corpus_benchmark [--json] [--iterations N]\n\n\
         Measures deterministic byte/token reduction and warm release extraction\n\
         latency across all fixtures in tests/corpus/.\n\n\
         Options:\n  \
         --json               Emit JSON instead of Markdown\n  \
         --iterations <N>     Warm iterations per fixture (1..={MAX_ITERATIONS}, default {DEFAULT_ITERATIONS})\n  \
         -h, --help           Show this help message"
    );
}

fn classification_name(cls: &ResponseClassification) -> &'static str {
    match cls {
        ResponseClassification::Ok => "ok",
        ResponseClassification::Blocked { .. } | ResponseClassification::RateLimited { .. } => {
            "blocked"
        }
        ResponseClassification::Challenge { .. } => "challenge",
        ResponseClassification::JavaScriptShell { .. } => "javascript_shell",
        ResponseClassification::HttpError { .. } => "http_error",
    }
}

fn classify_fixture_category(
    spec: &FixtureSpec,
    cls_name: &str,
    is_html: bool,
) -> Option<&'static str> {
    if cls_name != "ok" || spec.expected_exit_code != 0 {
        return None;
    }
    if spec.exact_passthrough {
        return Some("passthrough");
    }
    if is_html {
        return Some("html");
    }
    let ct_lower = spec
        .content_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    if ct_lower.contains("markdown") {
        return Some("native_markdown");
    }
    if ct_lower.contains("vtt") || ct_lower.contains("subrip") {
        return Some("transcript");
    }
    Some("feed")
}

fn reduction_pct(raw_bytes: usize, extracted_bytes: usize) -> f64 {
    ((raw_bytes as f64 - extracted_bytes as f64) / raw_bytes.max(1) as f64) * 100.0
}

fn load_fixture(path: &Path) -> Result<(String, FixtureSpec, Vec<u8>), String> {
    let raw = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let nl = raw
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| format!("missing header newline in {}", path.display()))?;
    let header = std::str::from_utf8(&raw[..nl])
        .map_err(|e| format!("non-utf8 header in {}: {e}", path.display()))?
        .trim();
    let json = header
        .strip_prefix("<!-- pleb-fixture:")
        .and_then(|s| s.strip_suffix("-->"))
        .ok_or_else(|| format!("missing pleb-fixture frontmatter in {}", path.display()))?
        .trim();
    let spec: FixtureSpec = serde_json::from_str(json)
        .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((name, spec, raw[nl + 1..].to_vec()))
}

fn summarize_category(name: &str, items: &[&FixtureResult]) -> CategorySummary {
    let raw_bytes: usize = items.iter().map(|r| r.raw_bytes).sum();
    let extracted_bytes: usize = items.iter().map(|r| r.extracted_bytes).sum();
    let total_mean_us: f64 = items.iter().map(|r| r.mean_us).sum();
    let count = items.len();
    CategorySummary {
        category: name.to_string(),
        fixtures: count,
        raw_bytes,
        extracted_bytes,
        reduction_percent: reduction_pct(raw_bytes, extracted_bytes),
        raw_est_tokens: raw_bytes.div_ceil(4),
        extracted_est_tokens: extracted_bytes.div_ceil(4),
        mean_us: if count > 0 {
            total_mean_us / count as f64
        } else {
            0.0
        },
        total_mean_ms: total_mean_us / 1_000.0,
    }
}

fn run_benchmark(config: &Config) -> Result<BenchmarkReport, String> {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut paths: Vec<PathBuf> = fs::read_dir(&corpus_dir)
        .map_err(|e| format!("read {}: {e}", corpus_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "html"))
        .collect();
    paths.sort();

    if paths.is_empty() {
        return Err("tests/corpus must contain fixtures".to_string());
    }

    let opts = ExtractOptions {
        format: OutputFormat::Markdown,
        mode: ExtractMode::Clean,
        include_images: true,
        include_links: true,
        max_bytes: None,
        ..Default::default()
    };

    let mut fixtures = Vec::new();
    let mut gate_cases = Vec::new();

    for path in &paths {
        let (name, spec, body_bytes) = load_fixture(path)?;
        let raw_bytes = body_bytes.len();
        let body_str = decode_text_bytes(&body_bytes, spec.content_type.as_deref());
        let is_html = is_html_content_type(spec.content_type.as_deref());
        let effective_url = spec.final_url.as_deref().unwrap_or(&spec.url);
        let mut parsed = None;

        let cls = classify_response_with_context(
            spec.status_code,
            is_html,
            Some(&spec.url),
            Some(effective_url),
            &body_str,
            &mut parsed,
        );
        let cls_name = classification_name(&cls);
        if let Some(exp) = &spec.expected_response_classification {
            if cls_name != exp {
                return Err(format!(
                    "{name}: response classification mismatch: got {cls_name}, want {exp}"
                ));
            }
        }

        let Some(category) = classify_fixture_category(&spec, cls_name, is_html) else {
            let matched = spec
                .expected_response_classification
                .as_deref()
                .is_none_or(|exp| exp == cls_name);
            gate_cases.push(ClassificationGateResult {
                fixture: name,
                status_code: spec.status_code,
                raw_bytes,
                classification: cls_name.to_string(),
                expected_classification: spec.expected_response_classification,
                matched,
            });
            continue;
        };

        let page_url = Url::parse(effective_url)
            .map_err(|e| format!("{name}: invalid URL {effective_url}: {e}"))?;
        let warmup = extract_document_parsed(
            &body_str,
            None,
            &page_url,
            spec.content_type.as_deref(),
            &opts,
        )
        .map_err(|e| format!("{name}: warmup extract failed: {e}"))?;

        if category == "passthrough" && warmup.content != body_str {
            return Err(format!("{name}: passthrough mismatch"));
        }

        let extracted_bytes = warmup.content.len();
        let resource_count = warmup.resources.len();

        let start = Instant::now();
        for _ in 0..config.iterations {
            let doc = extract_document_parsed(
                &body_str,
                None,
                &page_url,
                spec.content_type.as_deref(),
                &opts,
            )
            .map_err(|e| format!("{name}: timed extract failed: {e}"))?;
            black_box(doc.content.len());
        }
        let mean_us = start.elapsed().as_secs_f64() * 1_000_000.0 / config.iterations as f64;

        fixtures.push(FixtureResult {
            fixture: name,
            category: category.to_string(),
            content_type: spec
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            raw_bytes,
            extracted_bytes,
            reduction_percent: reduction_pct(raw_bytes, extracted_bytes),
            raw_est_tokens: raw_bytes.div_ceil(4),
            extracted_est_tokens: extracted_bytes.div_ceil(4),
            resource_count,
            mean_us,
        });
    }

    let order = [
        "html",
        "feed",
        "transcript",
        "native_markdown",
        "passthrough",
    ];
    let mut categories = Vec::new();
    for cat in order {
        let matching: Vec<&FixtureResult> = fixtures.iter().filter(|r| r.category == cat).collect();
        if !matching.is_empty() {
            categories.push(summarize_category(cat, &matching));
        }
    }

    let all_refs: Vec<&FixtureResult> = fixtures.iter().collect();
    let all_extractable = summarize_category("all_extractable", &all_refs);
    let total_mean_secs = all_extractable.total_mean_ms / 1_000.0;
    let throughput_mib_per_sec = if total_mean_secs > 0.0 {
        (all_extractable.raw_bytes as f64 / (1024.0 * 1024.0)) / total_mean_secs
    } else {
        0.0
    };

    let matched = gate_cases.iter().filter(|g| g.matched).count();
    Ok(BenchmarkReport {
        iterations: config.iterations,
        total_fixtures: paths.len(),
        extractable_fixtures: fixtures.len(),
        token_estimate_formula: "ceil(bytes / 4)",
        throughput_mib_per_sec,
        classification_gate: ClassificationGateSummary {
            count: gate_cases.len(),
            matched,
        },
        categories,
        all_extractable,
        fixtures,
        gate_cases,
    })
}

fn format_category_label(cat: &str) -> &str {
    match cat {
        "html" => "HTML pages",
        "feed" => "Syndication feeds (RSS/Atom/JSON)",
        "transcript" => "Transcripts (WebVTT/SRT)",
        "native_markdown" => "Native Markdown",
        "passthrough" => "Plain/JSON passthrough",
        "all_extractable" => "All extractable fixtures",
        _ => cat,
    }
}

fn print_markdown(report: &BenchmarkReport) {
    println!("# pleb Corpus Extraction Benchmark\n");
    println!(
        "- **Corpus:** {} total fixtures ({} extractable, {} fail-fast/escalation gates)",
        report.total_fixtures, report.extractable_fixtures, report.classification_gate.count
    );
    println!(
        "- **Iterations:** {} warm extraction passes per fixture (`ExtractMode::Clean`, `OutputFormat::Markdown`)",
        report.iterations
    );
    println!(
        "- **Token estimate:** `{}` heuristic (byte-based; exact BPE counts vary by tokenizer)",
        report.token_estimate_formula
    );
    println!(
        "- **Combined extraction throughput:** {:.2} MiB/s across {} bytes ({:.3} ms per corpus pass)\n",
        report.throughput_mib_per_sec,
        report.all_extractable.raw_bytes,
        report.all_extractable.total_mean_ms
    );

    println!("## Category Summary\n");
    println!("| Category | Fixtures | Raw Bytes | Output Bytes | Reduction | Raw Est. Tokens | Output Est. Tokens | Mean Latency/Doc |");
    println!("| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    for c in &report.categories {
        println!(
            "| {} | {} | {} | {} | {:.1}% | {} | {} | {:.1} us |",
            format_category_label(&c.category),
            c.fixtures,
            c.raw_bytes,
            c.extracted_bytes,
            c.reduction_percent,
            c.raw_est_tokens,
            c.extracted_est_tokens,
            c.mean_us
        );
    }
    let all = &report.all_extractable;
    println!(
        "| **{}** | **{}** | **{}** | **{}** | **{:.1}%** | **{}** | **{}** | **{:.1} us** |",
        format_category_label(&all.category),
        all.fixtures,
        all.raw_bytes,
        all.extracted_bytes,
        all.reduction_percent,
        all.raw_est_tokens,
        all.extracted_est_tokens,
        all.mean_us
    );

    println!(
        "\n## Response Classification Gates ({}/{} matched)\n",
        report.classification_gate.matched, report.classification_gate.count
    );
    println!("| Fixture | HTTP Status | Raw Bytes | Classification | Matched |");
    println!("| :--- | ---: | ---: | :--- | :--- |");
    for g in &report.gate_cases {
        println!(
            "| {} | {} | {} | `{}` | {} |",
            g.fixture, g.status_code, g.raw_bytes, g.classification, g.matched
        );
    }

    println!("\n## Per-Fixture Extraction Results\n");
    println!(
        "| Fixture | Category | Raw Bytes | Output Bytes | Reduction | Resources | Mean (us) |"
    );
    println!("| :--- | :--- | ---: | ---: | ---: | ---: | ---: |");
    for f in &report.fixtures {
        println!(
            "| {} | {} | {} | {} | {:.1}% | {} | {:.1} |",
            f.fixture,
            f.category,
            f.raw_bytes,
            f.extracted_bytes,
            f.reduction_percent,
            f.resource_count,
            f.mean_us
        );
    }
}

fn main() {
    let config = match parse_args() {
        Ok(Some(c)) => c,
        Ok(None) => {
            print_usage();
            return;
        }
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    let report = match run_benchmark(&config) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            process::exit(1);
        }
    };

    if config.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("serialize benchmark report")
        );
    } else {
        print_markdown(&report);
    }
}
