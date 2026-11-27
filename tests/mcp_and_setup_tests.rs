use pleb::context::{RuntimeConfig, RuntimeContext};
use pleb::mcp::serve_io;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

fn hermetic_dir(tag: &str) -> std::path::PathBuf {
    let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::path::PathBuf::from(format!("/tmp/pleb-t-{}-{tag}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

async fn spawn_mcp_test_origin() -> String {
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
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");

                let resp = match path {
                    "/doc" => {
                        let body = "<html><head><title>MCP Guide</title></head><body><main><h1>MCP Guide</h1><p>Clean markdown via local fetch.</p></main></body></html>";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                    "/waf" => concat!(
                        "HTTP/1.1 403 Forbidden\r\n",
                        "Content-Type: text/html\r\n",
                        "Server: cloudflare\r\n",
                        "CF-Ray: abc123-SJC\r\n",
                        "Content-Length: 9\r\n",
                        "Connection: close\r\n\r\n",
                        "Forbidden"
                    )
                    .to_string(),
                    _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string(),
                };
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    format!("http://{addr}")
}

async fn send_rpc<W: AsyncWriteExt + Unpin, R: tokio::io::AsyncBufRead + Unpin>(
    writer: &mut W,
    reader: &mut R,
    req: &Value,
) -> Value {
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).await.unwrap();
    assert!(n > 0, "unexpected EOF waiting for JSON-RPC response");
    serde_json::from_str(resp_line.trim()).unwrap()
}

#[tokio::test]
async fn mcp_initialize_list_fetch_block_diagnostics_and_status() {
    let base = spawn_mcp_test_origin().await;
    let state_dir = hermetic_dir("mcp-io");
    let ctx = Arc::new(
        RuntimeContext::new(RuntimeConfig {
            state_dir: state_dir.clone(),
            timeout: Duration::from_secs(5),
            cef_timeout: Duration::from_secs(5),
            use_daemon: false,
            no_web_bot_auth: true,
            cef_renderer_executable: Some(state_dir.join("missing-cef")),
            ..Default::default()
        })
        .unwrap(),
    );

    // Part A: in-process serve_io over duplex pipes.
    let (client_out, server_in) = tokio::io::duplex(64 * 1024);
    let (server_out, client_in) = tokio::io::duplex(64 * 1024);

    let ctx_server = Arc::clone(&ctx);
    let server_task = tokio::spawn(async move {
        serve_io(ctx_server, BufReader::new(server_in), server_out)
            .await
            .unwrap();
    });

    let mut writer = client_out;
    let mut reader = BufReader::new(client_in);

    // 1. initialize negotiates protocolVersion.
    let init_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            }
        }),
    )
    .await;
    assert_eq!(init_resp["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "pleb");

    // 2. notifications/initialized emits no response; ping returns {}.
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    writer.flush().await.unwrap();
    let ping_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" }),
    )
    .await;
    assert_eq!(ping_resp["id"], 2);
    assert_eq!(ping_resp["result"], json!({}));

    // 3. tools/list returns all 5 expected tools.
    let list_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let tool_names: Vec<&str> = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tool_names,
        vec![
            "pleb_fetch",
            "pleb_search",
            "pleb_crawl",
            "pleb_map",
            "pleb_status"
        ]
    );

    // 4. pleb_fetch returns clean Markdown.
    let fetch_ok = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": { "url": format!("{base}/doc"), "render": "cronet" }
            }
        }),
    )
    .await;
    assert_eq!(fetch_ok["result"]["isError"], false);
    let ok_text = fetch_ok["result"]["content"][0]["text"].as_str().unwrap();
    assert!(ok_text.contains("Title: MCP Guide"), "text={ok_text}");
    assert!(
        ok_text.contains("Clean markdown via local fetch."),
        "text={ok_text}"
    );

    // 5. pleb_fetch on 403 Cloudflare WAF returns isError: true with edge_waf_block.
    let fetch_waf = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": { "url": format!("{base}/waf"), "render": "cronet" }
            }
        }),
    )
    .await;
    assert_eq!(fetch_waf["result"]["isError"], true);
    let waf_text = fetch_waf["result"]["content"][0]["text"].as_str().unwrap();
    let waf_err: Value = serde_json::from_str(waf_text).unwrap();
    assert_eq!(waf_err["kind"], "blocked");
    assert_eq!(waf_err["block_reason"], "edge_waf_block");
    assert!(waf_err["error"].as_str().unwrap().contains("cloudflare"));

    // 6. pleb_status returns JSON transport readiness.
    let status_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "pleb_status", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(status_resp["result"]["isError"], false);
    let st: Value = serde_json::from_str(
        status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(st["cronet_enabled"], cfg!(feature = "cronet"));
    assert_eq!(st["cef_renderer"]["expected_protocol"], "v5-meta");

    // 7. Unknown tool -> -32602; unknown method -> -32601.
    let unknown_tool = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "nonexistent_tool", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(unknown_tool["error"]["code"], -32602);

    let unknown_method = send_rpc(
        &mut writer,
        &mut reader,
        &json!({ "jsonrpc": "2.0", "id": 8, "method": "resources/list" }),
    )
    .await;
    assert_eq!(unknown_method["error"]["code"], -32601);

    drop(writer);
    server_task.await.unwrap();

    // Part B: end-to-end `pleb --no-daemon mcp` subprocess over stdio.
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");
    let mut child = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--no-daemon", "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut sub_in = child.stdin.take().unwrap();
    let mut sub_out = BufReader::new(child.stdout.take().unwrap());

    let sub_init = send_rpc(
        &mut sub_in,
        &mut sub_out,
        &json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
    )
    .await;
    assert_eq!(sub_init["result"]["protocolVersion"], "2025-06-18");

    let sub_fetch = send_rpc(
        &mut sub_in,
        &mut sub_out,
        &json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": { "url": format!("{base}/doc"), "render": "cronet" }
            }
        }),
    )
    .await;
    assert_eq!(sub_fetch["result"]["isError"], false);
    assert!(sub_fetch["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Clean markdown via local fetch."));

    drop(sub_in);
    let out = child.wait_with_output().await.unwrap();
    assert!(out.status.success());

    let _ = std::fs::remove_dir_all(&state_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn setup_registers_and_removes_claude_and_codex_via_mock_clis() {
    use std::os::unix::fs::PermissionsExt;

    let work = hermetic_dir("setup");
    let fake_home = work.join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    let state_dir = work.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let log_file = work.join("invocations.log");

    let write_mock = |name: &str| {
        let path = work.join(format!("mock-{name}.sh"));
        let script = format!(
            "#!/bin/sh\necho \"{name}:$*\" >> \"{}\"\nexit 0\n",
            log_file.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let mock_claude = write_mock("claude");
    let mock_codex = write_mock("codex");
    let fake_pleb_bin = "/fake/bin/pleb";
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // 1. --dry-run does not invoke mock CLIs or write skill files.
    let dry_out = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CODEX_BIN", &mock_codex)
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--dry-run"])
        .output()
        .await
        .unwrap();
    assert!(dry_out.status.success());
    assert!(!log_file.exists(), "dry-run must not run harness CLIs");
    assert!(!fake_home.join(".claude/skills/pleb/SKILL.md").exists());
    let dry_json: Value = serde_json::from_slice(&dry_out.stdout).unwrap();
    assert_eq!(dry_json["dry_run"], true);
    assert_eq!(dry_json["results"][0]["status"], "planned");
    assert_eq!(
        dry_json["results"][0]["command"],
        "claude mcp add -s user -t stdio pleb -- /fake/bin/pleb mcp"
    );
    assert_eq!(
        dry_json["results"][1]["command"],
        "codex mcp add pleb -- /fake/bin/pleb mcp"
    );

    // 2. Full `pleb setup --json` registers both harnesses and installs bundled skills.
    let reg_out = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CODEX_BIN", &mock_codex)
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup"])
        .output()
        .await
        .unwrap();
    assert!(
        reg_out.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&reg_out.stderr)
    );
    let reg_json: Value = serde_json::from_slice(&reg_out.stdout).unwrap();
    assert_eq!(reg_json["results"][0]["status"], "configured");
    assert_eq!(reg_json["results"][1]["status"], "configured");
    assert!(fake_home.join(".claude/skills/pleb/SKILL.md").exists());
    assert!(fake_home.join(".codex/skills/pleb/SKILL.md").exists());

    let log_text = std::fs::read_to_string(&log_file).unwrap();
    let lines: Vec<&str> = log_text.lines().collect();
    assert_eq!(
        lines,
        vec![
            "claude:mcp remove -s user pleb",
            "claude:mcp add -s user -t stdio pleb -- /fake/bin/pleb mcp",
            "codex:mcp remove pleb",
            "codex:mcp add pleb -- /fake/bin/pleb mcp",
        ]
    );

    // 3. Targeted `--target claude --scope local --skip-skill` touches only Claude.
    std::fs::remove_file(&log_file).unwrap();
    let claude_only = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CODEX_BIN", &mock_codex)
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "setup",
            "--target",
            "claude",
            "--scope",
            "local",
            "--skip-skill",
        ])
        .output()
        .await
        .unwrap();
    assert!(claude_only.status.success());
    let claude_log = std::fs::read_to_string(&log_file).unwrap();
    assert_eq!(
        claude_log.lines().collect::<Vec<_>>(),
        vec![
            "claude:mcp remove -s local pleb",
            "claude:mcp add -s local -t stdio pleb -- /fake/bin/pleb mcp",
        ]
    );

    // 4. `--remove` runs remove commands only.
    std::fs::remove_file(&log_file).unwrap();
    let remove_out = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CODEX_BIN", &mock_codex)
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--remove"])
        .output()
        .await
        .unwrap();
    assert!(remove_out.status.success());
    let rem_json: Value = serde_json::from_slice(&remove_out.stdout).unwrap();
    assert_eq!(rem_json["remove"], true);
    assert_eq!(rem_json["results"][0]["status"], "removed");
    assert_eq!(rem_json["results"][1]["status"], "removed");
    let rem_log = std::fs::read_to_string(&log_file).unwrap();
    assert_eq!(
        rem_log.lines().collect::<Vec<_>>(),
        vec!["claude:mcp remove -s user pleb", "codex:mcp remove pleb"]
    );

    // 5. Missing CLI under `--target all` reports `not_found`; explicit `--target claude` fails.
    let missing_all = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PATH", "")
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", work.join("no-claude"))
        .env("PLEB_CODEX_BIN", work.join("no-codex"))
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--skip-skill"])
        .output()
        .await
        .unwrap();
    assert!(missing_all.status.success());
    let missing_json: Value = serde_json::from_slice(&missing_all.stdout).unwrap();
    assert_eq!(missing_json["results"][0]["status"], "not_found");
    assert_eq!(missing_json["results"][1]["status"], "not_found");

    let missing_explicit = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PATH", "")
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", work.join("no-claude"))
        .env("PLEB_SETUP_BINARY", fake_pleb_bin)
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--target", "claude"])
        .output()
        .await
        .unwrap();
    assert_eq!(missing_explicit.status.code(), Some(1));
    let missing_err: Value = serde_json::from_slice(&missing_explicit.stderr).unwrap();
    assert_eq!(missing_err["kind"], "setup_error");

    let _ = std::fs::remove_dir_all(&work);
}
