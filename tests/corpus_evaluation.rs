use mesh_wick::extract::{
    decode_text_bytes, detect_binary_content, extract_document_parsed, is_html_content_type,
    ExtractMode, ExtractOptions, OutputFormat, ParsedHtml, ResourceKind,
};
use mesh_wick::policy::{classify_response_with_context, ResponseClassification};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use url::Url;

#[derive(Debug, Deserialize)]
struct FixtureSpec {
    tier: String,
    url: String,
    final_url: Option<String>,
    #[serde(default = "default_status")]
    status_code: u16,
    content_type: Option<String>,
    #[serde(default)]
    expected_exit_code: i32,
    expected_error_kind: Option<String>,
    expected_response_classification: Option<String>,
    expected_title: Option<String>,
    expected_markdown_alternate: Option<String>,
    #[serde(default)]
    exact_passthrough: bool,
    #[serde(default)]
    variants: Vec<VariantSpec>,
}

#[derive(Debug, Deserialize)]
struct VariantSpec {
    name: String,
    #[serde(default = "default_format")]
    format: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_true")]
    include_images: bool,
    #[serde(default = "default_true")]
    include_links: bool,
    max_bytes: Option<usize>,
    expected_title: Option<String>,
    #[serde(default)]
    exact_passthrough: bool,
    #[serde(default)]
    require_balanced_fences: bool,
    #[serde(default)]
    expected_substrings: Vec<String>,
    #[serde(default)]
    expected_code_blocks: Vec<ExpectedCodeBlock>,
    #[serde(default)]
    expected_links: Vec<String>,
    #[serde(default)]
    expected_images: Vec<String>,
    #[serde(default)]
    forbidden_substrings: Vec<String>,
    #[serde(default)]
    expected_resource_urls: Vec<String>,
    #[serde(default)]
    expected_resource_kinds: Vec<String>,
    #[serde(default)]
    forbidden_resource_urls: Vec<String>,
    minimum_output_bytes: Option<usize>,
    maximum_output_bytes: Option<usize>,
    expected_structure: Option<ExpectedStructure>,
}

#[derive(Debug, Deserialize)]
struct ExpectedCodeBlock {
    language: Option<String>,
    snippet: String,
}

#[derive(Debug, Default, Deserialize)]
struct ExpectedStructure {
    #[serde(default)]
    minimum_headings: usize,
    #[serde(default)]
    minimum_code_blocks: usize,
    #[serde(default)]
    minimum_tables: usize,
    #[serde(default)]
    minimum_list_items: usize,
    #[serde(default)]
    minimum_links: usize,
}

fn resource_kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Diagram => "diagram",
        ResourceKind::Image => "image",
        ResourceKind::Transcript => "transcript",
        ResourceKind::Document => "document",
        ResourceKind::Dataset => "dataset",
        ResourceKind::Schema => "schema",
    }
}

fn default_status() -> u16 {
    200
}
fn default_format() -> String {
    "markdown".to_string()
}
fn default_mode() -> String {
    "clean".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Default)]
struct FixtureOutcome {
    passed: bool,
    cls_exact: bool,
    expected_hits: usize,
    expected_total: usize,
    forbidden_clean: usize,
    forbidden_total: usize,
    md_density: Option<f64>,
    failures: Vec<String>,
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_synthesized_title<'a>(md: &'a str, title: Option<&str>) -> &'a str {
    let Some(t) = title else { return md };
    let mut lines = md.split_inclusive('\n');
    if let Some(first) = lines.next() {
        if let Some(h) = first.trim().strip_prefix("# ") {
            if h.trim().eq_ignore_ascii_case(t.trim()) {
                return &md[first.len()..];
            }
        }
    }
    md
}

fn parse_fenced_blocks(md: &str) -> Vec<(Option<String>, String)> {
    let mut blocks = Vec::new();
    let mut open: Option<(char, usize, Option<String>, Vec<String>)> = None;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some((fc, fl, lang, mut body)) = open.take() {
            let t = line.trim();
            if t.len() >= fl && t.chars().all(|c| c == fc) {
                blocks.push((lang, body.join("\n")));
            } else {
                body.push(line.to_string());
                open = Some((fc, fl, lang, body));
            }
        } else if let Some(fc) = trimmed.chars().next().filter(|&c| c == '`' || c == '~') {
            let fl = trimmed.chars().take_while(|&c| c == fc).count();
            if fl >= 3 {
                let info = trimmed[fl..].split_whitespace().next();
                let lang = info
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_ascii_lowercase());
                open = Some((fc, fl, lang, Vec::new()));
            }
        }
    }
    blocks
}

fn has_unclosed_fence(md: &str) -> bool {
    let mut open: Option<(char, usize)> = None;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some((fc, fl)) = open {
            let t = line.trim();
            if t.len() >= fl && t.chars().all(|c| c == fc) {
                open = None;
            }
        } else if let Some(fc) = trimmed.chars().next().filter(|&c| c == '`' || c == '~') {
            let fl = trimmed.chars().take_while(|&c| c == fc).count();
            if fl >= 3 {
                open = Some((fc, fl));
            }
        }
    }
    open.is_some()
}

fn parse_markdown_destinations(md: &str) -> (Vec<String>, Vec<String>) {
    let (mut links, mut images) = (Vec::new(), Vec::new());
    let mut in_fence: Option<(char, usize)> = None;
    for line in md.lines() {
        let trimmed = line.trim_start();
        if let Some((fc, fl)) = in_fence {
            let t = line.trim();
            if t.len() >= fl && t.chars().all(|c| c == fc) {
                in_fence = None;
            }
            continue;
        }
        if let Some(fc) = trimmed.chars().next().filter(|&c| c == '`' || c == '~') {
            let fl = trimmed.chars().take_while(|&c| c == fc).count();
            if fl >= 3 {
                in_fence = Some((fc, fl));
                continue;
            }
        }
        scan_line_destinations(line, &mut links, &mut images);
    }
    (links, images)
}

fn scan_line_destinations(line: &str, links: &mut Vec<String>, images: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            i += ticks;
            while i + ticks <= bytes.len() && !bytes[i..i + ticks].iter().all(|&b| b == b'`') {
                i += 1;
            }
            i = (i + ticks).min(bytes.len());
            continue;
        }
        let is_img = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        if is_img || bytes[i] == b'[' {
            let start = if is_img { i + 2 } else { i + 1 };
            let mut depth = 1usize;
            let mut j = start;
            while j < bytes.len() && depth > 0 {
                if bytes[j] == b'\\' && j + 1 < bytes.len() {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'[' {
                    depth += 1;
                } else if bytes[j] == b']' {
                    depth -= 1;
                }
                j += 1;
            }
            if depth == 0 && j < bytes.len() && bytes[j] == b'(' {
                if let Some((dest, next_i)) = parse_paren_dest(line, j + 1) {
                    if is_img {
                        images.push(dest);
                    } else {
                        links.push(dest);
                    }
                    i = next_i;
                    continue;
                }
            }
        }
        i += 1;
    }
}

fn parse_paren_dest(line: &str, start: usize) -> Option<(String, usize)> {
    let rest = &line[start..];
    let trimmed = rest.trim_start();
    let lead = rest.len() - trimmed.len();
    if let Some(after_lt) = trimmed.strip_prefix('<') {
        let end_gt = after_lt.find('>')?;
        let dest = after_lt[..end_gt].to_string();
        let after = &after_lt[end_gt + 1..];
        let close = after.find(')')?;
        return Some((dest, start + lead + 1 + end_gt + 1 + close + 1));
    }
    let bytes = trimmed.as_bytes();
    let (mut depth, mut idx, mut dest_end) = (0usize, 0usize, None);
    while idx < bytes.len() {
        if bytes[idx] == b'\\' && idx + 1 < bytes.len() {
            idx += 2;
            continue;
        }
        match bytes[idx] {
            b'(' => depth += 1,
            b')' if depth == 0 => {
                let end = dest_end.unwrap_or(idx);
                let raw = trimmed[..end].replace("\\(", "(").replace("\\)", ")");
                return Some((raw, start + lead + idx + 1));
            }
            b')' => depth -= 1,
            b' ' | b'\t' if depth == 0 && dest_end.is_none() => dest_end = Some(idx),
            _ => {}
        }
        idx += 1;
    }
    None
}

fn count_structure(md: &str, links_count: usize, code_blocks: usize) -> [usize; 5] {
    let (mut headings, mut tables, mut list_items) = (0, 0, 0);
    let mut in_fence: Option<(char, usize)> = None;
    let mut prev_table_row = false;
    let mut in_table = false;
    for line in md.lines() {
        let t = line.trim();
        if let Some((fc, fl)) = in_fence {
            if t.len() >= fl && t.chars().all(|c| c == fc) {
                in_fence = None;
            }
            continue;
        }
        if let Some(fc) = t.chars().next().filter(|&c| c == '`' || c == '~') {
            let fl = t.chars().take_while(|&c| c == fc).count();
            if fl >= 3 {
                in_fence = Some((fc, fl));
                prev_table_row = false;
                in_table = false;
                continue;
            }
        }
        let hashes = t.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            headings += 1;
        }
        if t.starts_with("- ")
            || t.starts_with("* ")
            || t.starts_with("+ ")
            || t.split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        {
            list_items += 1;
        }
        if t.starts_with('|') && t.ends_with('|') && t.len() >= 2 {
            let is_sep = t[1..t.len() - 1]
                .split('|')
                .all(|c| !c.trim().is_empty() && c.trim().chars().all(|ch| ch == '-' || ch == ':'));
            if prev_table_row && is_sep && !in_table {
                tables += 1;
                in_table = true;
            }
            prev_table_row = true;
        } else {
            prev_table_row = false;
            in_table = false;
        }
    }
    [headings, code_blocks, tables, list_items, links_count]
}

fn evaluate_fixture(path: &Path) -> (String, FixtureSpec, FixtureOutcome) {
    let raw = fs::read(path).expect("read fixture");
    let nl = raw
        .iter()
        .position(|&b| b == b'\n')
        .expect("header newline");
    let header = std::str::from_utf8(&raw[..nl]).expect("utf8 header").trim();
    let json = header
        .strip_prefix("<!-- wick-fixture:")
        .and_then(|s| s.strip_suffix("-->"))
        .expect("wick-fixture frontmatter")
        .trim();
    let spec: FixtureSpec = serde_json::from_str(json).expect("valid fixture json");
    let body_bytes = &raw[nl + 1..];
    let body_str = decode_text_bytes(body_bytes, spec.content_type.as_deref());

    let mut out = FixtureOutcome::default();
    let is_html = is_html_content_type(spec.content_type.as_deref());
    let mut parsed = None;
    let cls = classify_response_with_context(
        spec.status_code,
        is_html,
        Some(&spec.url),
        Some(spec.final_url.as_deref().unwrap_or(&spec.url)),
        &body_str,
        &mut parsed,
    );
    let (cls_name, mut actual_exit, mut actual_kind) = match &cls {
        ResponseClassification::Ok => ("ok", 0, None),
        ResponseClassification::Blocked { .. } => ("blocked", 2, Some("blocked")),
        ResponseClassification::Challenge { .. } => ("challenge", 2, Some("challenge")),
        ResponseClassification::JavaScriptShell { .. } => ("javascript_shell", 0, None),
        ResponseClassification::HttpError { .. } => ("http_error", 3, Some("http_error")),
    };
    if actual_exit == 0 && detect_binary_content(spec.content_type.as_deref(), body_bytes).is_some()
    {
        actual_exit = 5;
        actual_kind = Some("unsupported_content_type");
    }

    let mut cls_ok = actual_exit == spec.expected_exit_code;
    if let Some(exp_cls) = &spec.expected_response_classification {
        if cls_name != exp_cls {
            cls_ok = false;
            out.failures
                .push(format!("cls: got {cls_name}, want {exp_cls}"));
        }
    }
    if let Some(exp_kind) = &spec.expected_error_kind {
        if actual_kind != Some(exp_kind.as_str()) {
            cls_ok = false;
            out.failures
                .push(format!("kind: got {actual_kind:?}, want {exp_kind}"));
        }
    }
    if actual_exit != spec.expected_exit_code {
        out.failures.push(format!(
            "exit: got {actual_exit}, want {}",
            spec.expected_exit_code
        ));
    }
    out.cls_exact = cls_ok;

    let page_url = Url::parse(spec.final_url.as_deref().unwrap_or(&spec.url)).expect("valid url");
    if let Some(exp_alt) = &spec.expected_markdown_alternate {
        let parsed_ref = parsed.get_or_insert_with(|| ParsedHtml::parse(&body_str));
        let got = parsed_ref.markdown_alternate_url(&page_url);
        out.expected_total += 1;
        if got.as_ref().map(Url::as_str) == Some(exp_alt.as_str()) {
            out.expected_hits += 1;
        } else {
            out.failures.push(format!(
                "markdown_alternate: got {:?}, want {:?}",
                got.as_ref().map(Url::as_str),
                exp_alt
            ));
        }
    }
    let mut densities = Vec::new();
    for v in &spec.variants {
        let format = match v.format.as_str() {
            "text" => OutputFormat::Text,
            "html" => OutputFormat::Html,
            _ => OutputFormat::Markdown,
        };
        let mode = match v.mode.as_str() {
            "article" => ExtractMode::Article,
            "full" => ExtractMode::Full,
            _ => ExtractMode::Clean,
        };
        let opts = ExtractOptions {
            format,
            mode,
            include_images: v.include_images,
            include_links: v.include_links,
            max_bytes: v.max_bytes,
        };
        let doc = extract_document_parsed(
            &body_str,
            None,
            &page_url,
            spec.content_type.as_deref(),
            &opts,
        )
        .expect("extract_document_parsed");
        let want_title = v.expected_title.as_deref().or(if v.exact_passthrough {
            None
        } else {
            spec.expected_title.as_deref()
        });
        if let Some(exp_title) = want_title {
            if doc.title.as_deref() != Some(exp_title) {
                out.failures.push(format!(
                    "{}: title {:?} != {exp_title:?}",
                    v.name, doc.title
                ));
            }
        }
        if (spec.exact_passthrough || v.exact_passthrough) && doc.content != body_str {
            out.failures
                .push(format!("{}: passthrough mismatch", v.name));
        }
        if v.require_balanced_fences && has_unclosed_fence(&doc.content) {
            out.failures
                .push(format!("{}: unclosed code fence", v.name));
        }
        if let Some(min_b) = v.minimum_output_bytes {
            if doc.content.len() < min_b {
                out.failures.push(format!(
                    "{}: len {} < min {min_b}",
                    v.name,
                    doc.content.len()
                ));
            }
        }
        if let Some(max_b) = v.maximum_output_bytes {
            if doc.content.len() > max_b {
                out.failures.push(format!(
                    "{}: len {} > max {max_b}",
                    v.name,
                    doc.content.len()
                ));
            }
        }

        let body_for_sub = if format == OutputFormat::Markdown {
            strip_synthesized_title(&doc.content, doc.title.as_deref())
        } else {
            &doc.content
        };
        let norm_body = normalize_ws(body_for_sub);
        let (mut cursor, mut matched_bytes) = (0usize, 0usize);
        for exp in &v.expected_substrings {
            out.expected_total += 1;
            let needle = normalize_ws(exp);
            if let Some(pos) = norm_body[cursor..].find(&needle) {
                cursor += pos + needle.len();
                matched_bytes += needle.len();
                out.expected_hits += 1;
            } else {
                out.failures
                    .push(format!("{}: missing ordered substring {:?}", v.name, exp));
            }
        }

        let blocks = parse_fenced_blocks(&doc.content);
        for cb in &v.expected_code_blocks {
            out.expected_total += 1;
            let hit = blocks.iter().any(|(lang, body)| {
                let lang_ok = match &cb.language {
                    Some(l) => lang.as_deref().is_some_and(|g| g.eq_ignore_ascii_case(l)),
                    None => true,
                };
                lang_ok && body.contains(&cb.snippet)
            });
            if hit {
                out.expected_hits += 1;
                matched_bytes += cb.snippet.len();
            } else {
                out.failures
                    .push(format!("{}: missing code block {:?}", v.name, cb.snippet));
            }
        }

        let (links, images) = parse_markdown_destinations(&doc.content);
        for link in &v.expected_links {
            out.expected_total += 1;
            if links.iter().any(|l| l == link) {
                out.expected_hits += 1;
            } else {
                out.failures
                    .push(format!("{}: missing link {link} (found {links:?})", v.name));
            }
        }
        for img in &v.expected_images {
            out.expected_total += 1;
            if images.iter().any(|im| im == img) {
                out.expected_hits += 1;
            } else {
                out.failures.push(format!(
                    "{}: missing image {img} (found {images:?})",
                    v.name
                ));
            }
        }

        let norm_full_lower = normalize_ws(&doc.content).to_ascii_lowercase();
        for forb in &v.forbidden_substrings {
            out.forbidden_total += 1;
            let needle = normalize_ws(forb).to_ascii_lowercase();
            if norm_full_lower.contains(&needle) {
                out.failures
                    .push(format!("{}: forbidden substring {:?}", v.name, forb));
            } else {
                out.forbidden_clean += 1;
            }
        }

        let res_urls: Vec<&str> = doc.resources.iter().map(|r| r.url.as_str()).collect();
        let res_kinds: Vec<&str> = doc
            .resources
            .iter()
            .map(|r| resource_kind_name(r.kind))
            .collect();
        for url in &v.expected_resource_urls {
            out.expected_total += 1;
            if res_urls.iter().any(|&u| u == url) {
                out.expected_hits += 1;
            } else {
                out.failures.push(format!(
                    "{}: missing resource url {url} (found {res_urls:?})",
                    v.name
                ));
            }
        }
        for kind in &v.expected_resource_kinds {
            out.expected_total += 1;
            if res_kinds.iter().any(|&k| k == kind) {
                out.expected_hits += 1;
            } else {
                out.failures.push(format!(
                    "{}: missing resource kind {kind} (found {res_kinds:?})",
                    v.name
                ));
            }
        }
        for forb_url in &v.forbidden_resource_urls {
            out.forbidden_total += 1;
            if res_urls.iter().any(|&u| u == forb_url) {
                out.failures
                    .push(format!("{}: forbidden resource url {forb_url}", v.name));
            } else {
                out.forbidden_clean += 1;
            }
        }

        if let Some(st) = &v.expected_structure {
            let got = count_structure(&doc.content, links.len(), blocks.len());
            let want = [
                st.minimum_headings,
                st.minimum_code_blocks,
                st.minimum_tables,
                st.minimum_list_items,
                st.minimum_links,
            ];
            for (label, g, w) in ["headings", "code_blocks", "tables", "list_items", "links"]
                .into_iter()
                .zip(got)
                .zip(want)
                .map(|((l, g), w)| (l, g, w))
            {
                if g < w {
                    out.failures.push(format!("{}: {label} {g} < {w}", v.name));
                }
            }
        }

        if is_html && format == OutputFormat::Markdown {
            let denom = doc
                .content
                .len()
                .max(v.minimum_output_bytes.unwrap_or(1))
                .max(1) as f64;
            densities.push((matched_bytes as f64 / denom).min(1.0));
        }
    }

    if !densities.is_empty() {
        out.md_density = Some(densities.iter().sum::<f64>() / densities.len() as f64);
    }
    out.passed = out.failures.is_empty();
    let id = path.file_stem().unwrap().to_string_lossy().to_string();
    (id, spec, out)
}

#[test]
fn corpus_evaluation_scorecard() {
    let corpus_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut paths: Vec<PathBuf> = fs::read_dir(&corpus_dir)
        .expect("read tests/corpus")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "html"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "tests/corpus must contain fixtures");

    let (mut ratchet_pass, mut ratchet_total) = (0usize, 0usize);
    let (mut f_cls_sum, mut f_rec_sum, mut f_sup_sum) = (0.0f64, 0.0f64, 0.0f64);
    let (mut f_cls_n, mut f_rec_n, mut f_sup_n) = (0usize, 0usize, 0usize);
    let mut densities = Vec::new();
    let mut ratchet_errors = Vec::new();

    // Inline binary magic-byte ratchet cases (exit code 5 / unsupported_content_type).
    let binary_cases: &[(&str, Option<&str>, &[u8])] = &[
        (
            "binary_pdf_magic",
            None,
            b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n",
        ),
        (
            "binary_png_magic",
            None,
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        ),
        (
            "binary_nul_stream",
            Some("application/octet-stream"),
            b"payload\x00with\x00nuls",
        ),
    ];
    for (name, ct, bytes) in binary_cases {
        ratchet_total += 1;
        if detect_binary_content(*ct, bytes).is_some() {
            ratchet_pass += 1;
        } else {
            ratchet_errors.push(format!("{name}: binary magic not detected"));
        }
    }

    for path in &paths {
        let (id, spec, out) = evaluate_fixture(path);
        if let Some(d) = out.md_density {
            densities.push(d);
        }
        if spec.tier == "ratchet" {
            ratchet_total += 1;
            if out.passed {
                ratchet_pass += 1;
            } else {
                ratchet_errors.push(format!("{id}: {}", out.failures.join("; ")));
            }
        } else {
            f_cls_n += 1;
            if out.cls_exact {
                f_cls_sum += 1.0;
            }
            if out.expected_total > 0 {
                f_rec_n += 1;
                f_rec_sum += out.expected_hits as f64 / out.expected_total as f64;
            }
            if out.forbidden_total > 0 {
                f_sup_n += 1;
                f_sup_sum += out.forbidden_clean as f64 / out.forbidden_total as f64;
            }
            if !out.passed {
                eprintln!("[FRONTIER] {id}: {}", out.failures.join(" | "));
            }
        }
    }

    let ratchet_rate = ratchet_pass as f64 / ratchet_total.max(1) as f64;
    let f_cls = if f_cls_n > 0 {
        f_cls_sum / f_cls_n as f64
    } else {
        1.0
    };
    let f_rec = if f_rec_n > 0 {
        f_rec_sum / f_rec_n as f64
    } else {
        1.0
    };
    let f_sup = if f_sup_n > 0 {
        f_sup_sum / f_sup_n as f64
    } else {
        1.0
    };
    let density = if densities.is_empty() {
        0.0
    } else {
        densities.iter().sum::<f64>() / densities.len() as f64
    };
    println!(
        "[SCORECARD] ratchet={ratchet_rate:.3} frontier_cls={f_cls:.3} frontier_recall={f_rec:.3} frontier_suppress={f_sup:.3} signal_density={density:.3}"
    );
    assert!(
        ratchet_errors.is_empty() && (ratchet_rate - 1.0).abs() < f64::EPSILON,
        "Ratchet regression ({ratchet_pass}/{ratchet_total}):\n{}",
        ratchet_errors.join("\n")
    );
}
