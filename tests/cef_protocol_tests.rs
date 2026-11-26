use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::fetch::{fetch_document, FetchError, FetchOptions};
use mesh_wick::policy::RenderMode;
use mesh_wick::transport::cef::{
    find_renderer_in, probe_renderer_binary, probe_renderer_in, CefProtocolError, CefRenderOptions,
    CefRenderer, CefTimeoutError, RendererProbe, EXPECTED_PROTOCOL,
};
use mesh_wick::transport::{TransportError, TransportErrorKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

struct TempStateDir(PathBuf);

impl TempStateDir {
    fn new() -> Self {
        let seq = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/wick-t-{}-{}", std::process::id(), seq));
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
    let renderer_bin = state_dir.path().join("cef").join("wick-renderer");
    let log_file = state_dir.path().join("requests.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# WICK_RENDERER_PROTOCOL=v5-meta
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
    let renderer_bin = state_dir.path().join("cef").join("wick-renderer");
    write_executable(
        &renderer_bin,
        "#!/bin/sh\n# WICK_RENDERER_PROTOCOL=v3-cf-aware\nexit 1\n",
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
    if std::env::var("WICK_NO_HOME_CEF").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")) {
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
        "#!/bin/sh\n# WICK_RENDERER_PROTOCOL=v5-meta\nexit 0\n",
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
    WickRenderRequest req = {0};
    char err[256] = {0};

    // Surrogate pair (U+1F980 crab) + single quote + backslash in selector
    const char* line = "{\"url\":\"https://example.com/a?x=1&y=2\",\"wait_for_selector\":\"[data-x='a\\\\b'] \\uD83E\\uDD80\",\"timeout_ms\":12000}";
    int rc = wick_parse_request_line(line, &req, err, sizeof(err));
    assert(rc == 0);
    assert(strcmp(req.url, "https://example.com/a?x=1&y=2") == 0);
    assert(req.timeout_ms == 12000);
    assert(strstr(req.wait_for_selector, "[data-x='a\\b']") != NULL);
    assert(strstr(req.wait_for_selector, "\xF0\x9F\xA6\x80") != NULL);

    char* js = wick_build_poller_js(req.wait_for_selector, "MARK", "SERR", 5000, 9000);
    assert(js != NULL);
    assert(strstr(js, "\\'a\\\\b\\'") != NULL);
    assert(strstr(js, "try{el=document.querySelector(sel);}") != NULL);
    assert(strstr(js, "window.__wickPollerActive") != NULL);
    assert(strstr(js, "MutationObserver") != NULL);
    assert(strstr(js, "requestAnimationFrame") != NULL);
    assert(strstr(js, "hasEmptySpaRoot") != NULL);
    assert(strstr(js, "window.__wickInflight") != NULL);
    assert(strstr(js, "len>500") == NULL);
    free(js);
    wick_render_request_free(&req);

    // Timeout clamping leaves headroom below caller deadline.
    int total = 0, post = 0, js_cap = 0, cf_cap = 0;
    wick_clamp_timeouts(5000, 1, &total, &post, &js_cap, &cf_cap);
    assert(total == 3500);
    assert(post == 3500);
    assert(js_cap == 3500);
    assert(cf_cap == 3500);

    // Unpaired surrogate must fail loudly.
    assert(wick_parse_request_line("{\"url\":\"https://x.com\",\"wait_for_selector\":\"\\uD800\"}", &req, err, sizeof(err)) != 0);

    // Exercise timezone helper and stdio line reader / ready+response writers.
    assert(wick_build_timezone_js(NULL) == NULL);
    assert(wick_build_timezone_js("") == NULL);
    char* tz_js = wick_build_timezone_js("America/New_York");
    assert(tz_js != NULL && strstr(tz_js, "America/New_York") != NULL);
    free(tz_js);

    FILE* tmp = tmpfile();
    assert(tmp != NULL);
    wick_write_ready(tmp, 1, NULL);
    wick_write_response(tmp, 200, "https://example.com/a\"b", "<b>hi</b>", 9, NULL, NULL);
    wick_write_response(tmp, 0, "https://example.com/err", NULL, 0, "load error -105", "load_error");
    rewind(tmp);

    char* rline = NULL;
    int too_long = 0;
    assert(wick_read_line(tmp, &rline, WICK_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"ready\":true") != NULL);
    free(rline);

    assert(wick_read_line(tmp, &rline, WICK_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"html_bytes\":9") != NULL && strstr(rline, "a\\\"b") != NULL);
    free(rline);

    // Consume the 9 raw HTML bytes before reading the error response line.
    char raw_html[10] = {0};
    assert(fread(raw_html, 1, 9, tmp) == 9);
    assert(strcmp(raw_html, "<b>hi</b>") == 0);

    assert(wick_read_line(tmp, &rline, WICK_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
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
    let state_renderer = cef_dir.join("wick-renderer");
    fs::write(
        &state_renderer,
        b"#!/bin/sh\n# WICK_RENDERER_PROTOCOL=v5-meta\n",
    )
    .unwrap();

    // Nonexistent explicit override must NOT silently fall back to state_dir/cef/wick-renderer.
    let missing = dir.path().join("does-not-exist-renderer");
    assert!(
        mesh_wick::transport::cef::find_renderer_with_override(dir.path(), Some(&missing))
            .is_none(),
        "explicit missing renderer override must not fall back to state_dir"
    );
    assert!(matches!(
        mesh_wick::transport::cef::probe_renderer_with_override(dir.path(), Some(&missing)),
        RendererProbe::NotInstalled
    ));

    // Empty path override falls through cleanly to state_dir/cef/wick-renderer.
    assert_eq!(
        mesh_wick::transport::cef::find_renderer_with_override(
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
    char* js = wick_build_poller_js(NULL, "MARK:", "SERR:", 4000, 8000);
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
        sb.window.__wickInflight = 0;
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
        sb.window.__wickInflight = 0;
        setTimeout(() => {{
            rootEl.innerText = 'Hydrated SPA article content with more than fifty characters of prose.';
            sb.document.body.innerText = rootEl.innerText;
            sb.document.documentElement.outerHTML = '<html><body><div id="root"><p>Hydrated</p></div></body></html>';
        }}, 160);
    }});
    if (spaRes.logs.length !== 1 || spaRes.elapsed < 300 || !spaRes.logs[0].msg.includes('Hydrated')) {{
        throw new Error('empty SPA root failed hydration wait: ' + JSON.stringify(spaRes));
    }}

    // 3. Active inflight subresources block settle until __wickInflight reaches 0.
    const inflightRes = await runScenario((sb) => {{
        sb.document.body.innerText = 'Ready text';
        sb.document.documentElement.outerHTML = '<html><body>Ready text</body></html>';
        sb.window.__wickInflight = 2;
        setTimeout(() => {{ sb.window.__wickInflight = 0; }}, 160);
    }});
    if (inflightRes.logs.length !== 1 || inflightRes.elapsed < 300) {{
        throw new Error('inflight subresources did not gate settle: ' + JSON.stringify(inflightRes));
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
