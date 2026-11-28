use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

struct ShortTempDir(PathBuf);

impl ShortTempDir {
    fn new() -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{seq}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        #[cfg(unix)]
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ShortTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn pleb_extract_cmd(pleb_home: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pleb"));
    cmd.env("PLEB_NO_HOME_CEF", "1")
        .env("PLEB_NO_KEYCHAIN", "1")
        .env("PLEB_HOME", pleb_home);
    cmd
}

fn run_with_stdin(args: &[&str], stdin_bytes: &[u8], pleb_home: &str) -> std::process::Output {
    let mut child = pleb_extract_cmd(pleb_home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pleb extract");
    child
        .stdin
        .as_mut()
        .expect("piped stdin")
        .write_all(stdin_bytes)
        .expect("write stdin");
    child.wait_with_output().expect("wait pleb extract")
}

const TABBED_HTML: &str = concat!(
    "<!doctype html><html><head><title>SDK Tabs</title></head><body><main>",
    "<h1>SDK Tabs</h1>",
    "<h2>Install</h2>",
    "<div role=\"tablist\">",
    "<button role=\"tab\" id=\"t-js\" aria-controls=\"p-js\" aria-selected=\"true\">JavaScript</button>",
    "<button role=\"tab\" id=\"t-rs\" aria-controls=\"p-rs\" aria-selected=\"false\">Rust</button>",
    "</div>",
    "<div id=\"p-js\" role=\"tabpanel\" aria-labelledby=\"t-js\"><p>Run npm install sdk.</p></div>",
    "<div id=\"p-rs\" role=\"tabpanel\" aria-labelledby=\"t-rs\" hidden>",
    "<p>First Rust line for offset slicing check.</p>",
    "<p>Second Rust line with cargo add sdk details.</p>",
    "</div>",
    "<img width=\"800\" height=\"400\" src=\"/diagram.png\" alt=\"Arch diagram\">",
    "<form toolname=\"search_docs\" tooldescription=\"Search docs\"><input name=\"q\"></form>",
    "</main></body></html>"
);

#[test]
fn extract_stdin_file_expand_section_offset_and_zero_home_touch() {
    let dir = ShortTempDir::new();
    let html_file = dir.path().join("snapshot.html");
    fs::write(&html_file, TABBED_HTML).unwrap();

    // Even with an unwritable PLEB_HOME, offline extract succeeds without touching ~/.pleb or creating daemon.sock.
    let unwritable_home = "/dev/null/pleb-extract-nope";

    // 1. Stdin with `--expand none` emits UI-Gated cue on stderr and keeps hidden Rust panel out.
    let stdin_none = run_with_stdin(
        &["extract", "--url", "https://docs.example.com/api", "-"],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert_eq!(stdin_none.status.code(), Some(0));
    let out_none = String::from_utf8_lossy(&stdin_none.stdout);
    let err_none = String::from_utf8_lossy(&stdin_none.stderr);
    assert!(out_none.contains("Run npm install sdk."), "stdout={out_none}");
    assert!(!out_none.contains("cargo add sdk"), "stdout={out_none}");
    assert!(
        err_none.contains("UI-Gated: 1 tab in DOM (pass --expand tabs) | untrusted labels: \"Rust\""),
        "stderr={err_none}"
    );
    assert!(!dir.path().join("daemon.sock").exists());

    // 2. File input with `--expand tabs --section "Tab: Rust"` recovers Rust panel, then `--offset` continues cleanly.
    let sec_full = pleb_extract_cmd(unwritable_home)
        .args([
            "--json",
            "extract",
            html_file.to_str().unwrap(),
            "--url",
            "https://docs.example.com/api",
            "--expand",
            "tabs",
            "--section",
            "Tab: Rust",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(sec_full.status.code(), Some(0));
    let full_json: serde_json::Value = serde_json::from_slice(&sec_full.stdout).unwrap();
    let full_content = full_json["content"].as_str().unwrap();
    assert!(full_content.contains("### Tab: Rust"), "content={full_content}");
    assert!(full_content.contains("cargo add sdk"), "content={full_content}");
    assert!(!full_content.contains("npm install"), "content={full_content}");
    assert!(full_json.get("ui_gates").is_none());

    let sec_head = pleb_extract_cmd(unwritable_home)
        .args([
            "--json",
            "extract",
            html_file.to_str().unwrap(),
            "--url",
            "https://docs.example.com/api",
            "--expand",
            "tabs",
            "--section",
            "Tab: Rust",
            "--max-bytes",
            "45",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(sec_head.status.code(), Some(0));
    let head_json: serde_json::Value = serde_json::from_slice(&sec_head.stdout).unwrap();
    assert_eq!(head_json["truncated"], true);
    let next_offset = head_json["next_offset"].as_u64().unwrap().to_string();

    let sec_tail = pleb_extract_cmd(unwritable_home)
        .args([
            "--json",
            "extract",
            html_file.to_str().unwrap(),
            "--url",
            "https://docs.example.com/api",
            "--expand",
            "tabs",
            "--section",
            "Tab: Rust",
            "--offset",
            &next_offset,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(sec_tail.status.code(), Some(0));
    let tail_json: serde_json::Value = serde_json::from_slice(&sec_tail.stdout).unwrap();
    assert_eq!(tail_json["truncated"], false);
    assert!(
        tail_json["content"]
            .as_str()
            .unwrap()
            .contains("cargo add sdk"),
        "tail={tail_json:?}"
    );
}

#[test]
fn extract_manifests_paginate_and_atomic_output_preserves_existing_file_on_error() {
    let dir = ShortTempDir::new();
    let out_path = dir.path().join("result.json");
    let unwritable_home = "/dev/null/pleb-extract-nope";

    // 1. Combined `--resources manifest --tools manifest` crosses `--max-bytes` boundary cleanly.
    let first = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com/api",
            "--resources",
            "manifest",
            "--tools",
            "manifest",
            "--max-bytes",
            "120",
        ],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert_eq!(first.status.code(), Some(0));
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_json["truncated"], true);
    let mut offset = first_json["next_offset"].as_u64().unwrap();
    let mut combined = first_json["content"].as_str().unwrap().to_string();

    for _ in 0..10 {
        let off_str = offset.to_string();
        let next = run_with_stdin(
            &[
                "--json",
                "extract",
                "--url",
                "https://docs.example.com/api",
                "--resources",
                "manifest",
                "--tools",
                "manifest",
                "--max-bytes",
                "120",
                "--offset",
                &off_str,
            ],
            TABBED_HTML.as_bytes(),
            unwritable_home,
        );
        assert_eq!(next.status.code(), Some(0));
        let j: serde_json::Value = serde_json::from_slice(&next.stdout).unwrap();
        combined.push_str(j["content"].as_str().unwrap());
        if let Some(n) = j["next_offset"].as_u64() {
            assert!(n > offset);
            offset = n;
        } else {
            assert_eq!(j["truncated"], false);
            break;
        }
    }
    assert!(combined.contains("## Linked Resources"), "combined={combined}");
    assert!(combined.contains("/diagram.png"), "combined={combined}");
    assert!(combined.contains("## Page Tools (WebMCP)"), "combined={combined}");
    assert!(combined.contains("search_docs"), "combined={combined}");

    // 2. `--json -o <path>` writes valid JSON to `-o` with empty stdout.
    let write_ok = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com/api",
            "-o",
            out_path.to_str().unwrap(),
        ],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert_eq!(write_ok.status.code(), Some(0));
    assert!(
        write_ok.stdout.is_empty(),
        "expected empty stdout with -o, got {:?}",
        String::from_utf8_lossy(&write_ok.stdout)
    );
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&out_path).unwrap()).expect("valid JSON in -o file");
    assert_eq!(written["title"], "SDK Tabs");
    let before_bytes = fs::read(&out_path).unwrap();

    // 3. Failing extract (binary exit 5) never clobbers an existing `-o` file.
    let fail_bin = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com/api",
            "-o",
            out_path.to_str().unwrap(),
        ],
        b"%PDF-1.7\n1 0 obj\n",
        unwritable_home,
    );
    assert_eq!(fail_bin.status.code(), Some(5));
    assert!(fail_bin.stdout.is_empty());
    assert_eq!(fs::read(&out_path).unwrap(), before_bytes);

    // 4. `--metadata-only` omits content while keeping `ui_gates`, requires `--json`, and conflicts with `-o`.
    let meta = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com/api",
            "--metadata-only",
        ],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert_eq!(meta.status.code(), Some(0));
    let meta_json: serde_json::Value = serde_json::from_slice(&meta.stdout).unwrap();
    assert!(meta_json.get("content").is_none());
    assert_eq!(meta_json["ui_gates"][0]["kind"], "tablist");

    let meta_no_json = run_with_stdin(
        &[
            "extract",
            "--url",
            "https://docs.example.com/api",
            "--metadata-only",
        ],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert_eq!(meta_no_json.status.code(), Some(1));

    let meta_with_output = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com/api",
            "--metadata-only",
            "-o",
            out_path.to_str().unwrap(),
        ],
        TABBED_HTML.as_bytes(),
        unwritable_home,
    );
    assert!(!meta_with_output.status.success());
    assert_eq!(fs::read(&out_path).unwrap(), before_bytes);
}

#[test]
fn extract_flag_rejections_input_limits_binary_guard_and_utf16_success() {
    let unwritable_home = "/dev/null/pleb-extract-nope";
    let dir = ShortTempDir::new();
    let blocked_out = dir.path().join("must-not-exist.json");

    // 1. Flag validation conflicts and unsupported options exit 1.
    for (args, expected_kind) in [
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--focus",
                "sdk",
                "--section",
                "Install",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--focus",
                "sdk",
                "--offset",
                "10",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--max-response-bytes",
                "1024",
                "extract",
                "--url",
                "https://docs.example.com",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--max-input-bytes",
                "0",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--format",
                "raw",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--format",
                "axtree",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "https://docs.example.com",
                "--resources",
                "transcripts",
            ],
            "invalid_input",
        ),
        (
            vec![
                "--json",
                "extract",
                "--url",
                "ftp://docs.example.com/file",
            ],
            "invalid_url",
        ),
    ] {
        let out = run_with_stdin(&args, TABBED_HTML.as_bytes(), unwritable_home);
        assert_eq!(
            out.status.code(),
            Some(1),
            "expected exit 1 for {args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let err_json: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err_json["kind"], expected_kind);
    }

    // 2. `--max-input-bytes` exact limit succeeds; `limit + 1` fails with `response_too_large`.
    let html_bytes = TABBED_HTML.as_bytes();
    let exact_str = html_bytes.len().to_string();
    let exact = run_with_stdin(
        &[
            "extract",
            "--url",
            "https://docs.example.com",
            "--max-input-bytes",
            &exact_str,
        ],
        html_bytes,
        unwritable_home,
    );
    assert_eq!(exact.status.code(), Some(0));

    let too_small_str = (html_bytes.len() - 1).to_string();
    let over = run_with_stdin(
        &[
            "--json",
            "extract",
            "--url",
            "https://docs.example.com",
            "--max-input-bytes",
            &too_small_str,
        ],
        html_bytes,
        unwritable_home,
    );
    assert_eq!(over.status.code(), Some(1));
    let over_json: serde_json::Value = serde_json::from_slice(&over.stderr).unwrap();
    assert_eq!(over_json["kind"], "response_too_large");

    // 3. Binary guard exit 5 with empty stdout and no `-o` file created.
    for (ct, payload) in [
        (None, b"%PDF-1.7\n%binary\n".as_slice()),
        (Some("text/html"), b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".as_slice()),
        (Some("text/html; charset=utf-8"), b"abc\x00def\x00ghi".as_slice()),
    ] {
        let mut args = vec![
            "--json",
            "extract",
            "--url",
            "https://docs.example.com",
            "-o",
            blocked_out.to_str().unwrap(),
        ];
        if let Some(c) = ct {
            args.push("--content-type");
            args.push(c);
        }
        let bin_out = run_with_stdin(&args, payload, unwritable_home);
        assert_eq!(bin_out.status.code(), Some(5));
        assert!(bin_out.stdout.is_empty());
        assert!(!blocked_out.exists());
        let err_json: serde_json::Value = serde_json::from_slice(&bin_out.stderr).unwrap();
        assert_eq!(err_json["kind"], "unsupported_content_type");
    }

    // 4. Valid UTF-16 LE BOM HTML succeeds despite alternating NUL bytes.
    let utf16_html = "<!doctype html><html><head><title>UTF16 Doc</title></head><body><main><p>Decoded UTF-16 prose.</p></main></body></html>";
    let mut utf16_bytes = vec![0xFFu8, 0xFEu8];
    for unit in utf16_html.encode_utf16() {
        utf16_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let utf16_out = run_with_stdin(
        &["--json", "extract", "--url", "https://docs.example.com/u16"],
        &utf16_bytes,
        unwritable_home,
    );
    assert_eq!(
        utf16_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&utf16_out.stderr)
    );
    let u16_json: serde_json::Value = serde_json::from_slice(&utf16_out.stdout).unwrap();
    assert_eq!(u16_json["title"], "UTF16 Doc");
    assert!(u16_json["content"]
        .as_str()
        .unwrap()
        .contains("Decoded UTF-16 prose."));
}
