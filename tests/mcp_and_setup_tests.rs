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

                let resp: Vec<u8> = match path {
                    "/doc" => {
                        let body = "<html><head><title>MCP Guide</title></head><body><main><h1>MCP Guide</h1><p>Clean markdown via local fetch.</p></main></body></html>";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .into_bytes()
                    }
                    "/diagram-page" => {
                        let body = "<html><head><title>Architecture</title></head><body><main><h1>Architecture</h1><p>System pipeline overview.</p><img src=\"/diagram.png\" width=\"640\" height=\"480\" alt=\"Architecture diagram\"></main></body></html>";
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .into_bytes()
                    }
                    "/diagram.png" => {
                        let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00\x1f\x15\xc4\x89\x00\x00\x00\nIDATx\x9cc\x00\x01\x00\x00\x05\x00\x01\r\n-\xb4\x00\x00\x00\x00IEND\xaeB`\x82";
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            png.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(png);
                        out
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
                    .as_bytes()
                    .to_vec(),
                    _ => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = socket.write_all(&resp).await;
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
    let fetch_props = &list_resp["result"]["tools"][0]["inputSchema"]["properties"];
    for param in [
        "url",
        "format",
        "extract",
        "render",
        "source",
        "resources",
        "stage_resources",
        "focus",
        "section",
        "offset",
        "max_bytes",
        "no_images",
        "no_links",
        "no_rewrite",
        "metadata_only",
        "tools",
    ] {
        assert!(
            fetch_props[param]["description"]
                .as_str()
                .is_some_and(|d| !d.trim().is_empty()),
            "pleb_fetch schema param '{param}' missing description"
        );
    }

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

    // 6b. MCP resource cue uses MCP arg names, manifest appends table, and stage_resources stages locally.
    let diag_default = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 61,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": { "url": format!("{base}/diagram-page"), "render": "cronet" }
            }
        }),
    )
    .await;
    assert_eq!(diag_default["result"]["isError"], false);
    let diag_default_text = diag_default["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let diagram_id = pleb::extract::resource_identifier(&format!("{base}/diagram.png"));
    assert!(
        diag_default_text.contains("resources=\"manifest\"")
            && diag_default_text.contains("stage_resources=\"<ids>\"")
            && diag_default_text.contains(&diagram_id),
        "expected MCP-native cue with deterministic diagram id {diagram_id}, got: {diag_default_text}"
    );
    assert!(
        !diag_default_text.contains("--stage-resources"),
        "MCP cue must not leak CLI flags: {diag_default_text}"
    );

    let diag_manifest = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 62,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/diagram-page"),
                    "render": "cronet",
                    "resources": "manifest"
                }
            }
        }),
    )
    .await;
    let diag_manifest_text = diag_manifest["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        diag_manifest_text.contains("## Linked Resources")
            && diag_manifest_text.contains(&format!("| {diagram_id} |"))
            && diag_manifest_text.contains("/diagram.png"),
        "manifest text={diag_manifest_text}"
    );

    let diag_staged = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 63,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/diagram-page"),
                    "render": "cronet",
                    "stage_resources": diagram_id
                }
            }
        }),
    )
    .await;
    assert_eq!(diag_staged["result"]["isError"], false);
    let diag_staged_text = diag_staged["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        diag_staged_text.contains(&format!("Staged: {diagram_id}=")),
        "expected Staged header line for {diagram_id}, got: {diag_staged_text}"
    );

    let diag_missing = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 64,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/diagram-page"),
                    "render": "cronet",
                    "stage_resources": "r_deadbeef"
                }
            }
        }),
    )
    .await;
    assert_eq!(diag_missing["result"]["isError"], false);
    let diag_missing_text = diag_missing["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        diag_missing_text.contains("Warning:")
            && diag_missing_text.contains("r_deadbeef")
            && diag_missing_text.contains("not found")
            && !diag_missing_text.contains("Staged:"),
        "drifted/unknown ID must warn cleanly without staging another resource, got: {diag_missing_text}"
    );

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
        .current_dir(&fake_home)
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

#[cfg(unix)]
#[tokio::test]
async fn setup_claude_webfetch_hooks_preserve_settings_switch_modes_and_run_protocol() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let work = hermetic_dir("hooks");
    let fake_home = work.join("home");
    std::fs::create_dir_all(&fake_home).unwrap();
    let state_dir = work.join("state");
    std::fs::create_dir_all(&state_dir).unwrap();
    let settings_path = work.join("claude/settings.json");
    std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let mock_claude = work.join("mock-claude.sh");
    std::fs::write(&mock_claude, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&mock_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let pleb_bin = env!("CARGO_BIN_EXE_pleb");

    // Seed existing settings and unrelated user hooks that must survive every transition.
    let initial_settings = json!({
        "theme": "dark",
        "hooks": {
            "PreToolUse": [
                {
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "/usr/local/bin/audit-bash" }]
                }
            ],
            "PostToolUseFailure": [
                {
                    "matcher": "Read",
                    "hooks": [{ "type": "command", "command": "/usr/local/bin/on-read-fail" }]
                }
            ]
        }
    });
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&initial_settings).unwrap(),
    )
    .unwrap();

    // 1. Install default redirect hook (PreToolUse) when --hooks is omitted.
    let redir = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CLAUDE_SETTINGS", &settings_path)
        .env("PLEB_SETUP_BINARY", "/fake/bin/pleb")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--target", "claude", "--skip-skill"])
        .output()
        .await
        .unwrap();
    assert!(
        redir.status.success(),
        "{}",
        String::from_utf8_lossy(&redir.stderr)
    );
    let redir_json: Value = serde_json::from_slice(&redir.stdout).unwrap();
    assert_eq!(redir_json["hook"]["status"], "configured");
    assert_eq!(redir_json["hook"]["mode"], "redirect");

    let after_redir: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(after_redir["theme"], "dark");
    assert_eq!(
        after_redir["hooks"]["PostToolUseFailure"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let pre_entries = after_redir["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre_entries.len(), 2);
    assert_eq!(
        pre_entries[1]["hooks"][0]["command"],
        "/fake/bin/pleb hook claude-webfetch --mode redirect"
    );

    // 2. Repeating the same hook install is idempotent (`up_to_date`).
    let redir_again = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CLAUDE_SETTINGS", &settings_path)
        .env("PLEB_SETUP_BINARY", "/fake/bin/pleb")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "setup",
            "--target",
            "claude",
            "--hooks",
            "redirect",
            "--skip-skill",
        ])
        .output()
        .await
        .unwrap();
    assert!(redir_again.status.success());
    let redir_again_json: Value = serde_json::from_slice(&redir_again.stdout).unwrap();
    assert_eq!(redir_again_json["hook"]["status"], "up_to_date");

    // 3. Switching to `--hooks fallback` removes PreToolUse pleb entry and installs PostToolUseFailure.
    let fb = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CLAUDE_SETTINGS", &settings_path)
        .env("PLEB_SETUP_BINARY", "/fake/bin/pleb")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "setup",
            "--target",
            "claude",
            "--hooks",
            "fallback",
            "--skip-skill",
        ])
        .output()
        .await
        .unwrap();
    assert!(fb.status.success());
    let fb_json: Value = serde_json::from_slice(&fb.stdout).unwrap();
    assert_eq!(fb_json["hook"]["status"], "configured");
    assert_eq!(fb_json["hook"]["mode"], "fallback");

    let after_fb: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(after_fb["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    let post_entries = after_fb["hooks"]["PostToolUseFailure"].as_array().unwrap();
    assert_eq!(post_entries.len(), 2);
    assert_eq!(
        post_entries[1]["hooks"][0]["command"],
        "/fake/bin/pleb hook claude-webfetch --mode fallback"
    );

    // 4. `--remove` strips only pleb's hook and leaves unrelated settings/hooks intact.
    let removed = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CLAUDE_SETTINGS", &settings_path)
        .env("PLEB_SETUP_BINARY", "/fake/bin/pleb")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args(["--json", "setup", "--target", "claude", "--remove"])
        .output()
        .await
        .unwrap();
    assert!(removed.status.success());
    let rem_json: Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(rem_json["hook"]["status"], "removed");
    let after_rem: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(after_rem, initial_settings);

    // 5. Runtime hook adapter: fallback returns exit 0 JSON additionalContext; redirect exits 2 on stderr.
    let mut fb_child = Command::new(pleb_bin)
        .args(["hook", "claude-webfetch", "--mode", "fallback"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    fb_child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"WebFetch","tool_input":{"url":"https://docs.example.com/a"},"error":"HTTP 403"}"#)
        .await
        .unwrap();
    let fb_hook_out = fb_child.wait_with_output().await.unwrap();
    assert_eq!(fb_hook_out.status.code(), Some(0));
    let fb_hook_json: Value = serde_json::from_slice(&fb_hook_out.stdout).unwrap();
    assert_eq!(
        fb_hook_json["hookSpecificOutput"]["hookEventName"],
        "PostToolUseFailure"
    );
    let ctx = fb_hook_json["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.contains("pleb_fetch"));
    assert!(ctx.contains("https://docs.example.com/a"));

    let mut redir_child = Command::new(pleb_bin)
        .args(["hook", "claude-webfetch", "--mode", "redirect"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    redir_child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"WebFetch","tool_input":{"url":"https://docs.example.com/b"}}"#)
        .await
        .unwrap();
    let redir_hook_out = redir_child.wait_with_output().await.unwrap();
    assert_eq!(redir_hook_out.status.code(), Some(2));
    assert!(redir_hook_out.stdout.is_empty());
    let redir_err = String::from_utf8_lossy(&redir_hook_out.stderr);
    assert!(redir_err.contains("pleb_fetch"));
    assert!(redir_err.contains("https://docs.example.com/b"));

    // 6. CLI `pleb auth init-web-bot-auth --json` emits public directory JSON without private key d.
    let wba_out = Command::new(pleb_bin)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env_remove("PLEB_WEB_BOT_AUTH_JWK")
        .env_remove("PLEB_WEB_BOT_AUTH_AGENT")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "auth",
            "init-web-bot-auth",
            "--agent",
            "https://bot.example.com",
            "--storage",
            "file",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        wba_out.status.success(),
        "{}",
        String::from_utf8_lossy(&wba_out.stderr)
    );
    let wba_json: Value = serde_json::from_slice(&wba_out.stdout).unwrap();
    assert_eq!(wba_json["agent_url"], "https://bot.example.com");
    assert_eq!(
        wba_json["directory_url"],
        "https://bot.example.com/.well-known/http-message-signatures-directory"
    );
    assert!(wba_json["public_jwk"]["d"].is_null());
    assert!(!String::from_utf8_lossy(&wba_out.stdout).contains("\"d\""));

    // 7. Bare `--hooks --dry-run` plans redirect hook without creating settings.json.
    let dry_settings_path = work.join("nonexistent-claude/settings.json");
    let dry_hooks = Command::new(pleb_bin)
        .env("HOME", &fake_home)
        .env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_CLAUDE_BIN", &mock_claude)
        .env("PLEB_CLAUDE_SETTINGS", &dry_settings_path)
        .env("PLEB_SETUP_BINARY", "/fake/bin/pleb")
        .args(["--state-dir"])
        .arg(&state_dir)
        .args([
            "--json",
            "setup",
            "--target",
            "claude",
            "--hooks",
            "--dry-run",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        dry_hooks.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_hooks.stderr)
    );
    assert!(
        !dry_settings_path.exists(),
        "--dry-run must not create Claude settings.json"
    );
    let dry_hooks_json: Value = serde_json::from_slice(&dry_hooks.stdout).unwrap();
    assert_eq!(dry_hooks_json["hook"]["status"], "planned");
    assert_eq!(dry_hooks_json["hook"]["mode"], "redirect");

    let _ = std::fs::remove_dir_all(&work);
}

#[tokio::test]
async fn mcp_fetch_supports_section_offset_and_flag_parity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());

    tokio::spawn(async move {
        let html = concat!(
            "<!doctype html><html><head><title>MCP Parity</title></head><body><main>",
            "<h1>MCP Parity</h1>",
            "<h2>Intro</h2><p>Opening notes for MCP parity verification.</p>",
            "<h2>Examples</h2>",
            "<p>See the <a href=\"/ref\">reference guide</a> and <img src=\"/fig.png\" alt=\"Fig\"> diagram.</p>",
            "<pre><code class=\"language-rust\">fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n</code></pre>",
            "<form toolname=\"search_docs\" tooldescription=\"Search local docs\" action=\"/search\">",
            "<input type=\"text\" name=\"q\" required></form>",
            "<h2>Outro</h2><p>Closing notes here.</p>",
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

    let state_dir = hermetic_dir("mcp-parity");
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

    let _ = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" }
        }),
    )
    .await;

    // 1. Parameter conflict checks return JSON-RPC -32602.
    for (idx, bad_args) in [
        json!({ "url": format!("{base}/"), "section": "Examples", "focus": "q" }),
        json!({ "url": format!("{base}/"), "offset": 10, "focus": "q" }),
        json!({ "url": format!("{base}/"), "section": "Examples", "format": "html" }),
    ]
    .into_iter()
    .enumerate()
    {
        let err = send_rpc(
            &mut writer,
            &mut reader,
            &json!({
                "jsonrpc": "2.0",
                "id": 100 + idx,
                "method": "tools/call",
                "params": { "name": "pleb_fetch", "arguments": bad_args }
            }),
        )
        .await;
        assert_eq!(err["error"]["code"], -32602, "expected -32602: {err:?}");
    }

    // 2. section + no_images + no_links + no_rewrite + default Tools cue.
    let sec_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/"),
                    "render": "cronet",
                    "section": "Examples",
                    "no_images": true,
                    "no_links": true,
                    "no_rewrite": true
                }
            }
        }),
    )
    .await;
    assert_eq!(sec_resp["result"]["isError"], false);
    let sec_text = sec_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(sec_text.contains("## Examples"), "text={sec_text}");
    assert!(sec_text.contains("Bytes:"), "text={sec_text}");
    assert!(sec_text.contains("tools=\"manifest\""), "text={sec_text}");
    assert!(!sec_text.contains("Opening notes for MCP parity verification."));
    assert!(!sec_text.contains("Closing notes here."));
    assert!(!sec_text.contains("/fig.png"));
    assert!(!sec_text.contains("](/ref"));

    // 3. tools="manifest" appends ## Page Tools (WebMCP).
    let tools_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/"),
                    "render": "cronet",
                    "tools": "manifest"
                }
            }
        }),
    )
    .await;
    let tools_text = tools_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        tools_text.contains("## Page Tools (WebMCP)"),
        "text={tools_text}"
    );
    assert!(tools_text.contains("search_docs"), "text={tools_text}");

    // 4. metadata_only omits document body while preserving status/bytes headers.
    let meta_resp = send_rpc(
        &mut writer,
        &mut reader,
        &json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "pleb_fetch",
                "arguments": {
                    "url": format!("{base}/"),
                    "render": "cronet",
                    "metadata_only": true
                }
            }
        }),
    )
    .await;
    let meta_text = meta_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(meta_text.contains("Title: MCP Parity"), "text={meta_text}");
    assert!(meta_text.contains("Bytes:"), "text={meta_text}");
    assert!(!meta_text.contains("Opening notes for MCP parity verification."));

    // 5. offset continuation using Next-Offset header recovers the entire page.
    let mut offset = 0usize;
    let mut all_pages = String::new();
    for step in 0..10 {
        let mut args = json!({
            "url": format!("{base}/"),
            "render": "cronet",
            "max_bytes": 140
        });
        if offset > 0 {
            args["offset"] = json!(offset);
        }
        let page = send_rpc(
            &mut writer,
            &mut reader,
            &json!({
                "jsonrpc": "2.0",
                "id": 200 + step,
                "method": "tools/call",
                "params": { "name": "pleb_fetch", "arguments": args }
            }),
        )
        .await;
        let text = page["result"]["content"][0]["text"].as_str().unwrap();
        all_pages.push_str(text);
        all_pages.push('\n');
        let next_line = text.lines().find(|l| l.starts_with("Next-Offset: "));
        match next_line {
            Some(line) => {
                let next_val: usize = line
                    .trim_start_matches("Next-Offset: ")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                assert!(next_val > offset);
                offset = next_val;
            }
            None => break,
        }
    }
    assert!(all_pages.contains("fn one()"));
    assert!(all_pages.contains("fn four()"));
    assert!(all_pages.contains("Closing notes here."));

    drop(writer);
    server_task.await.unwrap();
    let _ = std::fs::remove_dir_all(&state_dir);
}
