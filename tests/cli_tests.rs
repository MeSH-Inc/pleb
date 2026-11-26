use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::path::PathBuf::from(format!(
        "/tmp/mw-cli-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
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
    let state_dir = temp_dir("state");
    let out_dir = temp_dir("out");
    let wick_bin = env!("CARGO_BIN_EXE_wick");

    // 1. status --json emits structured JSON.
    let status_out = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "status"])
        .output()
        .await
        .unwrap();
    assert_eq!(status_out.status.code(), Some(0));
    let status_json: serde_json::Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status_json["cef_renderer"]["expected_protocol"], "v4-meta");

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
    let raw_path = out_dir.join("doc.pdf");
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
    let fail_path = out_dir.join("should-not-exist.md");
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
        .args(["--json", "fetch", "--render", "cronet", &format!("{base}/blocked")])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_403.status.code(), Some(2));
    let err_403_json: serde_json::Value = serde_json::from_slice(&fail_403.stderr).unwrap();
    assert_eq!(err_403_json["kind"], "blocked");

    let fail_pdf = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "fetch", "--render", "cronet", &format!("{base}/file.pdf")])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_pdf.status.code(), Some(5));
    let err_pdf_json: serde_json::Value = serde_json::from_slice(&fail_pdf.stderr).unwrap();
    assert_eq!(err_pdf_json["kind"], "unsupported_content_type");

    let fail_raw_json = Command::new(wick_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "fetch", "--format", "raw", &format!("{base}/file.pdf")])
        .output()
        .await
        .unwrap();
    assert_eq!(fail_raw_json.status.code(), Some(1));
    let err_raw_json: serde_json::Value = serde_json::from_slice(&fail_raw_json.stderr).unwrap();
    assert_eq!(err_raw_json["kind"], "invalid_input");

    // 7. search error without BRAVE_API_KEY respects global --json envelope.
    let fail_search = Command::new(wick_bin)
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

    let _ = fs::remove_dir_all(&state_dir);
    let _ = fs::remove_dir_all(&out_dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_batch_fetch_modes_stdin_and_flag_conflicts() {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let base = spawn_cli_test_server().await;
    let state_dir = temp_dir("batch");
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
    assert!(lines.iter().any(|v| v["ok"] == true && v.get("content").is_none()));
    assert!(lines.iter().any(|v| v["ok"] == false && v["exit_code"] == 3));

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
        vec!["fetch", "--format", "raw", &format!("{base}/ok"), &format!("{base}/ok")],
        vec!["fetch", "--output", "/tmp/unused.md", &format!("{base}/ok"), &format!("{base}/ok")],
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
    let _ = fs::remove_dir_all(&state_dir);
}
