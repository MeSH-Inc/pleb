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
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{}", std::process::id(), seq));
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
                    "/js-shell" => (
                        "200 OK",
                        "text/html; charset=utf-8",
                        b"<html><head><title>App</title></head><body><div id=\"root\"></div><script src=\"/app.js\"></script><noscript>You need to enable JavaScript to run this app.</noscript></body></html>",
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
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. status --json emits structured JSON.
    let status_out = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "status"])
        .output()
        .await
        .unwrap();
    assert_eq!(status_out.status.code(), Some(0));
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status_json["cef_renderer"]["expected_protocol"], "v6-meta");

    // 2. daemon status --json and session clear --json work cleanly.
    let daemon_out = Command::new(pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["daemon", "status", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(daemon_out.status.code(), Some(0));
    let daemon_json: serde_json::Value = serde_json::from_slice(&daemon_out.stdout).unwrap();
    assert_eq!(daemon_json["running"], false);

    let session_out = Command::new(pleb_bin)
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
    let fetch_ok = Command::new(pleb_bin)
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
    let raw_out = Command::new(pleb_bin)
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
    let fail_404 = Command::new(pleb_bin)
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
    let fail_403 = Command::new(pleb_bin)
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

    let fail_pdf = Command::new(pleb_bin)
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

    let fail_raw_json = Command::new(pleb_bin)
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
    let fail_search = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
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

    let _ = Command::new(pleb_bin)
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
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. Multi-URL --json returns ordered JSON array.
    let batch_json = Command::new(pleb_bin)
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
    let batch_jsonl = Command::new(pleb_bin)
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
    let batch_md = Command::new(pleb_bin)
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
    let mut child = Command::new(pleb_bin)
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
    let mut empty_child = Command::new(pleb_bin)
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
    let uniform_fail = Command::new(pleb_bin)
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

    let hetero_fail = Command::new(pleb_bin)
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
        let out = Command::new(pleb_bin)
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(1), "expected exit 1 for {args:?}");
    }

    let _ = Command::new(pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["daemon", "stop"])
        .output()
        .await;
}

#[tokio::test]
async fn adversarial_status_json_defaults_and_readme_contract_parity() {
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let state_dir = TempDir::new();

    // `pleb status --json` must emit clean default daemon fields when no daemon is running.
    let out = Command::new(pleb_bin)
        .args(["--state-dir"])
        .arg(state_dir.path())
        .args(["status", "--json"])
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "status --json failed");
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["daemon"]["running"], false);
    assert_eq!(val["daemon"]["active_requests"], 0);
    assert_eq!(val["cef_renderer"]["expected_protocol"], "v6-meta");
    assert_eq!(val["cdp_renderer"]["configured"], false);
    assert_eq!(val["cdp_renderer"]["prefer_cdp"], false);
    assert_eq!(val["agent_identity"]["jwk_configured"], false);

    // README.md, README-FULL.md, llms.txt, and llms-full.txt must maintain canonical branding,
    // valid relative file links, valid heading anchors, and CLI/schema contract parity without retired identifiers.
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(manifest_dir.join("README.md")).unwrap();
    let readme_full = std::fs::read_to_string(manifest_dir.join("README-FULL.md")).unwrap();
    let llms_txt = std::fs::read_to_string(manifest_dir.join("llms.txt")).unwrap();
    let llms_full_txt = std::fs::read_to_string(manifest_dir.join("llms-full.txt")).unwrap();
    let license = std::fs::read_to_string(manifest_dir.join("LICENSE")).unwrap();

    for (doc_name, doc_text) in [
        ("README.md", &readme),
        ("README-FULL.md", &readme_full),
        ("llms.txt", &llms_txt),
        ("llms-full.txt", &llms_full_txt),
    ] {
        assert!(
            doc_text.contains("pleb"),
            "{doc_name} missing canonical pleb name"
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
        "name = \"pleb\"",
        "authors = [\"Memory Sync Hub LLC\"]",
        "readme = \"README.md\"",
        "repository = \"https://github.com/MeSH-Inc/pleb\"",
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
    let llms_anchors = extract_markdown_anchors(&llms_txt);
    let llms_full_anchors = extract_markdown_anchors(&llms_full_txt);
    let mut readme_full_anchor_links = 0usize;
    let mut full_internal_anchor_links = 0usize;
    let mut relative_file_links = 0usize;

    for (doc_name, doc_text, self_anchors) in [
        ("README.md", &readme, &readme_anchors),
        ("README-FULL.md", &readme_full, &full_anchors),
        ("llms.txt", &llms_txt, &llms_anchors),
        ("llms-full.txt", &llms_full_txt, &llms_full_anchors),
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
                } else if rel_path == "llms.txt" {
                    &llms_anchors
                } else if rel_path == "llms-full.txt" {
                    &llms_full_anchors
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
        readme_full_anchor_links >= 4,
        "README.md should link to README-FULL.md sections (found {readme_full_anchor_links})"
    );
    assert!(
        full_internal_anchor_links >= 10,
        "README-FULL.md Contents should link to internal sections (found {full_internal_anchor_links})"
    );
    assert!(
        relative_file_links >= 6,
        "README docs should contain verified relative file links (found {relative_file_links})"
    );

    // README-FULL.md must stay in sync with v6-meta, env overrides, batch/search flags, and JSON schemas.
    for required in [
        "v6-meta",
        "PLEB_CEF_RENDERER",
        "PLEB_NO_HOME_CEF",
        "PLEB_DAEMON_EXECUTABLE",
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
        "pleb auth set",
        "pleb auth remove",
        "pleb auth status",
        "PLEB_NO_KEYCHAIN=1",
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
        "corpus_benchmark",
        "Firecrawl",
        "Jina Reader",
        "Crawl4AI",
        "crates.io",
        "packaging/homebrew/pleb.rb",
        "--tools",
        "--render cdp",
        "--cdp-url",
        "--cdp-timeout",
        "--prefer-cdp",
        "--no-web-bot-auth",
        "--format axtree",
        "web-bot-auth",
        "cdp-access",
        "workerd",
        "pleb skill install",
        "warning",
        "content_outcome",
        "acquired_sha256",
        "representation_url",
        "failed_pages",
        "--focus",
        "--section",
        "--offset",
        "next_offset",
        "pleb mcp",
        "pleb setup",
        "--hooks",
        "hook claude-webfetch",
        "PLEB_CLAUDE_SETTINGS",
        "init-web-bot-auth",
        "block_reason",
        "--expand",
        "pleb extract",
        "--max-input-bytes",
        "--no-navigate",
        "cdp_attach",
        "ui_gates",
        "UiGateDescriptor",
        "ExtractOutput",
        "UI-Gated:",
        "--reveal",
        "--scroll",
        "Network XHR",
    ] {
        assert!(
            readme_full.contains(required),
            "README-FULL.md missing documented capability: {required}"
        );
    }
    for required in [
        "pleb",
        "Memory Sync Hub LLC",
        "v6-meta",
        "daemon.sock",
        "corpus_benchmark",
        "Firecrawl",
        "Jina Reader",
        "Crawl4AI",
        "cargo install pleb",
        "pleb skill install",
        "pleb auth set",
        "--stage-resources",
        "FetchOutput",
        "WebMcpToolDescriptor",
        "PLEB_NO_KEYCHAIN",
        "content_outcome",
        "acquired_sha256",
        "representation_url",
        "--focus",
        "--section",
        "--offset",
        "next_offset",
        "pleb mcp",
        "pleb setup",
        "--hooks",
        "hook claude-webfetch",
        "PLEB_CLAUDE_SETTINGS",
        "init-web-bot-auth",
        "block_reason",
        "--expand",
        "pleb extract",
        "--max-input-bytes",
        "--no-navigate",
        "cdp_attach",
        "ui_gates",
        "UiGateDescriptor",
        "ExtractOutput",
        "UI-Gated:",
        "--reveal",
        "--scroll",
        "Network XHR",
    ] {
        assert!(
            llms_full_txt.contains(required),
            "llms-full.txt missing documented capability: {required}"
        );
    }
    let retired_docs_and_skill = [
        "--meta-only",
        "--no-cef-daemon",
        "--cef-idle-timeout",
        "MESH_WICK_CEF_IDLE_SECS",
        "MESH_WICK_NO_CEF_DAEMON",
        "cef.sock",
        "v4-meta",
        "v5-meta",
        "MeSH-wick",
        "MeSH-Inc/mesh-wick",
        "MESH_WICK_HOME",
        "MESH_WICK_PROXY",
        "MESH_WICK_NO_DAEMON",
        "MESH_WICK_DAEMON_IDLE_SECONDS",
        "MESH_WICK_TIMEZONE",
        "WICK_NO_KEYCHAIN",
        "WICK_CEF_RENDERER",
        "WICK_NO_HOME_CEF",
        "WICK_DAEMON_EXECUTABLE",
        "WICK_BUILD_ID",
        "WICK_RENDERER_PROTOCOL",
    ];
    for (doc_name, doc_text) in [
        ("README.md", &readme),
        ("README-FULL.md", &readme_full),
        ("llms.txt", &llms_txt),
        ("llms-full.txt", &llms_full_txt),
    ] {
        for retired in retired_docs_and_skill {
            assert!(
                !doc_text.contains(retired),
                "{doc_name} contains retired identifier: {retired}"
            );
        }
    }

    // Canonical skills/pleb/SKILL.md must stay in sync with CLI flags, exit codes, and `pleb skill print`.
    let skill_md = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/pleb/SKILL.md"),
    )
    .unwrap();
    for required in [
        "name: pleb",
        "v6-meta",
        "daemon.sock",
        "protocol_version: 5",
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
        "PLEB_CEF_RENDERER",
        "PLEB_DAEMON_EXECUTABLE",
        "PLEB_NO_HOME_CEF",
        "pleb skill print",
        "pleb skill install",
        "credentials.enc",
        "youtube",
        "130",
        "artifacts/",
        "pleb auth set",
        "pleb auth remove",
        "pleb auth status",
        "PLEB_NO_KEYCHAIN=1",
        "TAVILY_API_KEY",
        "SERPER_API_KEY",
        "EXA_API_KEY",
        "SEARXNG_URL",
        "federated",
        "--mode",
        "exact",
        "semantic",
        "--tools",
        "--render cdp",
        "--cdp-url",
        "--prefer-cdp",
        "--no-web-bot-auth",
        "--format axtree",
        "content_outcome",
        "acquired_sha256",
        "--focus",
        "--section",
        "--offset",
        "next_offset",
        "pleb mcp",
        "pleb setup",
        "--hooks",
        "hook claude-webfetch",
        "PLEB_CLAUDE_SETTINGS",
        "init-web-bot-auth",
        "block_reason",
        "--expand",
        "pleb extract",
        "--max-input-bytes",
        "--no-navigate",
        "cdp_attach",
        "ui_gates",
        "UiGateDescriptor",
        "ExtractOutput",
        "UI-Gated:",
        "--reveal",
        "--scroll",
        "Network XHR",
    ] {
        assert!(
            skill_md.contains(required),
            "skills/pleb/SKILL.md missing documented capability: {required}"
        );
    }
    for retired in retired_docs_and_skill {
        assert!(
            !skill_md.contains(retired),
            "skills/pleb/SKILL.md contains retired identifier: {retired}"
        );
    }

    let skill_out = Command::new(pleb_bin)
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
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. Direct --stage with --referer stages PNG and prints local path on stdout.
    let stage_plain = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
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
    let stage_json = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
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
    let stage_fake = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
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

    // 4. Flag conflicts and usage errors fail consistently with exit code 1 (both plain and --json/--jsonl).
    for (args, expected_code) in [
        (
            vec![
                "fetch",
                "--stage",
                "--output",
                "/tmp/unused.png",
                &format!("{base}/diagram.png"),
            ],
            1,
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
        let out = Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
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

    // 5. `pleb session clear` removes `<state_dir>/artifacts`.
    assert!(state_dir.path().join("artifacts").exists());
    let clear_out = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
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
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. Help parity for `pleb search --help`, `pleb auth --help`, and `pleb auth set --help`.
    let search_help = Command::new(pleb_bin)
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
            "`pleb search --help` missing {expected}"
        );
    }

    let auth_help = Command::new(pleb_bin)
        .args(["auth", "--help"])
        .output()
        .await
        .unwrap();
    assert_eq!(auth_help.status.code(), Some(0));
    let auth_help_txt = String::from_utf8_lossy(&auth_help.stdout);
    for expected in ["set", "remove", "status"] {
        assert!(
            auth_help_txt.contains(expected),
            "`pleb auth --help` missing {expected}"
        );
    }

    let auth_set_help = Command::new(pleb_bin)
        .args(["auth", "set", "--help"])
        .output()
        .await
        .unwrap();
    assert_eq!(auth_set_help.status.code(), Some(0));
    let auth_set_help_txt = String::from_utf8_lossy(&auth_set_help.stdout);
    for expected in ["--stdin", "--storage", "auto", "keychain", "file"] {
        assert!(
            auth_set_help_txt.contains(expected),
            "`pleb auth set --help` missing {expected}"
        );
    }

    // 2. Non-TTY `pleb auth set` without `--stdin` must fail loudly with exit 1.
    let non_tty = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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
        let mut child = Command::new(pleb_bin)
            .env("PLEB_NO_KEYCHAIN", "1")
            .env("PLEB_NO_HOME_CEF", "1")
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

    // 4. `pleb auth status --json` and plain text with env vars report source without leaking secret or URL.
    let env_secret = "pleb-env-secret-9f8e7d6c";
    let env_searxng = "http://127.0.0.1:18888";
    let env_status_json = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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

    let env_status_plain = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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

    // 5. `pleb auth set --stdin --storage file` -> `pleb auth status --json` -> `pleb auth remove`.
    let file_secret = "pleb-file-secret-a1b2c3d4";
    let mut set_child = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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

    assert_eq!(set_out.status.code(), Some(0), "stderr: {set_stderr}");
    let file_status = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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
    let remove_with_env = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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
    let after_remove = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
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

#[tokio::test]
async fn cli_status_auth_status_redaction_and_m6_flag_validations() {
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let state_dir = temp_dir();

    // 1. status --json redacts query tokens from --cdp-url and reports prefer_cdp.
    let status_out = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
        .env_remove("PLEB_CDP_URL")
        .args(["--state-dir"])
        .arg(state_dir.path())
        .args([
            "--cdp-url",
            "ws://127.0.0.1:9222/devtools/browser?token=super-secret-query",
            "--prefer-cdp",
            "status",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(status_out.status.code(), Some(0));
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["cdp_renderer"]["configured"], true);
    assert_eq!(status_json["cdp_renderer"]["prefer_cdp"], true);
    assert_eq!(
        status_json["cdp_renderer"]["url"],
        "ws://127.0.0.1:9222/devtools/browser"
    );
    assert!(!String::from_utf8_lossy(&status_out.stdout).contains("super-secret-query"));
    assert_eq!(status_json["agent_identity"]["jwk_configured"], false);

    // 2. auth status --json preserves top-level search keys alongside agent_identity & cdp_access.
    let auth_status = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(state_dir.path())
        .args(["auth", "status", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(auth_status.status.code(), Some(0));
    let auth_json: serde_json::Value = serde_json::from_slice(&auth_status.stdout).unwrap();
    assert!(auth_json.get("brave").is_some());
    assert!(auth_json.get("agent_identity").is_some());
    assert!(auth_json.get("cdp_access").is_some());
    assert_eq!(auth_json["cdp_access"]["client_id_configured"], false);

    // 3. Invalid Milestone 6 CLI flag combinations fail with exit code 1 before network calls.
    for args in [
        vec![
            "fetch",
            "https://example.com",
            "--tools",
            "manifest",
            "--stage",
        ],
        vec![
            "fetch",
            "https://example.com",
            "--tools",
            "manifest",
            "--format",
            "raw",
            "--output",
            "/tmp/pleb-unused",
        ],
        vec![
            "fetch",
            "https://example.com",
            "--format",
            "axtree",
            "--stage",
        ],
        vec![
            "fetch",
            "https://example.com",
            "--format",
            "axtree",
            "--render",
            "cronet",
        ],
        vec![
            "fetch",
            "https://example.com",
            "--format",
            "axtree",
            "--render",
            "cef",
        ],
        vec!["fetch", "https://example.com", "--format", "axtree"],
        vec![
            "--cdp-url",
            "ws://127.0.0.1:9222/devtools/browser",
            "--proxy",
            "http://127.0.0.1:8888",
            "fetch",
            "https://example.com",
            "--render",
            "cdp",
        ],
    ] {
        let out = Command::new(pleb_bin)
            .env("PLEB_NO_KEYCHAIN", "1")
            .env("PLEB_NO_HOME_CEF", "1")
            .env_remove("PLEB_CDP_URL")
            .args(["--state-dir"])
            .arg(state_dir.path())
            .args(&args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected exit 1 for {args:?}, got {:?} ({})",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[tokio::test]
async fn cli_fetch_render_cdp_emits_tools_json_metadata() {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{json, Value};
    use tokio_tungstenite::tungstenite::Message;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let Ok(val) = serde_json::from_str::<Value>(text.as_str()) else {
                        continue;
                    };
                    let id = val["id"].as_i64().unwrap_or(0);
                    let method = val["method"].as_str().unwrap_or("");
                    let sid = val["sessionId"].as_str().map(|s| s.to_string());
                    let mk = |id_opt: Option<i64>, ev: Option<&str>, result: Value| {
                        let mut obj = serde_json::Map::new();
                        if let Some(i) = id_opt {
                            obj.insert("id".into(), json!(i));
                            obj.insert("result".into(), result);
                        } else if let Some(m) = ev {
                            obj.insert("method".into(), json!(m));
                            obj.insert("params".into(), result);
                        }
                        if let Some(ref s) = sid {
                            obj.insert("sessionId".into(), json!(s));
                        }
                        Message::Text(Value::Object(obj).to_string().into())
                    };
                    let replies = match method {
                        "Target.createTarget" => {
                            vec![mk(Some(id), None, json!({ "targetId": "tgt-cli" }))]
                        }
                        "Target.attachToTarget" => {
                            vec![mk(Some(id), None, json!({ "sessionId": "sess-cli" }))]
                        }
                        "Target.closeTarget" | "Page.enable" | "Network.enable"
                        | "Fetch.enable" | "WebMCP.enable" => vec![mk(Some(id), None, json!({}))],
                        "Page.getFrameTree" => vec![mk(
                            Some(id),
                            None,
                            json!({ "frameTree": { "frame": { "id": "mf-cli" } } }),
                        )],
                        "Page.navigate" => vec![
                            mk(
                                None,
                                Some("Network.responseReceived"),
                                json!({
                                    "requestId": "req-cli",
                                    "frameId": "mf-cli",
                                    "type": "Document",
                                    "response": {
                                        "url": "https://example.com/cli-cdp",
                                        "status": 200,
                                        "headers": { "Content-Type": "text/html" }
                                    }
                                }),
                            ),
                            mk(
                                None,
                                Some("WebMCP.toolsAdded"),
                                json!({
                                    "tools": [{
                                        "name": "search_docs",
                                        "description": "Search docs",
                                        "frameId": "mf-cli"
                                    }]
                                }),
                            ),
                            mk(Some(id), None, json!({})),
                            mk(None, Some("Page.loadEventFired"), json!({})),
                        ],
                        "DOM.getDocument" => {
                            vec![mk(Some(id), None, json!({ "root": { "nodeId": 11 } }))]
                        }
                        "DOM.getOuterHTML" => vec![mk(
                            Some(id),
                            None,
                            json!({ "outerHTML": "<html><head><title>CLI CDP</title></head><body><main><p>CLI rendered body</p></main></body></html>" }),
                        )],
                        _ => {
                            let mut obj = json!({
                                "id": id,
                                "error": { "code": -32601, "message": "not found" }
                            });
                            if let Some(ref s) = sid {
                                obj["sessionId"] = json!(s);
                            }
                            vec![Message::Text(obj.to_string().into())]
                        }
                    };
                    for m in replies {
                        if ws.send(m).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });

    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let state_dir = temp_dir();
    let ws_url = format!("ws://127.0.0.1:{}/devtools/browser", addr.port());

    let out = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(state_dir.path())
        .args([
            "--cdp-url",
            &ws_url,
            "fetch",
            "https://example.com/cli-cdp",
            "--render",
            "cdp",
            "--json",
            "--metadata-only",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["strategy"], "cdp");
    assert_eq!(val["tools"][0]["name"], "search_docs");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_contract_hardening_exit_codes_js_shell_warning_crawl_seed_keychain_and_skill_install()
{
    use tokio::io::AsyncWriteExt;

    let base = spawn_cli_test_server().await;
    let state_dir = temp_dir();
    let skill_root = temp_dir();
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. Unknown CLI flags without --json exit 1 (never colliding with exit 2); --help/--version exit 0.
    let bad_flag = Command::new(pleb_bin)
        .args(["fetch", "--not-a-real-flag", "https://example.com"])
        .output()
        .await
        .unwrap();
    assert_eq!(bad_flag.status.code(), Some(1));
    assert!(bad_flag.stdout.is_empty());
    assert!(!bad_flag.stderr.is_empty());

    for ok_args in [["--help"], ["--version"]] {
        let ok = Command::new(pleb_bin).args(ok_args).output().await.unwrap();
        assert_eq!(ok.status.code(), Some(0));
    }

    // 2. Unrendered JavaScript shell surfaces an explicit warning in stderr, --json, --metadata-only, and --jsonl.
    let js_plain = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env_remove("PLEB_CDP_URL")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--no-daemon", "fetch", &format!("{base}/js-shell")])
        .output()
        .await
        .unwrap();
    assert_eq!(js_plain.status.code(), Some(0));
    let js_plain_stderr = String::from_utf8_lossy(&js_plain.stderr);
    assert!(
        js_plain_stderr.contains("Warning:") && js_plain_stderr.contains("JavaScript shell"),
        "expected JS shell warning on stderr, got: {js_plain_stderr}"
    );

    let js_json = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env_remove("PLEB_CDP_URL")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            &format!("{base}/js-shell"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(js_json.status.code(), Some(0));
    let js_val: serde_json::Value = serde_json::from_slice(&js_json.stdout).unwrap();
    assert_eq!(js_val["content_outcome"], "incomplete_shell");
    assert_eq!(js_val["acquired_sha256"].as_str().map(str::len), Some(64));
    assert!(
        js_val["warning"]
            .as_str()
            .is_some_and(|w| w.contains("JavaScript shell")),
        "expected FetchOutput.warning, got: {js_val}"
    );

    let js_meta = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env_remove("PLEB_CDP_URL")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--metadata-only",
            &format!("{base}/js-shell"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        js_meta.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&js_meta.stderr)
    );
    let js_meta_val: serde_json::Value = serde_json::from_slice(&js_meta.stdout).unwrap();
    assert_eq!(js_meta_val["content_outcome"], "incomplete_shell");
    assert_eq!(
        js_meta_val["acquired_sha256"].as_str().map(str::len),
        Some(64)
    );
    assert!(
        js_meta_val["warning"]
            .as_str()
            .is_some_and(|w| w.contains("JavaScript shell")),
        "expected FetchMetadataOutput.warning, got: {js_meta_val}"
    );

    let js_jsonl = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env_remove("PLEB_CDP_URL")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "fetch",
            "--jsonl",
            &format!("{base}/js-shell"),
            &format!("{base}/ok"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(js_jsonl.status.code(), Some(0));
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&js_jsonl.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let shell_item = lines
        .iter()
        .find(|item| item["url"].as_str().unwrap().ends_with("/js-shell"))
        .unwrap();
    let ok_item = lines
        .iter()
        .find(|item| item["url"].as_str().unwrap().ends_with("/ok"))
        .unwrap();
    assert_eq!(shell_item["content_outcome"], "incomplete_shell");
    assert_eq!(ok_item["content_outcome"], "complete");
    assert_eq!(
        shell_item["acquired_sha256"].as_str().map(str::len),
        Some(64)
    );
    assert_eq!(ok_item["acquired_sha256"].as_str().map(str::len), Some(64));
    assert!(shell_item["warning"]
        .as_str()
        .is_some_and(|w| w.contains("JavaScript shell")));
    assert!(ok_item.get("warning").is_none());

    // 3. `pleb crawl` fails loudly when the seed URL fails rather than silently exiting 0.
    let bad_crawl = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "crawl",
            "--no-robots",
            &format!("{base}/missing"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(bad_crawl.status.code(), Some(3));
    let bad_crawl_err: serde_json::Value = serde_json::from_slice(&bad_crawl.stderr).unwrap();
    assert_eq!(bad_crawl_err["kind"], "http_error");
    assert_eq!(bad_crawl_err["exit_code"], 3);

    // 4. `PLEB_NO_KEYCHAIN=true` disables macOS Keychain consistently with `PLEB_NO_KEYCHAIN=1`.
    let mut kc_child = Command::new(pleb_bin)
        .env("PLEB_NO_KEYCHAIN", "true")
        .env("PLEB_NO_HOME_CEF", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["auth", "set", "brave", "--stdin", "--storage", "keychain"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    kc_child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"dummy-secret\n")
        .await
        .unwrap();
    let kc_out = kc_child.wait_with_output().await.unwrap();
    assert_eq!(kc_out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&kc_out.stderr).contains("PLEB_NO_KEYCHAIN=1"));

    // 5. `pleb skill install --dir` writes canonical SKILL.md, stays idempotent, guards edits, and supports --force.
    let target_dir = skill_root.path().join("pleb");
    let canonical = Command::new(pleb_bin)
        .args(["skill", "print"])
        .output()
        .await
        .unwrap();
    assert_eq!(canonical.status.code(), Some(0));
    let canonical_str = String::from_utf8_lossy(&canonical.stdout).to_string();

    let first = Command::new(pleb_bin)
        .args([
            "--json",
            "skill",
            "install",
            "--dir",
            target_dir.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(
        first.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_json["installed"][0]["status"], "installed");
    let installed_file = target_dir.join("SKILL.md");
    assert_eq!(fs::read_to_string(&installed_file).unwrap(), canonical_str);

    let second = Command::new(pleb_bin)
        .args([
            "--json",
            "skill",
            "install",
            "--dir",
            target_dir.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(second.status.code(), Some(0));
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["installed"][0]["status"], "up_to_date");

    fs::write(&installed_file, "local customization\n").unwrap();
    let conflict = Command::new(pleb_bin)
        .args(["skill", "install", "--dir", target_dir.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert_eq!(conflict.status.code(), Some(1));
    assert_eq!(
        fs::read_to_string(&installed_file).unwrap(),
        "local customization\n"
    );

    let forced = Command::new(pleb_bin)
        .args([
            "--json",
            "skill",
            "install",
            "--dir",
            target_dir.to_str().unwrap(),
            "--force",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(forced.status.code(), Some(0));
    let forced_json: serde_json::Value = serde_json::from_slice(&forced.stdout).unwrap();
    assert_eq!(forced_json["installed"][0]["status"], "installed");
    assert_eq!(fs::read_to_string(&installed_file).unwrap(), canonical_str);

    // 6. `--focus` rejects incompatible formats/flags loudly and works cleanly on markdown fetches.
    for bad_args in [
        vec![
            "--json",
            "fetch",
            "--focus",
            "q",
            "--stage",
            &format!("{base}/ok"),
        ],
        vec![
            "--json",
            "fetch",
            "--focus",
            "q",
            "--format",
            "html",
            &format!("{base}/ok"),
        ],
        vec![
            "fetch",
            "--focus",
            "q",
            "--format",
            "raw",
            "-o",
            "/tmp/unused.bin",
            &format!("{base}/ok"),
        ],
    ] {
        let out = Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
            .env("PLEB_NO_KEYCHAIN", "1")
            .args(&bad_args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected conflict exit 1 for {bad_args:?}"
        );
    }

    let focused_ok = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--focus",
            "guide",
            "--max-bytes",
            "1024",
            &format!("{base}/ok"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(focused_ok.status.code(), Some(0));
    let focused_json: serde_json::Value = serde_json::from_slice(&focused_ok.stdout).unwrap();
    assert_eq!(focused_json["content_outcome"], "complete");
    assert!(focused_json["content_bytes"].as_u64().unwrap() <= 1024);
}

#[tokio::test]
async fn cli_stage_resources_with_max_bytes_prints_staged_paths_on_stderr() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        let padding = "System architecture walkthrough paragraph. ".repeat(30);
        let html = format!(
            "<!doctype html><html><head><title>Arch</title></head><body><main>\
             <h1>Arch</h1><img width=\"800\" height=\"400\" src=\"/arch.png\" alt=\"System diagram\">\
             <p>{padding}</p></main></body></html>"
        );
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let html = html.clone();
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
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let (status, ct, body): (&str, &str, Vec<u8>) = match path {
                    "/page" => ("200 OK", "text/html; charset=utf-8", html.into_bytes()),
                    "/arch.png" => (
                        "200 OK",
                        "image/png",
                        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec(),
                    ),
                    _ => ("404 Not Found", "text/plain", b"missing".to_vec()),
                };
                let hdr = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(hdr.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            });
        }
    });

    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let state_dir = temp_dir();
    let diagram_id = pleb::extract::resource_identifier(&format!("{base}/arch.png"));

    let out = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "fetch",
            "--render",
            "cronet",
            "--stage-resources",
            &diagram_id,
            "--max-bytes",
            "120",
            &format!("{base}/page"),
        ])
        .output()
        .await
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.len() <= 120,
        "stdout exceeded max-bytes: {} bytes",
        out.stdout.len()
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let prefix = format!("Staged: {diagram_id}=");
    let staged_line = stderr
        .lines()
        .find(|l| l.starts_with(&prefix))
        .unwrap_or_else(|| panic!("expected stderr {prefix:?}; got:\n{stderr}"));
    let staged_path = Path::new(staged_line.trim_start_matches(&prefix).trim());
    assert!(
        staged_path.exists(),
        "staged path missing on disk: {}",
        staged_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_section_offset_pagination_and_conflict_validation() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        let html = concat!(
            "<!doctype html><html><head><title>CLI Slicing</title></head><body><main>",
            "<h1>CLI Slicing</h1>",
            "<h2>Install</h2><p>Run cargo install pleb to begin setup quickly.</p>",
            "<h2>Usage Guide</h2>",
            "<pre><code class=\"language-python\">print('alpha line')\nprint('beta line')\nprint('gamma line')\nprint('delta line')\n</code></pre>",
            "<h2>Appendix</h2><p>Extra trailing appendix notes.</p>",
            "</main></body></html>"
        );
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
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    html.len()
                );
                let _ = socket.write_all(hdr.as_bytes()).await;
                let _ = socket.write_all(html.as_bytes()).await;
            });
        }
    });

    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let state_dir =
        std::path::PathBuf::from(format!("/tmp/pleb-t-{}-cli-slice", std::process::id()));
    let _ = fs::remove_dir_all(&state_dir);
    fs::create_dir_all(&state_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    for bad_args in [
        vec![
            "fetch",
            "--section",
            "Install",
            "--focus",
            "q",
            &format!("{base}/"),
        ],
        vec![
            "fetch",
            "--offset",
            "10",
            "--focus",
            "q",
            &format!("{base}/"),
        ],
        vec![
            "fetch",
            "--section",
            "Install",
            "--format",
            "text",
            &format!("{base}/"),
        ],
        vec![
            "fetch",
            "--section",
            "Install",
            "--format",
            "html",
            &format!("{base}/"),
        ],
        vec![
            "fetch",
            "--section",
            "Install",
            "--stage",
            &format!("{base}/"),
        ],
        vec!["fetch", "--offset", "10", "--stage", &format!("{base}/")],
        vec![
            "fetch",
            "--offset",
            "10",
            "--format",
            "raw",
            "-o",
            "/tmp/unused.bin",
            &format!("{base}/"),
        ],
    ] {
        let out = Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
            .env("PLEB_NO_KEYCHAIN", "1")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(["--no-daemon"])
            .args(&bad_args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected exit 1 for {bad_args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // `--section` returns exact section without truncation or next_offset.
    let sec_out = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--render",
            "cronet",
            "--section",
            "Install",
            &format!("{base}/"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(sec_out.status.code(), Some(0));
    let sec_json: serde_json::Value = serde_json::from_slice(&sec_out.stdout).unwrap();
    assert_eq!(sec_json["truncated"], false);
    assert!(sec_json["next_offset"].is_null());
    let sec_content = sec_json["content"].as_str().unwrap();
    assert!(sec_content.contains("cargo install pleb"));
    assert!(!sec_content.contains("Appendix"));

    // Plain stderr emits `Next-Offset:` when truncated by `--max-bytes`.
    let plain_trunc = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "fetch",
            "--render",
            "cronet",
            "--max-bytes",
            "110",
            &format!("{base}/"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(plain_trunc.status.code(), Some(0));
    assert!(plain_trunc.stdout.len() <= 110);
    let plain_err = String::from_utf8_lossy(&plain_trunc.stderr);
    assert!(
        plain_err.lines().any(|l| l.starts_with("Next-Offset: ")),
        "expected Next-Offset header on stderr, got:\n{plain_err}"
    );

    // `--metadata-only` retains `next_offset` and omits `content`.
    let meta_out = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--no-daemon",
            "--json",
            "fetch",
            "--render",
            "cronet",
            "--max-bytes",
            "110",
            "--metadata-only",
            &format!("{base}/"),
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(meta_out.status.code(), Some(0));
    let meta_json: serde_json::Value = serde_json::from_slice(&meta_out.stdout).unwrap();
    assert!(meta_json["content"].is_null());
    assert!(meta_json["next_offset"].as_u64().is_some());

    // JSON sequential pagination via `--offset` converges cleanly and preserves fences.
    let mut offset = 0u64;
    let mut combined = String::new();
    for _ in 0..12 {
        let offset_str = offset.to_string();
        let page_out = Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
            .env("PLEB_NO_KEYCHAIN", "1")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args([
                "--no-daemon",
                "--json",
                "fetch",
                "--render",
                "cronet",
                "--max-bytes",
                "120",
                "--offset",
                &offset_str,
                &format!("{base}/"),
            ])
            .output()
            .await
            .unwrap();
        assert_eq!(page_out.status.code(), Some(0));
        let page_json: serde_json::Value = serde_json::from_slice(&page_out.stdout).unwrap();
        let chunk = page_json["content"].as_str().unwrap();
        assert!(chunk.len() <= 120);
        assert!(pleb::extract::markdown::open_fence_at(chunk, chunk.len()).is_none());
        combined.push_str(chunk);
        combined.push('\n');
        match page_json["next_offset"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert!(combined.contains("alpha line"));
    assert!(combined.contains("delta line"));
    assert!(combined.contains("Extra trailing appendix notes."));

    let _ = fs::remove_dir_all(&state_dir);
}

#[tokio::test]
async fn cli_rejects_invalid_reveal_and_scroll_flag_combinations() {
    let state_dir = temp_dir();
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    for bad_args in [
        vec![
            "--no-daemon",
            "fetch",
            "--scroll",
            "21",
            "https://example.com",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cronet",
            "--reveal",
            "tabs",
            "https://example.com",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cronet",
            "--scroll",
            "2",
            "https://example.com",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            "ws://127.0.0.1:9222/devtools/page/abc",
            "--no-navigate",
            "--reveal",
            "all",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--render",
            "cdp",
            "--cdp-url",
            "ws://127.0.0.1:9222/devtools/page/abc",
            "--no-navigate",
            "--scroll",
            "1",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--stage",
            "--reveal",
            "tabs",
            "https://example.com/a.png",
        ],
        vec![
            "--no-daemon",
            "fetch",
            "--format",
            "raw",
            "--scroll",
            "1",
            "https://example.com",
        ],
    ] {
        let out = Command::new(pleb_bin)
            .env("PLEB_NO_HOME_CEF", "1")
            .env("PLEB_NO_KEYCHAIN", "1")
            .args(["--state-dir"])
            .arg(&state_dir)
            .args(&bad_args)
            .output()
            .await
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected exit 1 for {bad_args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
