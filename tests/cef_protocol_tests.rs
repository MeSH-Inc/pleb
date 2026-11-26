use mesh_wick::context::{RuntimeConfig, RuntimeContext};
use mesh_wick::fetch::{fetch_document, FetchError, FetchOptions};
use mesh_wick::policy::RenderMode;
use mesh_wick::transport::cef::{
    probe_renderer_binary, probe_renderer_in, CefProtocolError, CefRenderOptions, CefRenderer,
    RendererProbe, EXPECTED_PROTOCOL,
};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_state_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "mesh-wick-cef-proto-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    fs::create_dir_all(dir.join("cef")).unwrap();
    dir
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
    let state_dir = temp_state_dir("mock");
    let custom_cache = state_dir.join("custom-cache");
    let renderer_bin = state_dir.join("cef").join("wick-renderer");
    let log_file = state_dir.join("requests.log");

    let script = format!(
        r#"#!/usr/bin/env python3
# WICK_RENDERER_PROTOCOL=v4-meta
import json, sys

args_log = {log_path:?}
with open(args_log, "a") as f:
    f.write("ARGS=" + json.dumps(sys.argv[1:]) + "\n")

sys.stdout.write(json.dumps({{"ready": True, "protocol": "v4-meta", "error": None}}) + "\n")
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
        hdr = {{"status_code": 0, "final_url": url, "html_bytes": 0, "error": "load error -105"}}
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
        probe_renderer_in(&state_dir),
        RendererProbe::Ready(renderer_bin.clone())
    );

    // Verify direct CefRenderer passes --cache-dir and preserves process across logical errors.
    let renderer =
        CefRenderer::new_direct(state_dir.clone(), None, false).with_cache_dir(custom_cache.clone());
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

    // Logical renderer error leaves the warm renderer process alive for subsequent requests.
    let err = renderer
        .render("https://mock.example/load-fail", &opts, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("load error -105"));
    assert!(renderer.has_live_process());

    let recovered = renderer
        .render("https://mock.example/ok", &opts, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(recovered.status_code, 200);
    assert!(recovered.html.contains("Recovered after error"));

    // Verify fetch_document propagates real 404 from CEF and does not blindly double-render <2KB pages.
    let ctx = RuntimeContext::new(RuntimeConfig {
        state_dir: state_dir.clone(),
        timeout: Duration::from_secs(5),
        cef_timeout: Duration::from_secs(5),
        cef_daemon: false,
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
    let _ = fs::remove_dir_all(&state_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn outdated_v3_renderer_fails_loudly_with_rebuild_hint() {
    let state_dir = temp_state_dir("stale");
    let renderer_bin = state_dir.join("cef").join("wick-renderer");
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

    let renderer = CefRenderer::new_direct(state_dir.clone(), None, false);
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
    assert!(msg.contains("v4-meta"));
    assert!(msg.contains("install-cef-"));

    let _ = fs::remove_dir_all(&state_dir);
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
    assert(strstr(js, "try{hit=!!document.querySelector(sel);}") != NULL);
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
    wick_write_response(tmp, 200, "https://example.com/a\"b", "<b>hi</b>", 9, NULL);
    rewind(tmp);

    char* rline = NULL;
    int too_long = 0;
    assert(wick_read_line(tmp, &rline, WICK_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"ready\":true") != NULL);
    free(rline);

    assert(wick_read_line(tmp, &rline, WICK_MAX_REQUEST_LINE_BYTES, &too_long) == 1);
    assert(too_long == 0 && strstr(rline, "\"html_bytes\":9") != NULL && strstr(rline, "a\\\"b") != NULL);
    free(rline);
    fclose(tmp);

    return 0;
}
"#;
    let dir = temp_state_dir("c-proto");
    let src = dir.join("test_proto.c");
    let bin = dir.join("test_proto");
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

    let _ = fs::remove_dir_all(&dir);
}
