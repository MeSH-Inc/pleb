use pleb::context::{RuntimeConfig, RuntimeContext};
use pleb::fetch::{fetch_document, FetchError, FetchOptions};
use pleb::policy::RenderMode;
use pleb::transport::cef::{
    find_renderer_in, probe_renderer_binary, probe_renderer_in, CefProtocolError, CefRenderOptions,
    CefRenderer, CefTimeoutError, RendererProbe, EXPECTED_PROTOCOL,
};
use pleb::transport::{TransportError, TransportErrorKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempStateDir(PathBuf);

impl TempStateDir {
    fn new() -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/pleb-t-{}-{}", std::process::id(), seq));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }

    fn with_cef_dir() -> Self {
        let dir = Self::new();
        fs::create_dir_all(dir.0.join("cef")).unwrap();
        dir
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl Drop for TempStateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn v4_meta_mock_renderer_handles_metadata_selectors_and_errors() {
    let state_dir = TempStateDir::with_cef_dir();
    let custom_cache = state_dir.path().join("custom-cache");
    let renderer_bin = state_dir.path().join("cef").join("pleb-renderer");
    let log_file = state_dir.path().join("requests.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# PLEB_RENDERER_PROTOCOL=v5-meta
import json, sys

args_log = {log_path:?}
with open(args_log, "a") as f:
    f.write("ARGS=" + json.dumps(sys.argv[1:]) + "\n")

sys.stdout.write(json.dumps({{"ready": True, "protocol": "v5-meta", "error": None}}) + "\n")
sys.stdout.flush()

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        break
    req = json.loads(line)
    with open(args_log, "a") as f:
        f.write("REQ=" + json.dumps(req) + "\n")

    url = req["url"]
    if url.endswith("/short"):
        html = "<html><head><title>Short</title></head><body><main><p>Tiny ok</p></main></body></html>".encode("utf-8")
        hdr = {{"status_code": 200, "final_url": "https://final.example/short-landed", "html_bytes": len(html), "error": None}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
        sys.stdout.buffer.write(html)
        sys.stdout.buffer.flush()
    elif url.endswith("/missing"):
        html = "<html><body><h1>Not Found</h1></body></html>".encode("utf-8")
        hdr = {{"status_code": 404, "final_url": "https://missing.example/404", "html_bytes": len(html), "error": None}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
        sys.stdout.buffer.write(html)
        sys.stdout.buffer.flush()
    elif url.endswith("/load-fail"):
        hdr = {{"status_code": 0, "final_url": url, "html_bytes": 0, "error": "load error -105", "error_kind": "load_error"}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
    elif url.endswith("/bad-selector"):
        hdr = {{"status_code": 200, "final_url": url, "html_bytes": 0, "error": "invalid selector: [unclosed", "error_kind": "invalid_selector"}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
    elif url.endswith("/timeout-kind"):
        hdr = {{"status_code": 0, "final_url": url, "html_bytes": 0, "error": "renderer budget exhausted", "error_kind": "timeout"}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
    else:
        html = "<html><body><p>Recovered after error</p></body></html>".encode("utf-8")
        hdr = {{"status_code": 200, "final_url": url, "html_bytes": len(html), "error": None}}
        sys.stdout.write(json.dumps(hdr) + "\n")
        sys.stdout.flush()
        sys.stdout.buffer.write(html)
        sys.stdout.buffer.flush()
"#,
        log_path = log_file.display().to_string()
    );
    write_executable(&renderer_bin, &script);

    assert_eq!(
        probe_renderer_in(state_dir.path()),
        RendererProbe::Ready(renderer_bin.clone())
    );

    // Verify direct CefRenderer passes --cache-dir and preserves process across logical errors.
    let renderer = CefRenderer::new_direct(state_dir.to_path_buf(), None, false)
        .with_cache_dir(custom_cache.clone());
    let opts = CefRenderOptions {
        wait_for_selector: Some("[data-testid='tweet']".to_string()),
        ..Default::default()
    };

    let short = renderer
        .render("https://mock.example/short", &opts, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(short.status_code, 200);
    assert_eq!(short.final_url, "https://final.example/short-landed");
    assert!(short.html.contains("Tiny ok"));

    // Logical renderer error leaves the warm renderer process alive and preserves error_kind.
    let err = renderer
        .render(
            "https://mock.example/load-fail",
            &opts,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    let load_te = err
        .downcast_ref::<TransportError>()
        .expect("load error must carry structured TransportError");
    assert_eq!(load_te.kind, TransportErrorKind::LoadError);
    assert!(renderer.has_live_process());

    let sel_err = renderer
        .render(
            "https://mock.example/bad-selector",
            &opts,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    let sel_te = sel_err
        .downcast_ref::<TransportError>()
        .expect("invalid selector error must carry structured TransportError");
    assert_eq!(sel_te.kind, TransportErrorKind::InvalidSelector);
    assert!(renderer.has_live_process());

    // Timeout error_kind maps to CefTimeoutError regardless of message wording.
    let timeout_err = renderer
        .render(
            "https://mock.example/timeout-kind",
            &opts,
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(
        timeout_err.is::<CefTimeoutError>(),
        "expected CefTimeoutError from error_kind=timeout, got: {timeout_err}"
    );
    assert!(renderer.has_live_process());

    let recovered = renderer
        .render("https://mock.example/ok", &opts, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(recovered.status_code, 200);
    assert!(recovered.html.contains("Recovered after error"));

    // Verify fetch_document propagates real 404 from CEF and does not blindly double-render <2KB pages.
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.to_path_buf(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        use_daemon: false,
        ..Default::default()
    })
    .unwrap();

    let cef_opts = FetchOptions {
        render: RenderMode::Cef,
        wait_for_selector: Some("[data-testid='tweet']".to_string()),
        ..Default::default()
    };

    let doc = fetch_document(&ctx, "https://mock.example/short", &cef_opts)
        .await
        .unwrap();
    assert_eq!(doc.status_code, 200);
    assert_eq!(doc.final_url, "https://final.example/short-landed");

    let err_404 = fetch_document(&ctx, "https://mock.example/missing", &cef_opts)
        .await
        .unwrap_err();
    assert!(matches!(err_404, FetchError::HttpError { status: 404, .. }));
    assert_eq!(err_404.exit_code(), 3);
    // 404 error must not overwrite "cef" with "cef_timeout", nor record a fresh host.
    assert_eq!(
        ctx.site_cache.get("mock.example").map(|e| e.strategy),
        Some("cef".to_string())
    );
    let err_404_fresh = fetch_document(&ctx, "https://missing-only.example/missing", &cef_opts)
        .await
        .unwrap_err();
    assert_eq!(err_404_fresh.exit_code(), 3);
    assert!(ctx.site_cache.get("missing-only.example").is_none());

    let logged = fs::read_to_string(&log_file).unwrap();
    assert!(logged.contains(&format!("--cache-dir={}", custom_cache.display())));
    assert!(logged.contains(r#""wait_for_selector": "[data-testid='tweet']""#));
    // Short valid page (<2KB) was rendered once per fetch_document, not blindly retried twice.
    assert_eq!(logged.matches("https://mock.example/short").count(), 2);

    drop(renderer);
    drop(ctx);
}

#[cfg(unix)]
#[tokio::test]
async fn outdated_v3_renderer_fails_loudly_with_rebuild_hint() {
    let state_dir = TempStateDir::with_cef_dir();
    let renderer_bin = state_dir.path().join("cef").join("pleb-renderer");
    write_executable(
        &renderer_bin,
        "#!/bin/sh\n# PLEB_RENDERER_PROTOCOL=v3-cf-aware\nexit 1\n",
    );

    assert_eq!(
        probe_renderer_binary(&renderer_bin),
        RendererProbe::ProtocolMismatch {
            path: renderer_bin.clone(),
            found: Some("v3-cf-aware".to_string()),
            expected: EXPECTED_PROTOCOL,
        }
    );

    let renderer = CefRenderer::new_direct(state_dir.to_path_buf(), None, false);
    let err = renderer
        .render(
            "https://example.com",
            &CefRenderOptions::default(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
    assert!(err.is::<CefProtocolError>());
    let msg = err.to_string();
    assert!(msg.contains("v3-cf-aware"));
    assert!(msg.contains("v5-meta"));
    assert!(msg.contains("install-cef-"));
}

#[cfg(unix)]
#[test]
fn renderer_override_and_no_home_cef_isolation() {
    let empty_state = TempStateDir::new();
    if std::env::var("PLEB_NO_HOME_CEF").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
        assert_eq!(find_renderer_in(empty_state.path()), None);
        assert_eq!(
            probe_renderer_in(empty_state.path()),
            RendererProbe::NotInstalled
        );
    }

    let custom_dir = TempStateDir::new();
    let custom_bin = custom_dir.path().join("custom-renderer");
    write_executable(
        &custom_bin,
        "#!/bin/sh\n# PLEB_RENDERER_PROTOCOL=v5-meta\nexit 0\n",
    );

    let renderer = CefRenderer::new_direct(empty_state.to_path_buf(), None, false)
        .with_renderer_executable(Some(custom_bin.clone()));
    assert_eq!(renderer.renderer_path(), Some(custom_bin.clone()));
    assert_eq!(renderer.probe(), RendererProbe::Ready(custom_bin));
}

#[test]
fn c_protocol_header_parses_escapes_surrogates_and_clamps_timeouts() {
    let c_harness = r#"
#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "protocol.h"

int main(void) {
    PlebRenderRequest req = {0};
    char err[256] = {0};

    // Surrogate pair (U+1F980 crab) + single quote + backslash in selector
    const char* line = "{\"url\":\"https://example.com/a?x=1&y=2\",\"wait_for_selector\":\"[data-x='a\\\\b'] \\uD83E\\uDD80\",\"timeout_ms\":12000}";
    int rc = pleb_parse_request_line(line, &req, err, sizeof(err));
    assert(rc == 0);
    assert(strcmp(req.url, "https://example.com/a?x=1&y=2") == 0);
    assert(req.timeout_ms == 12000);
    assert(strstr(req.wait_for_selector, "[data-x='a\\b']") != NULL);
    assert(strstr(req.wait_for_selector, "\xF0\x9F\xA6\x80") != NULL);

    char* js = pleb_build_poller_js(req.wait_for_selector, "MARK", "SERR", 5000);
    assert(js != NULL);
    assert(strstr(js, "\\'a\\\\b\\'") != NULL);
    assert(strstr(js, "try{el=document.querySelector(sel);}") != NULL);
    assert(strstr(js, "window.__plebPollerActive") != NULL);
    assert(strstr(js, "MutationObserver") != NULL);
    assert(strstr(js, "requestAnimationFrame") != NULL);
    assert(strstr(js, "hasEmptySpaRoot") != NULL);
    assert(strstr(js, "window.__plebInflight") != NULL);
    assert(strstr(js, "len>500") == NULL);
    free(js);
    pleb_render_request_free(&req);

    // Timeout clamping leaves headroom below caller deadline.
    int total = 0, post = 0, js_cap = 0;
    pleb_clamp_timeouts(5000, 1, &total, &post, &js_cap);
    assert(total == 3500);
    assert(post == 3500);
    assert(js_cap == 3500);

    // Unpaired surrogate must fail loudly.
    assert(pleb_parse_request_line("{\"url\":\"https://x.com\",\"wait_for_selector\":\"\\uD800\"}", &req, err, sizeof(err)) != 0);

    // Exercise timezone helper and stdio line reader / ready+response writers.
    assert(pleb_build_timezone_js(NULL) == NULL);
    assert(pleb_build_timezone_js("") == NULL);
    char* tz_js = pleb_build_timezone_js("America/New_York");
    assert(tz_js != NULL && strstr(tz_js, "America/New_York") != NULL);
    free(tz_js);

    FILE* tmp = tmpfile();
    assert(tmp != NULL);
    pleb_write_ready(tmp, 1, NULL);
    pleb_write_response(tmp, 200, "https://example.com/a\"b", "<b>hi</b>", 9, NULL, NULL);
    pleb_write_response(tmp, 0, "https://example.com/err", NULL, 0, "load error -105", "load_error");
    rewind(tmp);

    char* rline = NULL;
    int too_long = 0;
    assert(pleb_read_line(tmp, &rline, PLEB_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"ready\":true") != NULL);
    free(rline);

    assert(pleb_read_line(tmp, &rline, PLEB_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"html_bytes\":9") != NULL && strstr(rline, "a\\\"b") != NULL);
    free(rline);

    // Consume the 9 raw HTML bytes before reading the error response line.
    char raw_html[10] = {0};
    assert(fread(raw_html, 1, 9, tmp) == 9);
    assert(strcmp(raw_html, "<b>hi</b>") == 0);

    assert(pleb_read_line(tmp, &rline, PLEB_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"error_kind\":\"load_error\"") != NULL);
    free(rline);
    fclose(tmp);

    return 0;
}
"#;
    let dir = TempStateDir::new();
    let src = dir.path().join("test_proto.c");
    let bin = dir.path().join("test_proto");
    fs::write(&src, c_harness).unwrap();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cef_inc = manifest_dir.join("cef");
    let status = std::process::Command::new("cc")
        .arg("-std=c99")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg(format!("-I{}", cef_inc.display()))
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .status()
        .expect("cc compiler must be available");
    assert!(status.success(), "C protocol harness failed to compile");

    let run = std::process::Command::new(&bin)
        .status()
        .expect("failed to execute C protocol harness");
    assert!(run.success(), "C protocol harness assertions failed");
}

#[test]
fn adversarial_explicit_missing_renderer_override_does_not_fallback_to_state_dir() {
    let dir = TempStateDir::new();
    let cef_dir = dir.path().join("cef");
    fs::create_dir_all(&cef_dir).unwrap();
    let state_renderer = cef_dir.join("pleb-renderer");
    fs::write(
        &state_renderer,
        b"#!/bin/sh\n# PLEB_RENDERER_PROTOCOL=v5-meta\n",
    )
    .unwrap();

    // Nonexistent explicit override must NOT silently fall back to state_dir/cef/pleb-renderer.
    let missing = dir.path().join("does-not-exist-renderer");
    assert!(
        pleb::transport::cef::find_renderer_with_override(dir.path(), Some(&missing)).is_none(),
        "explicit missing renderer override must not fall back to state_dir"
    );
    assert!(matches!(
        pleb::transport::cef::probe_renderer_with_override(dir.path(), Some(&missing)),
        RendererProbe::NotInstalled
    ));

    // Empty path override falls through cleanly to state_dir/cef/pleb-renderer.
    assert_eq!(
        pleb::transport::cef::find_renderer_with_override(
            dir.path(),
            Some(std::path::Path::new(""))
        ),
        Some(state_renderer)
    );
}

#[test]
fn v5_poller_js_short_page_settles_fast_and_waits_for_empty_spa_root() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }

    let c_emitter = r#"
#include <stdio.h>
#include <stdlib.h>
#include "protocol.h"

int main(void) {
    char* js = pleb_build_poller_js(NULL, "MARK:", "SERR:", 4000);
    if (!js) return 1;
    fputs(js, stdout);
    free(js);
    return 0;
}
"#;
    let dir = TempStateDir::new();
    let src = dir.path().join("emit_poller.c");
    let bin = dir.path().join("emit_poller");
    fs::write(&src, c_emitter).unwrap();

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cef_inc = manifest_dir.join("cef");
    let compile = std::process::Command::new("cc")
        .arg("-std=c99")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Wno-unused-function")
        .arg("-Werror")
        .arg(format!("-I{}", cef_inc.display()))
        .arg(&src)
        .arg("-o")
        .arg(&bin)
        .status()
        .expect("cc compiler must be available");
    assert!(compile.success());

    let emitted = std::process::Command::new(&bin)
        .output()
        .expect("failed to execute poller emitter");
    assert!(emitted.status.success());
    let poller_js = String::from_utf8(emitted.stdout).unwrap();

    let node_harness = format!(
        r#"
const vm = require('vm');
const pollerCode = {poller_json};

function runScenario(setupFn) {{
    return new Promise((resolve, reject) => {{
        const logs = [];
        const started = Date.now();
        const sandbox = {{
            window: {{}},
            document: {{
                body: {{ innerText: '' }},
                documentElement: {{ outerHTML: '<html></html>' }},
                scripts: [],
                querySelector: () => null,
            }},
            MutationObserver: class {{
                constructor(cb) {{ this.cb = cb; }}
                observe() {{}}
                disconnect() {{}}
            }},
            requestAnimationFrame: (fn) => setTimeout(fn, 10),
            setInterval,
            clearInterval,
            setTimeout,
            Date,
            console: {{
                log: (msg) => {{
                    logs.push({{ msg: String(msg), elapsed: Date.now() - started }});
                }},
            }},
        }};
        setupFn(sandbox, () => logs);
        vm.runInNewContext(pollerCode, sandbox);
        const wait = setInterval(() => {{
            if (logs.length > 0) {{
                clearInterval(wait);
                setTimeout(() => resolve({{ logs, elapsed: logs[0].elapsed }}), 60);
            }} else if (Date.now() - started > 3000) {{
                clearInterval(wait);
                reject(new Error('poller timed out'));
            }}
        }}, 20);
    }});
}}

(async () => {{
    // 1. Short static page (<= 500 chars) must settle quickly (< 350ms, well below 1500ms floor).
    const shortRes = await runScenario((sb) => {{
        sb.document.body.innerText = 'Hello short page';
        sb.document.documentElement.outerHTML = '<html><body><p>Hello short page</p></body></html>';
        sb.window.__plebInflight = 0;
    }});
    if (shortRes.logs.length !== 1 || shortRes.elapsed >= 350) {{
        throw new Error('short page failed fast settle: ' + JSON.stringify(shortRes));
    }}

    // 2. Empty SPA root (#root with < 50 chars) must wait until hydrated before settling.
    const spaRes = await runScenario((sb) => {{
        const rootEl = {{ innerText: '' }};
        sb.document.body.innerText = '';
        sb.document.documentElement.outerHTML = '<html><body><div id="root"></div></body></html>';
        sb.document.querySelector = (s) => s.includes('#root') ? rootEl : null;
        sb.window.__plebInflight = 0;
        setTimeout(() => {{
            rootEl.innerText = 'Hydrated SPA article content with more than fifty characters of prose.';
            sb.document.body.innerText = rootEl.innerText;
            sb.document.documentElement.outerHTML = '<html><body><div id="root"><p>Hydrated</p></div></body></html>';
        }}, 160);
    }});
    if (spaRes.logs.length !== 1 || spaRes.elapsed < 300 || !spaRes.logs[0].msg.includes('Hydrated')) {{
        throw new Error('empty SPA root failed hydration wait: ' + JSON.stringify(spaRes));
    }}

    // 3. Active inflight subresources block settle until __plebInflight reaches 0.
    const inflightRes = await runScenario((sb) => {{
        sb.document.body.innerText = 'Ready text';
        sb.document.documentElement.outerHTML = '<html><body>Ready text</body></html>';
        sb.window.__plebInflight = 2;
        setTimeout(() => {{ sb.window.__plebInflight = 0; }}, 160);
    }});
    if (inflightRes.logs.length !== 1 || inflightRes.elapsed < 300) {{
        throw new Error('inflight subresources did not gate settle: ' + JSON.stringify(inflightRes));
    }}

    // 4. Active bot challenge interstitial dumps immediately instead of waiting out a challenge timer.
    const challengeRes = await runScenario((sb) => {{
        sb.document.body.innerText = 'Just a moment... Checking your browser before accessing';
        sb.document.documentElement.outerHTML = '<html><body>Checking your browser</body></html>';
        sb.window.__plebInflight = 1;
    }});
    if (challengeRes.logs.length !== 1 || challengeRes.elapsed >= 200) {{
        throw new Error('challenge interstitial did not fail fast: ' + JSON.stringify(challengeRes));
    }}
}})().catch((err) => {{
    console.error(err);
    process.exit(1);
}});
"#,
        poller_json = serde_json::to_string(&poller_js).unwrap()
    );

    let script_path = dir.path().join("verify_poller.js");
    fs::write(&script_path, node_harness).unwrap();
    let out = std::process::Command::new("node")
        .arg(&script_path)
        .output()
        .expect("failed to run node poller harness");
    assert!(
        out.status.success(),
        "poller JS behavioral harness failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn extract_quoted_assignment(src: &str, key: &str) -> String {
    let prefix = format!("{key}=\"");
    src.lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix(&prefix)?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .unwrap_or_else(|| panic!("missing {key}=\"...\" assignment"))
}

#[test]
fn chromium_cronet_cef_version_and_keyring_parity() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let transport_src = fs::read_to_string(root.join("src/transport/mod.rs")).unwrap();
    let chrome_major: u32 = transport_src
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("const CHROME_MAJOR: usize = ")?;
            rest.strip_suffix(';')?.trim().parse().ok()
        })
        .expect("missing CHROME_MAJOR in src/transport/mod.rs");
    let chrome_full = transport_src
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("const CHROME_FULL: &str = \"")?;
            let end = rest.find('"')?;
            Some(rest[..end].to_string())
        })
        .expect("missing CHROME_FULL in src/transport/mod.rs");
    assert!(
        chrome_full.starts_with(&format!("{chrome_major}.0.")),
        "CHROME_FULL ({chrome_full}) must start with {chrome_major}.0."
    );

    let cronet_mac = fs::read_to_string(root.join("scripts/download-cronet-macos.sh")).unwrap();
    let cronet_linux = fs::read_to_string(root.join("scripts/download-cronet-linux.sh")).unwrap();
    let mac_commit = extract_quoted_assignment(&cronet_mac, "COMMIT");
    let linux_commit = extract_quoted_assignment(&cronet_linux, "COMMIT");
    assert_eq!(
        mac_commit, linux_commit,
        "macOS and Linux Cronet scripts must pin the same SagerNet/cronet-go commit"
    );
    assert!(
        cronet_mac.contains(&chrome_full) && cronet_linux.contains(&chrome_full),
        "Cronet download scripts must stay aligned with CHROME_FULL ({chrome_full})"
    );

    let cef_mac = fs::read_to_string(root.join("scripts/install-cef-mac.sh")).unwrap();
    let cef_linux = fs::read_to_string(root.join("scripts/install-cef-linux.sh")).unwrap();
    let cef_helpers = fs::read_to_string(root.join("cef/setup-helpers.sh")).unwrap();
    let mac_cef_ver = extract_quoted_assignment(&cef_mac, "CEF_VERSION");
    let linux_cef_ver = extract_quoted_assignment(&cef_linux, "CEF_VERSION");
    assert_eq!(
        mac_cef_ver, linux_cef_ver,
        "macOS and Linux CEF installers must pin the same CEF_VERSION"
    );
    assert!(
        mac_cef_ver.contains(&format!("+chromium-{chrome_major}.0.")),
        "CEF_VERSION ({mac_cef_ver}) must match CHROME_MAJOR ({chrome_major})"
    );

    let expected_api = (chrome_major * 100).to_string();
    assert_eq!(
        extract_quoted_assignment(&cef_mac, "CEF_API_VERSION"),
        expected_api
    );
    assert_eq!(
        extract_quoted_assignment(&cef_linux, "CEF_API_VERSION"),
        expected_api
    );
    assert!(
        cef_linux.contains("CEF_PLATFORM=\"linux64\"")
            && cef_linux.contains("CEF_PLATFORM=\"linuxarm64\""),
        "scripts/install-cef-linux.sh must support both linux64 (x86_64) and linuxarm64 (aarch64)"
    );
    assert!(
        cef_helpers.contains(&format!("CEF_API_VERSION=\"${{3:-{expected_api}}}\"")),
        "cef/setup-helpers.sh default CEF_API_VERSION must equal {expected_api}"
    );

    let renderer_mac = fs::read_to_string(root.join("cef/renderer.m")).unwrap();
    let renderer_linux = fs::read_to_string(root.join("cef/renderer_linux.c")).unwrap();
    let renderer_core = fs::read_to_string(root.join("cef/renderer_core.h")).unwrap();
    let helper_core = fs::read_to_string(root.join("cef/helper_core.h")).unwrap();
    assert!(
        renderer_mac.contains("--use-mock-keychain")
            && renderer_mac.contains("--password-store=basic")
            && renderer_mac.contains("cef_initialize(&main_args, &settings, &g_app, NULL)"),
        "cef/renderer.m must pass --use-mock-keychain, --password-store=basic, and &g_app to prevent headless macOS Keychain hangs"
    );
    assert!(
        renderer_linux.contains("--password-store=basic")
            && renderer_linux.contains("cef_initialize(&main_args, &settings, &g_app, NULL)"),
        "cef/renderer_linux.c must pass --password-store=basic and &g_app to prevent headless keyring hangs"
    );
    assert!(
        renderer_core.contains("on_before_command_line_processing")
            && renderer_core.contains("on_before_child_process_launch")
            && renderer_core.contains("use-mock-keychain")
            && renderer_core.contains("password-store"),
        "cef/renderer_core.h must enforce keyring isolation across browser and child processes"
    );
    assert!(
        helper_core.contains("on_before_command_line_processing")
            && helper_core.contains("use-mock-keychain")
            && helper_core.contains("password-store"),
        "cef/helper_core.h must enforce keyring isolation in CEF helper subprocesses"
    );
    assert!(
        renderer_linux.contains("display_env[0] != '\\0'"),
        "cef/renderer_linux.c must treat empty DISPLAY=\"\" as headless (Ozone headless)"
    );
    let cef_rs = fs::read_to_string(root.join("src/transport/cef.rs")).unwrap();
    assert!(
        !cef_rs.contains("Command::new(\"Xvfb\")") && !cef_rs.contains("cmd.env(\"DISPLAY\", \":99\")"),
        "src/transport/cef.rs must not unconditionally inject DISPLAY=:99 / Xvfb and break Ozone headless"
    );
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires installed v5-meta CEF renderer; run explicitly via --ignored"]
async fn live_cef_renderer_cold_start_static_and_spa_hydration() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let renderer_path = std::env::var_os("PLEB_CEF_RENDERER")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            find_renderer_in(&home.join(".pleb"))
        })
        .expect(
            "installed v5-meta CEF renderer not found; build with `bash scripts/install-cef-mac.sh --force` or set PLEB_CEF_RENDERER",
        );
    assert_eq!(
        probe_renderer_binary(&renderer_path),
        RendererProbe::Ready(renderer_path.clone()),
        "installed renderer at {} must match {EXPECTED_PROTOCOL}",
        renderer_path.display()
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
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
                let body: &[u8] = match path {
                    "/static" => b"<!DOCTYPE html><html><head><title>Local Static</title></head><body><main><h1>Local Static</h1><p>Static CEF render verified.</p></main></body></html>",
                    "/spa" => b"<!DOCTYPE html><html><head><title>Local SPA</title></head><body><div id=\"root\"></div><script>setTimeout(function(){document.getElementById('root').innerHTML='<main><article data-ready=\"1\"><h1>Hydrated SPA</h1><p>Client-rendered content loaded asynchronously via script for verification.</p></article></main>';},150);</script></body></html>",
                    _ => b"<!DOCTYPE html><html><body>not found</body></html>",
                };
                let status = if matches!(path, "/static" | "/spa") {
                    "200 OK"
                } else {
                    "404 Not Found"
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.write_all(body).await;
            });
        }
    });

    // Allocate under /tmp to also exercise macOS /tmp -> /private/tmp cache path canonicalization.
    let state_dir = TempStateDir::new();
    let cache_dir = state_dir.path().join("cef-cache");
    fs::create_dir_all(&cache_dir).unwrap();

    let renderer = CefRenderer::new_direct(state_dir.to_path_buf(), None, false)
        .with_cache_dir(cache_dir)
        .with_renderer_executable(Some(renderer_path));

    let static_url = format!("http://{addr}/static");
    let static_res = renderer
        .render(
            &static_url,
            &CefRenderOptions::default(),
            Duration::from_secs(15),
        )
        .await
        .expect("cold-start static CEF render should succeed without keyring deadlock");
    assert_eq!(static_res.status_code, 200);
    assert!(static_res.html.contains("Static CEF render verified."));

    let spa_url = format!("http://{addr}/spa");
    let spa_res = renderer
        .render(
            &spa_url,
            &CefRenderOptions {
                wait_for_selector: Some("article[data-ready=\"1\"]".to_string()),
                ..Default::default()
            },
            Duration::from_secs(15),
        )
        .await
        .expect("JS-hydrated SPA render with selector wait should succeed");
    assert_eq!(spa_res.status_code, 200);
    assert!(spa_res
        .html
        .contains("Client-rendered content loaded asynchronously"));

    drop(renderer);
    server.abort();
}
