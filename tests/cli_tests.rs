use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/wick-t-{}-{}", std::process::id(), seq));
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TempDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> TempDir {
    TempDir::new()
}

async fn spawn_cli_test_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");

                let route = path.split('?').next().unwrap_or(path);
                let (status, ct, body): (&str, &str, &[u8]) = match route {
                    "/ok" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>CLI Test</title></head><body><main><p>Hello CLI.</p></main></body></html>",
                    ),
                    "/blocked" => ("403 Forbidden", "text/html", b"Forbidden"),
                    "/missing" => ("404 Not Found", "text/plain", b"Not Found"),
                    "/file.pdf" => ("200 OK", "application/pdf", b"%PDF-1.7\nbinary"),
                    "/diagram.png" => (
                        "200 OK",
                        "image/png",
                        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
                    ),
                    "/fake.png" => (
                        "200 OK",
                        "image/png",
                        b"<!DOCTYPE html><html><body>soft 404</body></html>",
                    ),
                    _ => ("404 Not Found", "text/plain", b"unknown"),
                };
                let hdr = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(hdr.as_bytes()).await;
                let _ = socket.write_all(body).await;
            });
        }
    });

    format!("http://{}", addr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_supports_value_enums_output_file_and_uniform_json_errors() {
    let base = spawn_cli_test_server().await;
    let state_dir = temp_dir();
    let out_dir = temp_dir();
    let wick_bin = env!("CARGO_BIN_EXE_wick");

    // 1. status --json emits structured JSON.
    let status_out = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "status"])
        .output()
        .await
        .unwrap();
    assert_eq!(status_out.status.code(), Some(0));
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status_json["cef_renderer"]["expected_protocol"], "v5-meta");

    // 2. daemon status --json and session clear --json work cleanly.
    let daemon_out = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["daemon", "status", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(daemon_out.status.code(), Some(0));
    let daemon_json: serde_json::Value = serde_json::from_slice(&daemon_out.stdout).unwrap();
    assert_eq!(daemon_json["running"], false);

    let session_out = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["session", "clear", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(session_out.status.code(), Some(0));
    let session_json: serde_json::Value = serde_json::from_slice(&session_out.stdout).unwrap();
    assert_eq!(session_json["cleared"], true);

    // 3. ValueEnum aliases/case-insensitivity and --json --metadata-only.
    let fetch_ok = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "fetch",
            "--format",
            "MD",
            "--extract",
            "Clean",
            "--render",
            "no-js",
            "--source",
            "None",
            "--json",
            "--metadata-only",
            &format!("{base}/ok"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        fetch_ok.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&fetch_ok.stderr)
    );
    let meta_json: serde_json::Value = serde_json::from_slice(&fetch_ok.stdout).unwrap();
    assert_eq!(meta_json["status_code"], 200);
    assert!(meta_json.get("content").is_none());
    assert!(meta_json["content_bytes"].as_u64().unwrap() > 0);

    // 4. --format raw --output writes exact bytes without touching stdout.
    let raw_path = out_dir.path().join("doc.pdf");
    let raw_out = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "fetch",
            "--format",
            "raw",
            "--output",
            raw_path.to_str().unwrap(),
            &format!("{base}/file.pdf"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        raw_out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&raw_out.stderr)
    );
    assert!(raw_out.stdout.is_empty());
    assert_eq!(fs::read(&raw_path).unwrap(), b"%PDF-1.7\nbinary");

    // 5. Failed fetch with --output must NOT create an empty file, and --json writes error JSON to stderr.
    let fail_path = out_dir.path().join("should-not-exist.md");
    let fail_404 = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "fetch",
            "--render",
            "cronet",
            "--output",
            fail_path.to_str().unwrap(),
            &format!("{base}/missing"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_404.status.code(), Some(3));
    assert!(!fail_path.exists());
    let err_404_json: serde_json::Value = serde_json::from_slice(&fail_404.stderr).unwrap();
    assert_eq!(err_404_json["kind"], "http_error");
    assert_eq!(err_404_json["exit_code"], 3);

    // 6. 403 -> exit 2, PDF in non-raw -> exit 5, raw+json conflict -> exit 1.
    let fail_403 = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/blocked"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_403.status.code(), Some(2));
    let err_403_json: serde_json::Value = serde_json::from_slice(&fail_403.stderr).unwrap();
    assert_eq!(err_403_json["kind"], "blocked");

    let fail_pdf = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/file.pdf"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_pdf.status.code(), Some(5));
    let err_pdf_json: serde_json::Value = serde_json::from_slice(&fail_pdf.stderr).unwrap();
    assert_eq!(err_pdf_json["kind"], "unsupported_content_type");

    let fail_raw_json = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "fetch",
            "--format",
            "raw",
            &format!("{base}/file.pdf"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_raw_json.status.code(), Some(1));
    let err_raw_json: serde_json::Value = serde_json::from_slice(&fail_raw_json.stderr).unwrap();
    assert_eq!(err_raw_json["kind"], "invalid_input");

    // 7. search error without BRAVE_API_KEY respects global --json envelope.
    let fail_search = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env_remove("BRAVE_API_KEY")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["search", "--provider", "brave", "--json", "rust"])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_search.status.code(), Some(1));
    let err_search_json: serde_json::Value = serde_json::from_slice(&fail_search.stderr).unwrap();
    assert_eq!(err_search_json["kind"], "search_error");

    let _ = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["daemon", "stop"])
        .output()
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_batch_fetch_modes_stdin_and_flag_conflicts() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let base = spawn_cli_test_server().await;
    let state_dir = temp_dir();
    let wick_bin = env!("CARGO_BIN_EXE_wick");

    // 1. Multi-URL --json returns ordered JSON array.
    let batch_json = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "fetch",
            "--render",
            "cronet",
            "--concurrency",
            "2",
            &format!("{base}/ok"),
            &format!("{base}/ok?v=2"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(batch_json.status.code(), Some(0));
    let arr: serde_json::Value = serde_json::from_slice(&batch_json.stdout).unwrap();
    let items = arr.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["url"], format!("{base}/ok"));
    assert_eq!(items[1]["url"], format!("{base}/ok?v=2"));

    // 2. Multi-URL --jsonl --metadata-only streams one line per URL without content.
    let batch_jsonl = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "fetch",
            "--render",
            "cronet",
            "--jsonl",
            "--metadata-only",
            &format!("{base}/ok"),
            &format!("{base}/missing"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(batch_jsonl.status.code(), Some(4));
    let stdout_str = String::from_utf8_lossy(&batch_jsonl.stdout);
    let lines: Vec<serde_json::Value> = stdout_str
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(lines
        .iter()
        .any(|v| v["ok"] == true && v.get("content").is_none()));
    assert!(lines
        .iter()
        .any(|v| v["ok"] == false && v["exit_code"] == 3));

    // 3. Multi-URL default markdown prints numbered sections.
    let batch_md = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/ok"),
            &format!("{base}/ok?n=2"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(batch_md.status.code(), Some(0));
    let md_out = String::from_utf8_lossy(&batch_md.stdout);
    assert!(md_out.contains("## [1/2]"));
    assert!(md_out.contains("## [2/2]"));

    // 4. Stdin '-' with 1 URL still emits a 1-element JSON array.
    let mut child = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "fetch", "--render", "cronet", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("\n# comment\n{base}/ok\n\n").as_bytes())
        .await
        .unwrap();
    let stdin_out = child.wait_with_output().await.unwrap();
    assert_eq!(stdin_out.status.code(), Some(0));
    let stdin_arr: serde_json::Value = serde_json::from_slice(&stdin_out.stdout).unwrap();
    assert_eq!(stdin_arr.as_array().unwrap().len(), 1);

    // 5. Empty stdin -> exit 1 with JSON error envelope.
    let mut empty_child = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "fetch", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    empty_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"# only comment\n\n")
        .await
        .unwrap();
    let empty_out = empty_child.wait_with_output().await.unwrap();
    assert_eq!(empty_out.status.code(), Some(1));
    let empty_err: serde_json::Value = serde_json::from_slice(&empty_out.stderr).unwrap();
    assert_eq!(empty_err["kind"], "invalid_input");

    // 6. Uniform all-fail -> exit 3; heterogeneous all-fail -> exit 1.
    let uniform_fail = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/missing"),
            &format!("{base}/missing2"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(uniform_fail.status.code(), Some(3));

    let hetero_fail = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "fetch",
            "--render",
            "cronet",
            &format!("{base}/blocked"),
            &format!("{base}/missing"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(hetero_fail.status.code(), Some(1));

    // 7. Flag conflicts return exit 1.
    for args in [
        vec![
            "fetch",
            "--format",
            "raw",
            &format!("{base}/ok"),
            &format!("{base}/ok"),
        ],
        vec![
            "fetch",
            "--output",
            "/tmp/unused.md",
            &format!("{base}/ok"),
            &format!("{base}/ok"),
        ],
        vec!["fetch", "--json", "--jsonl", &format!("{base}/ok")],
        vec!["fetch", "--metadata-only", &format!("{base}/ok")],
        vec!["search", "--urls-only", "--json", "rust"],
    ] {
        let out = Command::new(wick_bin)
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
    }

    let _ = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["daemon", "stop"])
        .output()
        .await;
}

#[tokio::test]
async fn adversarial_status_json_defaults_and_readme_contract_parity() {
    let wick_bin = env!("CARGO_BIN_EXE_wick");
    let state_dir = TempDir::new();

    // `wick status --json` must emit clean default daemon fields when no daemon is running.
    let out = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(state_dir.path())
        .args(["status", "--json"])
        .env("WICK_NO_HOME_CEF", "1")
        .env("WICK_NO_KEYCHAIN", "1")
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "status --json failed");
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["daemon"]["running"], false);
    assert_eq!(val["daemon"]["active_requests"], 0);
    assert_eq!(val["cef_renderer"]["expected_protocol"], "v5-meta");

    // README.md and README-FULL.md must maintain canonical branding, valid relative file links,
    // valid heading anchors, and CLI/schema contract parity without retired identifiers.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(manifest_dir.join("README.md")).unwrap();
    let readme_full = std::fs::read_to_string(manifest_dir.join("README-FULL.md")).unwrap();
    let license = std::fs::read_to_string(manifest_dir.join("LICENSE")).unwrap();

    for (doc_name, doc_text) in [("README.md", &readme), ("README-FULL.md", &readme_full)] {
        assert!(
            doc_text.contains("MeSH-wick"),
            "{doc_name} missing canonical MeSH-wick stylization"
        );
        assert!(
            doc_text.contains("Memory Sync Hub LLC"),
            "{doc_name} missing Memory Sync Hub LLC attribution"
        );
    }
    assert!(
        license.contains("Memory Sync Hub LLC"),
        "LICENSE missing Memory Sync Hub LLC copyright line"
    );
    let cargo_toml = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap();
    for required_meta in [
        "authors = [\"Memory Sync Hub LLC\"]",
        "readme = \"README.md\"",
        "repository = \"https://github.com/MeSH-Inc/mesh-wick\"",
        "homepage = \"https://github.com/MeSH-Inc/mesh-wick\"",
    ] {
        assert!(
            cargo_toml.contains(required_meta),
            "Cargo.toml missing expected metadata: {required_meta}"
        );
    }

    fn extract_markdown_anchors(md: &str) -> std::collections::HashSet<String> {
        let mut anchors = std::collections::HashSet::new();
        let mut fence_len: Option<usize> = None;
        for line in md.lines() {
            let trimmed = line.trim_start();
            let ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
            if ticks >= 3 {
                match fence_len {
                    None => {
                        fence_len = Some(ticks);
                        continue;
                    }
                    Some(open) if ticks >= open && trimmed[ticks..].trim().is_empty() => {
                        fence_len = None;
                        continue;
                    }
                    Some(_) => continue,
                }
            }
            if fence_len.is_some() {
                continue;
            }
            let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
            if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
                let heading = trimmed[hashes + 1..].trim();
                let slug: String = heading
                    .to_ascii_lowercase()
                    .chars()
                    .filter_map(|c| match c {
                        'a'..='z' | '0'..='9' | '-' | '_' => Some(c),
                        ' ' => Some('-'),
                        _ => None,
                    })
                    .collect();
                anchors.insert(slug);
            }
        }
        anchors
    }

    fn extract_markdown_link_targets(md: &str) -> Vec<String> {
        let mut targets = Vec::new();
        let mut fence_len: Option<usize> = None;
        for line in md.lines() {
            let trimmed = line.trim_start();
            let ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
            if ticks >= 3 {
                match fence_len {
                    None => {
                        fence_len = Some(ticks);
                        continue;
                    }
                    Some(open) if ticks >= open && trimmed[ticks..].trim().is_empty() => {
                        fence_len = None;
                        continue;
                    }
                    Some(_) => continue,
                }
            }
            if fence_len.is_some() {
                continue;
            }
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b']' && bytes.get(i + 1) == Some(&b'(') {
                    let start = i + 2;
                    let mut depth = 1usize;
                    let mut j = start;
                    while j < bytes.len() && depth > 0 {
                        match bytes[j] {
                            b'(' => depth += 1,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        if depth > 0 {
                            j += 1;
                        }
                    }
                    if depth == 0 && j > start {
                        let target = line[start..j]
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .trim_matches(|c| c == '<' || c == '>');
                        if !target.is_empty() {
                            targets.push(target.to_string());
                        }
                    }
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
        }
        targets
    }

    let readme_anchors = extract_markdown_anchors(&readme);
    let full_anchors = extract_markdown_anchors(&readme_full);
    let mut readme_full_anchor_links = 0usize;
    let mut full_internal_anchor_links = 0usize;
    let mut relative_file_links = 0usize;

    for (doc_name, doc_text, self_anchors) in [
        ("README.md", &readme, &readme_anchors),
        ("README-FULL.md", &readme_full, &full_anchors),
    ] {
        for target in extract_markdown_link_targets(doc_text) {
            if target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            if let Some(anchor) = target.strip_prefix('#') {
                assert!(
                    self_anchors.contains(anchor),
                    "{doc_name} links to missing internal anchor: #{anchor}"
                );
                if doc_name == "README-FULL.md" {
                    full_internal_anchor_links += 1;
                }
                continue;
            }
            let (rel_path, opt_anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            assert!(
                manifest_dir.join(rel_path).exists(),
                "{doc_name} links to missing relative path: {rel_path}"
            );
            relative_file_links += 1;
            if let Some(anchor) = opt_anchor {
                let target_anchors = if rel_path == "README-FULL.md" {
                    readme_full_anchor_links += 1;
                    &full_anchors
                } else if rel_path == "README.md" {
                    &readme_anchors
                } else {
                    continue;
                };
                assert!(
                    target_anchors.contains(anchor),
                    "{doc_name} links to missing anchor {rel_path}#{anchor}"
                );
            }
        }
    }
    assert!(
        readme_full_anchor_links >= 10,
        "README.md should link to README-FULL.md sections (found {readme_full_anchor_links})"
    );
    assert!(
        full_internal_anchor_links >= 10,
        "README-FULL.md Contents should link to internal sections (found {full_internal_anchor_links})"
    );
    assert!(
        relative_file_links >= 8,
        "README docs should contain verified relative file links (found {relative_file_links})"
    );

    // README-FULL.md must stay in sync with v5-meta, env overrides, batch/search flags, and JSON schemas.
    for required in [
        "v5-meta",
        "WICK_CEF_RENDERER",
        "WICK_NO_HOME_CEF",
        "WICK_DAEMON_EXECUTABLE",
        "--jsonl",
        "--urls-only",
        "daemon.sock",
        "--source",
        "raw_source",
        "markdown_mirror",
        "markdown_negotiated",
        "RSS 2.0",
        "--resources",
        "--stage-resources",
        "--referer",
        "--stage",
        "artifacts/",
        "<svg>",
        ".vtt",
        ".srt",
        "wick auth set",
        "wick auth remove",
        "wick auth status",
        "WICK_NO_KEYCHAIN=1",
        "TAVILY_API_KEY",
        "SERPER_API_KEY",
        "EXA_API_KEY",
        "SEARXNG_URL",
        "federated",
        "--mode",
        "exact",
        "semantic",
        "FetchOutput",
        "BatchItemResult",
        "StagedAssetOutput",
        "ResourceDescriptor",
        "SearchResponse",
        "StatusJson",
        "CrawlResult",
        "MapResult",
    ] {
        assert!(
            readme_full.contains(required),
            "README-FULL.md missing documented capability: {required}"
        );
    }
    for (doc_name, doc_text) in [("README.md", &readme), ("README-FULL.md", &readme_full)] {
        for retired in [
            "--meta-only",
            "--no-cef-daemon",
            "--cef-idle-timeout",
            "MESH_WICK_CEF_IDLE_SECS",
            "MESH_WICK_NO_CEF_DAEMON",
            "cef.sock",
            "v4-meta",
        ] {
            assert!(
                !doc_text.contains(retired),
                "{doc_name} contains retired identifier: {retired}"
            );
        }
    }

    // Canonical skills/mesh-wick/SKILL.md must stay in sync with CLI flags, exit codes, and `wick skill print`.
    let skill_md = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/mesh-wick/SKILL.md"),
    )
    .unwrap();
    for required in [
        "name: mesh-wick",
        "v5-meta",
        "daemon.sock",
        "protocol_version: 3",
        "--source",
        "raw_source",
        "markdown_mirror",
        "markdown_negotiated",
        "--resources",
        "--stage-resources",
        "--referer",
        "--stage",
        "--jsonl",
        "--urls-only",
        "--metadata-only",
        "--daemon-idle-timeout",
        "--no-daemon",
        "WICK_CEF_RENDERER",
        "WICK_DAEMON_EXECUTABLE",
        "WICK_NO_HOME_CEF",
        "wick skill print",
        "artifacts/",
        "wick auth set",
        "wick auth remove",
        "wick auth status",
        "WICK_NO_KEYCHAIN=1",
        "TAVILY_API_KEY",
        "SERPER_API_KEY",
        "EXA_API_KEY",
        "SEARXNG_URL",
        "federated",
        "--mode",
        "exact",
        "semantic",
    ] {
        assert!(
            skill_md.contains(required),
            "skills/mesh-wick/SKILL.md missing documented capability: {required}"
        );
    }
    for retired in [
        "--meta-only",
        "--no-cef-daemon",
        "--cef-idle-timeout",
        "MESH_WICK_CEF_IDLE_SECS",
        "MESH_WICK_NO_CEF_DAEMON",
        "cef.sock",
        "v4-meta",
    ] {
        assert!(
            !skill_md.contains(retired),
            "skills/mesh-wick/SKILL.md contains retired identifier: {retired}"
        );
    }

    let skill_out = Command::new(wick_bin)
        .args(["skill", "print"])
        .output()
        .await
        .unwrap();
    assert_eq!(skill_out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&skill_out.stdout), skill_md);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_direct_stage_rejects_disguised_html_conflicts_and_clears_artifacts() {
    let base = spawn_cli_test_server().await;
    let state_dir = temp_dir();
    let wick_bin = env!("CARGO_BIN_EXE_wick");

    // 1. Direct --stage with --referer stages PNG and prints local path on stdout.
    let stage_plain = Command::new(wick_bin)
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "fetch",
            "--stage",
            "--referer",
            &format!("{base}/ok"),
            &format!("{base}/diagram.png"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        stage_plain.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&stage_plain.stderr)
    );
    let staged_path_str = String::from_utf8_lossy(&stage_plain.stdout)
        .trim()
        .to_string();
    let staged_path = PathBuf::from(&staged_path_str);
    assert!(
        staged_path.exists(),
        "expected staged file at {staged_path_str}"
    );
    assert!(staged_path.starts_with(state_dir.path().join("artifacts")));
    assert_eq!(
        fs::read(&staged_path).unwrap(),
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"
    );
    let stderr_str = String::from_utf8_lossy(&stage_plain.stderr);
    assert!(stderr_str.contains("Status: 200 | Bytes: 16 | Staged:"));

    // 2. Direct --stage --json emits structured JSON.
    let stage_json = Command::new(wick_bin)
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--stage",
            &format!("{base}/file.pdf"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(stage_json.status.code(), Some(0));
    let val: serde_json::Value = serde_json::from_slice(&stage_json.stdout).unwrap();
    assert_eq!(val["status_code"], 200);
    assert_eq!(val["content_type"], "application/pdf");
    assert_eq!(val["byte_count"], 15);
    assert!(Path::new(val["staged_path"].as_str().unwrap()).exists());

    // 3. Disguised HTML-as-PNG soft-404 is rejected with exit 1.
    let stage_fake = Command::new(wick_bin)
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--stage",
            &format!("{base}/fake.png"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(stage_fake.status.code(), Some(1));

    // 4. Flag conflicts with --stage fail cleanly (exit 1 with --json/--jsonl/runtime checks, exit 2 from clap).
    for (args, expected_code) in [
        (
            vec![
                "fetch",
                "--stage",
                "--output",
                "/tmp/unused.png",
                &format!("{base}/diagram.png"),
            ],
            2,
        ),
        (
            vec![
                "fetch",
                "--stage",
                "--metadata-only",
                "--json",
                &format!("{base}/diagram.png"),
            ],
            1,
        ),
        (
            vec![
                "fetch",
                "--stage",
                "--jsonl",
                &format!("{base}/diagram.png"),
            ],
            1,
        ),
        (
            vec![
                "fetch",
                "--stage",
                &format!("{base}/diagram.png"),
                &format!("{base}/file.pdf"),
            ],
            1,
        ),
    ] {
        let out = Command::new(wick_bin)
            .env("WICK_NO_HOME_CEF", "1")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(expected_code),
            "expected exit {expected_code} for {args:?}"
        );
    }

    // 5. `wick session clear` removes `<state_dir>/artifacts`.
    assert!(state_dir.path().join("artifacts").exists());
    let clear_out = Command::new(wick_bin)
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["session", "clear"])
        .output()
        .await
        .unwrap();
    assert_eq!(clear_out.status.code(), Some(0));
    assert!(!state_dir.path().join("artifacts").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_auth_subcommands_help_and_zero_secret_leakage() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let state_dir = temp_dir();
    let wick_bin = env!("CARGO_BIN_EXE_wick");

    // 1. Help parity for `wick search --help`, `wick auth --help`, and `wick auth set --help`.
    let search_help = Command::new(wick_bin)
        .args(["search", "--help"])
        .output()
        .await
        .unwrap();
    assert_eq!(search_help.status.code(), Some(0));
    let search_help_txt = String::from_utf8_lossy(&search_help.stdout);
    for expected in [
        "--provider",
        "--mode",
        "brave",
        "tavily",
        "serper",
        "exa",
        "searxng",
        "duckduckgo",
        "federated",
        "auto",
        "exact",
        "semantic",
    ] {
        assert!(
            search_help_txt.contains(expected),
            "`wick search --help` missing {expected}"
        );
    }

    let auth_help = Command::new(wick_bin)
        .args(["auth", "--help"])
        .output()
        .await
        .unwrap();
    assert_eq!(auth_help.status.code(), Some(0));
    let auth_help_txt = String::from_utf8_lossy(&auth_help.stdout);
    for expected in ["set", "remove", "status"] {
        assert!(
            auth_help_txt.contains(expected),
            "`wick auth --help` missing {expected}"
        );
    }

    let auth_set_help = Command::new(wick_bin)
        .args(["auth", "set", "--help"])
        .output()
        .await
        .unwrap();
    assert_eq!(auth_set_help.status.code(), Some(0));
    let auth_set_help_txt = String::from_utf8_lossy(&auth_set_help.stdout);
    for expected in ["--stdin", "--storage", "auto", "keychain", "file"] {
        assert!(
            auth_set_help_txt.contains(expected),
            "`wick auth set --help` missing {expected}"
        );
    }

    // 2. Non-TTY `wick auth set` without `--stdin` must fail loudly with exit 1.
    let non_tty = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "auth", "set", "brave"])
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(non_tty.status.code(), Some(1));
    let non_tty_err: serde_json::Value = serde_json::from_slice(&non_tty.stderr).unwrap();
    assert_eq!(non_tty_err["kind"], "invalid_input");
    assert!(non_tty_err["error"].as_str().unwrap().contains("--stdin"));

    // 3. Invalid `--stdin` inputs (empty, oversized > 4096 bytes, multi-line) fail loudly without echoing secrets.
    let oversized_secret = "A".repeat(4097);
    for (label, payload, needle) in [
        ("empty", "   \n", ""),
        ("oversized", oversized_secret.as_str(), "AAAA"),
        (
            "multiline",
            "secret-line-1\nsecret-line-2\n",
            "secret-line-1",
        ),
    ] {
        let mut child = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args([
                "--json",
                "auth",
                "set",
                "brave",
                "--stdin",
                "--storage",
                "file",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .await
            .unwrap();
        let out = child.wait_with_output().await.unwrap();
        assert_eq!(out.status.code(), Some(1), "case {label} should exit 1");
        let err_json: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err_json["kind"], "invalid_input", "case {label}");
        if !needle.is_empty() {
            assert!(!String::from_utf8_lossy(&out.stdout).contains(needle));
            assert!(!String::from_utf8_lossy(&out.stderr).contains(needle));
        }
    }

    // 4. `wick auth status --json` and plain text with env vars report source without leaking secret or URL.
    let env_secret = "wick-env-secret-9f8e7d6c";
    let env_searxng = "http://127.0.0.1:18888";
    let env_status_json = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .env("BRAVE_API_KEY", env_secret)
        .env("SEARXNG_URL", env_searxng)
        .env_remove("TAVILY_API_KEY")
        .env_remove("SERPER_API_KEY")
        .env_remove("EXA_API_KEY")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["auth", "status", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(env_status_json.status.code(), Some(0));
    let stdout_str = String::from_utf8_lossy(&env_status_json.stdout);
    let stderr_str = String::from_utf8_lossy(&env_status_json.stderr);
    for forbidden in [env_secret, "9f8e7d6c", env_searxng, "18888"] {
        assert!(
            !stdout_str.contains(forbidden),
            "leaked {forbidden} on stdout"
        );
        assert!(
            !stderr_str.contains(forbidden),
            "leaked {forbidden} on stderr"
        );
    }
    let val: serde_json::Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(val["brave"]["configured"], true);
    assert_eq!(val["brave"]["source"], "environment");
    assert_eq!(val["searxng"]["configured"], true);
    assert_eq!(val["searxng"]["source"], "environment");
    assert_eq!(val["tavily"]["configured"], false);
    assert!(val["tavily"]["source"].is_null());

    let env_status_plain = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .env("BRAVE_API_KEY", env_secret)
        .env("SEARXNG_URL", env_searxng)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["auth", "status"])
        .output()
        .await
        .unwrap();
    assert_eq!(env_status_plain.status.code(), Some(0));
    let plain_out = String::from_utf8_lossy(&env_status_plain.stdout);
    assert!(plain_out.contains("brave: configured (environment)"));
    assert!(plain_out.contains("searxng: configured (environment)"));
    assert!(!plain_out.contains(env_secret));
    assert!(!plain_out.contains(env_searxng));

    // 5. `wick auth set --stdin --storage file` -> `wick auth status --json` -> `wick auth remove`.
    let file_secret = "wick-file-secret-a1b2c3d4";
    let mut set_child = Command::new(wick_bin)
        .env("WICK_NO_KEYCHAIN", "1")
        .env("WICK_NO_HOME_CEF", "1")
        .env_remove("BRAVE_API_KEY")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["auth", "set", "brave", "--stdin", "--storage", "file"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    set_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{file_secret}\n").as_bytes())
        .await
        .unwrap();
    let set_out = set_child.wait_with_output().await.unwrap();
    let set_stdout = String::from_utf8_lossy(&set_out.stdout);
    let set_stderr = String::from_utf8_lossy(&set_out.stderr);
    assert!(!set_stdout.contains(file_secret));
    assert!(!set_stderr.contains(file_secret));

    if set_out.status.code() == Some(1) && set_stderr.contains("not yet implemented") {
        // Wave-header stub in src/auth.rs before auth-store lands: verify remove routes cleanly too.
        let remove_out = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .env_remove("BRAVE_API_KEY")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(["auth", "remove", "brave", "--storage", "file"])
            .output()
            .await
            .unwrap();
        assert_eq!(remove_out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&remove_out.stderr).contains("not yet implemented"));
    } else {
        assert_eq!(set_out.status.code(), Some(0), "stderr: {set_stderr}");
        let file_status = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .env_remove("BRAVE_API_KEY")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(["auth", "status", "--json"])
            .output()
            .await
            .unwrap();
        assert_eq!(file_status.status.code(), Some(0));
        let file_status_str = String::from_utf8_lossy(&file_status.stdout);
        assert!(!file_status_str.contains(file_secret));
        let file_status_json: serde_json::Value = serde_json::from_str(&file_status_str).unwrap();
        assert_eq!(file_status_json["brave"]["configured"], true);
        assert_eq!(file_status_json["brave"]["source"], "file");

        // Removing while BRAVE_API_KEY is set emits a warning on stderr without leaking the key.
        let remove_with_env = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .env("BRAVE_API_KEY", env_secret)
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(["auth", "remove", "brave", "--storage", "file"])
            .output()
            .await
            .unwrap();
        assert_eq!(remove_with_env.status.code(), Some(0));
        let remove_stderr = String::from_utf8_lossy(&remove_with_env.stderr);
        assert!(remove_stderr.contains("BRAVE_API_KEY"));
        assert!(!remove_stderr.contains(env_secret));

        // After removal without env var, brave is no longer configured.
        let after_remove = Command::new(wick_bin)
            .env("WICK_NO_KEYCHAIN", "1")
            .env("WICK_NO_HOME_CEF", "1")
            .env_remove("BRAVE_API_KEY")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(["auth", "status", "--json"])
            .output()
            .await
            .unwrap();
        assert_eq!(after_remove.status.code(), Some(0));
        let after_json: serde_json::Value = serde_json::from_slice(&after_remove.stdout).unwrap();
        assert_eq!(after_json["brave"]["configured"], false);
        assert!(after_json["brave"]["source"].is_null());
    }
}
