// wick-renderer (macOS): CEF offscreen renderer with v4-meta stdin/stdout JSON+bytes framing.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <libgen.h>
#include <mach-o/dyld.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>

#import <Cocoa/Cocoa.h>

#include "protocol.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/capi/cef_display_handler_capi.h"
#include "include/capi/cef_frame_capi.h"
#include "include/capi/cef_life_span_handler_capi.h"
#include "include/capi/cef_load_handler_capi.h"
#include "include/capi/cef_render_handler_capi.h"
#include "include/cef_api_hash.h"

// ── State ─────────────────────────────────────────────────────

static cef_browser_t* g_browser = NULL;
static int g_load_done = 0;
static int g_source_done = 0;
static int g_status_code = 0;
static int g_committed_main_frame = 0;
static int g_load_error = 0;
static char* g_html = NULL;
static size_t g_html_len = 0;
static char* g_selector_error = NULL;
static int g_click_x = -1;
static int g_click_y = -1;
static int g_clicked = 0;
static char g_marker[32];
static char g_click_prefix[48];
static char g_sel_err_prefix[48];

static void init_marker(void) {
    FILE* f = fopen("/dev/urandom", "r");
    unsigned char buf[36];
    if (f && fread(buf, 1, sizeof(buf), f) == sizeof(buf)) {
        for (int i = 0; i < 12; i++)
            snprintf(g_marker + i * 2, 3, "%02x", buf[i]);
        for (int i = 0; i < 12; i++)
            snprintf(g_click_prefix + i * 2, 3, "%02x", buf[12 + i]);
        for (int i = 0; i < 12; i++)
            snprintf(g_sel_err_prefix + i * 2, 3, "%02x", buf[24 + i]);
        fclose(f);
    } else {
        snprintf(g_marker, sizeof(g_marker), "_m%d_%ld_", getpid(), (long)time(NULL));
        snprintf(g_click_prefix, sizeof(g_click_prefix), "_c%d_", getpid());
        snprintf(g_sel_err_prefix, sizeof(g_sel_err_prefix), "_e%d_", getpid());
    }
}

// ── Ref counting ──────────────────────────────────────────────

static void CEF_CALLBACK add_ref(cef_base_ref_counted_t* self) { (void)self; }
static int CEF_CALLBACK release_fn(cef_base_ref_counted_t* self) { (void)self; return 1; }
static int CEF_CALLBACK has_one(cef_base_ref_counted_t* self) { (void)self; return 1; }
static int CEF_CALLBACK has_any(cef_base_ref_counted_t* self) { (void)self; return 1; }

static void init_base(cef_base_ref_counted_t* base, size_t size) {
    base->size = size;
    base->add_ref = add_ref;
    base->release = release_fn;
    base->has_one_ref = has_one;
    base->has_at_least_one_ref = has_any;
}

static void execute_js_on_main_frame(const char* js) {
    if (!g_browser || !js) return;
    cef_frame_t* frame = g_browser->get_main_frame(g_browser);
    if (!frame) return;
    cef_string_t cef_js = {};
    cef_string_utf8_to_utf16(js, strlen(js), &cef_js);
    frame->execute_java_script(frame, &cef_js, NULL, 0);
    cef_string_clear(&cef_js);
    frame->base.release(&frame->base);
}

// ── Display handler — captures console.log for DOM dump ───────

static int CEF_CALLBACK on_console_message(
    cef_display_handler_t* self, cef_browser_t* browser,
    cef_log_severity_t level, const cef_string_t* message,
    const cef_string_t* source, int line) {
    (void)self; (void)browser; (void)level; (void)source; (void)line;
    if (!message || !message->str || g_source_done) return 0;

    cef_string_utf8_t utf8 = {};
    cef_string_utf16_to_utf8(message->str, message->length, &utf8);

    size_t cplen = strlen(g_click_prefix);
    if (utf8.length > cplen && strncmp(utf8.str, g_click_prefix, cplen) == 0) {
        int x = 0, y = 0;
        if (sscanf(utf8.str + cplen, "%d,%d", &x, &y) == 2) {
            g_click_x = x;
            g_click_y = y;
        }
        cef_string_utf8_clear(&utf8);
        return 1;
    }

    size_t selen = strlen(g_sel_err_prefix);
    if (utf8.length >= selen && strncmp(utf8.str, g_sel_err_prefix, selen) == 0) {
        const char* msg = utf8.str + selen;
        size_t msg_len = utf8.length - selen;
        free(g_selector_error);
        g_selector_error = (char*)malloc(msg_len + 1);
        if (g_selector_error) {
            memcpy(g_selector_error, msg, msg_len);
            g_selector_error[msg_len] = '\0';
        }
        g_source_done = 1;
        cef_string_utf8_clear(&utf8);
        return 1;
    }

    size_t mlen = strlen(g_marker);
    if (utf8.length >= mlen && strncmp(utf8.str, g_marker, mlen) == 0) {
        const char* html = utf8.str + mlen;
        size_t html_len = utf8.length - mlen;
        free(g_html);
        g_html = NULL;
        g_html_len = 0;
        if (html_len > 0) {
            g_html = (char*)malloc(html_len);
            if (g_html) {
                memcpy(g_html, html, html_len);
                g_html_len = html_len;
            }
        }
        g_source_done = 1;
        cef_string_utf8_clear(&utf8);
        return 1;
    }

    cef_string_utf8_clear(&utf8);
    return 0;
}

static cef_display_handler_t g_display_handler;

// ── POST_LOAD_FIXUP for properties CEF resets ─────────────────

static const char* POST_LOAD_FIXUP =
    "(function(){"
    "  var rt={"
    "    OnInstalledReason:{CHROME_UPDATE:'chrome_update',INSTALL:'install',SHARED_MODULE_UPDATE:'shared_module_update',UPDATE:'update'},"
    "    OnRestartRequiredReason:{APP_UPDATE:'app_update',OS_UPDATE:'os_update',PERIODIC:'periodic'},"
    "    PlatformArch:{ARM:'arm',ARM64:'arm64',MIPS:'mips',MIPS64:'mips64',X86_32:'x86-32',X86_64:'x86-64'},"
    "    PlatformOs:{ANDROID:'android',CROS:'cros',LINUX:'linux',MAC:'mac',OPENBSD:'openbsd',WIN:'win'},"
    "    RequestUpdateCheckStatus:{NO_UPDATE:'no_update',THROTTLED:'throttled',UPDATE_AVAILABLE:'update_available'},"
    "    connect:function(){return{onDisconnect:{addListener:function(){}},onMessage:{addListener:function(){}},postMessage:function(){}};},"
    "    sendMessage:function(){},"
    "    id:undefined"
    "  };"
    "  function fix(){"
    "    if(window.chrome&&!window.chrome.runtime)window.chrome.runtime=rt;"
    "    if(window.outerHeight===window.innerHeight){"
    "      try{Object.defineProperty(window,'outerHeight',{get:function(){return window.innerHeight+85;},configurable:true});}catch(e){}"
    "    }"
    "  }"
    "  fix();"
    "  setTimeout(fix,0);"
    "  setTimeout(fix,1);"
    "  setTimeout(fix,10);"
    "  setTimeout(fix,50);"
    "})();";

// ── Load handler ──────────────────────────────────────────────

static int g_nav_count = 0;

static void CEF_CALLBACK on_loading_state_change(
    cef_load_handler_t* self, cef_browser_t* browser,
    int isLoading, int canGoBack, int canGoForward) {
    (void)self; (void)canGoBack; (void)canGoForward;
    if (isLoading && browser) {
        g_nav_count++;
        g_load_done = 0;
        g_source_done = 0;
    }
    if (!isLoading && browser) {
        g_load_done = 1;
    }
}

static void CEF_CALLBACK on_load_start(cef_load_handler_t* self,
    cef_browser_t* browser, cef_frame_t* frame, cef_transition_type_t tt) {
    (void)self; (void)browser; (void)tt;
    if (frame && frame->is_main(frame)) {
        g_status_code = 0;
        g_load_error = 0;
        g_committed_main_frame = 0;
    }
}

static void CEF_CALLBACK on_load_end(cef_load_handler_t* self,
    cef_browser_t* browser, cef_frame_t* frame, int httpStatusCode) {
    (void)self; (void)browser;
    if (frame && frame->is_main(frame)) {
        g_status_code = httpStatusCode;
        g_committed_main_frame = 1;
        cef_string_t cef_js = {};
        cef_string_utf8_to_utf16(POST_LOAD_FIXUP, strlen(POST_LOAD_FIXUP), &cef_js);
        frame->execute_java_script(frame, &cef_js, NULL, 0);
        cef_string_clear(&cef_js);
    }
}

static void CEF_CALLBACK on_load_error(cef_load_handler_t* self,
    cef_browser_t* browser, cef_frame_t* frame,
    cef_errorcode_t errorCode, const cef_string_t* errorText,
    const cef_string_t* failedUrl) {
    (void)self; (void)browser; (void)errorText; (void)failedUrl;
    if (frame && frame->is_main(frame)) {
        if (errorCode == -3) { // ERR_ABORTED: expected during redirects / challenge reloads
            return;
        }
        fprintf(stderr, "wick-renderer: main frame load error %d\n", errorCode);
        g_load_error = (int)errorCode;
        g_load_done = 1;
        g_source_done = 1;
    }
}

static cef_load_handler_t g_load_handler;

// ── Render handler (OSR 1920x1080) ────────────────────────────

static void CEF_CALLBACK get_view_rect(cef_render_handler_t* self,
    cef_browser_t* browser, cef_rect_t* rect) {
    (void)self; (void)browser;
    rect->x = 0; rect->y = 0; rect->width = 1920; rect->height = 1080;
}

static void CEF_CALLBACK on_paint(cef_render_handler_t* self,
    cef_browser_t* browser, cef_paint_element_type_t type,
    size_t n, const cef_rect_t* d, const void* buf, int w, int h) {
    (void)self; (void)browser; (void)type; (void)n; (void)d; (void)buf; (void)w; (void)h;
}

static int CEF_CALLBACK get_screen_info(cef_render_handler_t* self,
    cef_browser_t* browser, cef_screen_info_t* info) {
    (void)self; (void)browser; (void)info; return 0;
}

static cef_render_handler_t g_render_handler;

// ── Life span handler ─────────────────────────────────────────

static void CEF_CALLBACK on_after_created(cef_life_span_handler_t* self,
    cef_browser_t* browser) {
    (void)self; g_browser = browser;
}
static int CEF_CALLBACK do_close(cef_life_span_handler_t* self,
    cef_browser_t* browser) {
    (void)self; (void)browser; return 0;
}
static void CEF_CALLBACK on_before_close(cef_life_span_handler_t* self,
    cef_browser_t* browser) {
    (void)self; (void)browser;
}

static cef_life_span_handler_t g_life_span_handler;

// ── Client ────────────────────────────────────────────────────

static cef_life_span_handler_t* CEF_CALLBACK get_life_span_handler(cef_client_t* s) {
    (void)s; return &g_life_span_handler;
}
static cef_load_handler_t* CEF_CALLBACK get_load_handler(cef_client_t* s) {
    (void)s; return &g_load_handler;
}
static cef_render_handler_t* CEF_CALLBACK get_render_handler(cef_client_t* s) {
    (void)s; return &g_render_handler;
}
static cef_display_handler_t* CEF_CALLBACK get_display_handler(cef_client_t* s) {
    (void)s; return &g_display_handler;
}

static cef_client_t g_client;

// ── Render a URL with v4-meta response ────────────────────────

static void force_dump_dom(void) {
    char* dump_js = wick_asprintf(
        "console.log('%s'+(document.documentElement?document.documentElement.outerHTML:''));",
        g_marker
    );
    if (!dump_js) return;
    execute_js_on_main_frame(dump_js);
    free(dump_js);

    for (int i = 0; i < 100 && !g_source_done; i++) {
        cef_do_message_loop_work();
        usleep(10000);
    }
}

static char* copy_main_frame_url(void) {
    if (!g_browser) return NULL;
    cef_frame_t* frame = g_browser->get_main_frame(g_browser);
    if (!frame) return NULL;

    char* result = NULL;
    cef_string_userfree_t url_str = frame->get_url(frame);
    if (url_str) {
        if (url_str->str && url_str->length > 0) {
            cef_string_utf8_t utf8 = {};
            cef_string_utf16_to_utf8(url_str->str, url_str->length, &utf8);
            if (utf8.str && utf8.length > 0) {
                result = (char*)malloc(utf8.length + 1);
                if (result) {
                    memcpy(result, utf8.str, utf8.length);
                    result[utf8.length] = '\0';
                }
            }
            cef_string_utf8_clear(&utf8);
        }
        cef_string_userfree_free(url_str);
    }
    frame->base.release(&frame->base);
    return result;
}

static void render_url_v4(const WickRenderRequest* req) {
    g_load_done = 0;
    g_source_done = 0;
    g_status_code = 0;
    g_committed_main_frame = 0;
    g_load_error = 0;
    g_click_x = -1;
    g_click_y = -1;
    g_clicked = 0;
    free(g_html);
    g_html = NULL;
    g_html_len = 0;
    free(g_selector_error);
    g_selector_error = NULL;

    cef_frame_t* frame = g_browser->get_main_frame(g_browser);
    if (!frame) {
        wick_write_response(stdout, 0, req->url, NULL, 0, "main frame unavailable");
        return;
    }

    cef_string_t cef_url = {};
    cef_string_utf8_to_utf16(req->url, strlen(req->url), &cef_url);
    frame->load_url(frame, &cef_url);
    cef_string_clear(&cef_url);
    frame->base.release(&frame->base);

    int has_selector = (req->wait_for_selector && req->wait_for_selector[0]) ? 1 : 0;
    int total_cap = 0, post_load_cap = 0, js_cap = 0, cf_cap = 0;
    wick_clamp_timeouts(req->timeout_ms, has_selector, &total_cap, &post_load_cap, &js_cap, &cf_cap);

    int post_load_ms = 0;
    const int tick_ms = 50;
    int poll_injected = 0;
    int last_nav = g_nav_count;
    int total_ms = 0;
    int timed_out_uncommitted = 0;

    while (!g_source_done) {
        cef_do_message_loop_work();
        usleep(tick_ms * 1000);
        total_ms += tick_ms;

        if (g_nav_count != last_nav) {
            last_nav = g_nav_count;
            post_load_ms = 0;
            poll_injected = 0;
            g_click_x = -1;
            g_clicked = 0;
        }

        if (total_ms >= total_cap) {
            if (g_committed_main_frame && !g_source_done) {
                force_dump_dom();
            } else if (!g_committed_main_frame) {
                timed_out_uncommitted = 1;
            }
            break;
        }

        if (g_load_done && g_load_error == 0) {
            post_load_ms += tick_ms;

            if (!poll_injected && g_browser) {
                char* poller_js = wick_build_poller_js(
                    req->wait_for_selector,
                    g_marker,
                    g_sel_err_prefix,
                    js_cap,
                    cf_cap
                );
                if (poller_js) {
                    execute_js_on_main_frame(poller_js);
                    free(poller_js);
                }
                poll_injected = 1;
            }

            if (post_load_ms >= 2000 && !g_clicked && g_click_x == -1 && g_browser) {
                char* click_js = wick_asprintf(
                    "(function(){"
                    "var f=document.querySelector("
                    "'iframe[src*=\"challenges.cloudflare.com\"],"
                    "iframe[src*=\"captcha-delivery.com\"],"
                    "iframe[src*=\"recaptcha\"],"
                    "iframe[src*=\"hcaptcha\"]');"
                    "if(f){"
                    "  var r=f.getBoundingClientRect();"
                    "  if(r.width>0){"
                    "    console.log('%s'+"
                    "      Math.round(r.x+r.width/2)+','+"
                    "      Math.round(r.y+r.height/2));"
                    "  }"
                    "}"
                    "})();",
                    g_click_prefix
                );
                if (click_js) {
                    execute_js_on_main_frame(click_js);
                    free(click_js);
                }
            }

            if (g_click_x >= 0 && !g_clicked && g_browser) {
                cef_browser_host_t* host = g_browser->get_host(g_browser);
                if (host) {
                    cef_mouse_event_t mouse = {};
                    mouse.x = g_click_x;
                    mouse.y = g_click_y;
                    host->send_mouse_move_event(host, &mouse, 0);
                    cef_do_message_loop_work();
                    usleep(100000);
                    host->send_mouse_click_event(host, &mouse, MBT_LEFT, 0, 1);
                    cef_do_message_loop_work();
                    usleep(50000);
                    host->send_mouse_click_event(host, &mouse, MBT_LEFT, 1, 1);
                    host->base.release(&host->base);
                    g_clicked = 1;
                    post_load_ms = 0;
                    g_load_done = 0;
                    poll_injected = 0;
                }
            }

            if (post_load_ms >= post_load_cap && !g_source_done) {
                if (g_committed_main_frame) {
                    force_dump_dom();
                } else {
                    timed_out_uncommitted = 1;
                }
                break;
            }
        }
    }

    if (g_load_error != 0) {
        char err_msg[96];
        snprintf(err_msg, sizeof(err_msg), "load error %d", g_load_error);
        wick_write_response(stdout, 0, req->url, NULL, 0, err_msg);
        return;
    }

    if (g_selector_error && g_selector_error[0]) {
        char* err_msg = wick_asprintf("invalid selector: %s", g_selector_error);
        wick_write_response(stdout, g_status_code, req->url, NULL, 0,
                            err_msg ? err_msg : "invalid selector");
        free(err_msg);
        return;
    }

    if (timed_out_uncommitted || !g_source_done) {
        const char* msg = timed_out_uncommitted
            ? "timed out before main frame commit"
            : "timed out capturing rendered DOM";
        wick_write_response(stdout, g_status_code, req->url, NULL, 0, msg);
        return;
    }

    char* final_url = copy_main_frame_url();
    const char* out_url = (final_url && final_url[0]) ? final_url : req->url;
    wick_write_response(stdout, g_status_code, out_url, g_html, g_html_len, NULL);
    free(final_url);
}

// ── Main ──────────────────────────────────────────────────────

int main(int argc, char* argv[]) {
    init_marker();

    char exe_buf[4096];
    uint32_t exe_buf_size = sizeof(exe_buf);
    if (_NSGetExecutablePath(exe_buf, &exe_buf_size) != 0) {
        wick_write_ready(stdout, 0, "executable path too long");
        return 1;
    }

    const char* cache_dir_arg = NULL;
    char** new_argv = (char**)malloc(sizeof(char*) * ((size_t)argc + 3));
    if (!new_argv) {
        wick_write_ready(stdout, 0, "out of memory");
        return 1;
    }

    int new_argc = 0;
    new_argv[new_argc++] = argv[0];
    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "--cache-dir=", 12) == 0) {
            cache_dir_arg = argv[i] + 12;
        } else if (strcmp(argv[i], "--cache-dir") == 0 && i + 1 < argc) {
            cache_dir_arg = argv[++i];
        } else {
            new_argv[new_argc++] = argv[i];
        }
    }
    new_argv[new_argc++] = (char*)"--no-sandbox";
    new_argv[new_argc++] = (char*)"--disable-gpu-compositing";
    new_argv[new_argc] = NULL;

    cef_api_hash(CEF_API_VERSION, 0);

    cef_main_args_t main_args = { .argc = new_argc, .argv = new_argv };
    int exit_code = cef_execute_process(&main_args, NULL, NULL);
    if (exit_code >= 0) {
        free(new_argv);
        return exit_code;
    }

    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    }

    init_base(&g_display_handler.base, sizeof(cef_display_handler_t));
    g_display_handler.on_console_message = on_console_message;

    init_base(&g_load_handler.base, sizeof(cef_load_handler_t));
    g_load_handler.on_loading_state_change = on_loading_state_change;
    g_load_handler.on_load_start = on_load_start;
    g_load_handler.on_load_end = on_load_end;
    g_load_handler.on_load_error = on_load_error;

    init_base(&g_render_handler.base, sizeof(cef_render_handler_t));
    g_render_handler.get_view_rect = get_view_rect;
    g_render_handler.on_paint = on_paint;
    g_render_handler.get_screen_info = get_screen_info;

    init_base(&g_life_span_handler.base, sizeof(cef_life_span_handler_t));
    g_life_span_handler.on_after_created = on_after_created;
    g_life_span_handler.do_close = do_close;
    g_life_span_handler.on_before_close = on_before_close;

    init_base(&g_client.base, sizeof(cef_client_t));
    g_client.get_life_span_handler = get_life_span_handler;
    g_client.get_load_handler = get_load_handler;
    g_client.get_render_handler = get_render_handler;
    g_client.get_display_handler = get_display_handler;

    cef_settings_t settings = {};
    settings.size = sizeof(cef_settings_t);
    settings.windowless_rendering_enabled = 1;
    settings.no_sandbox = 1;
    settings.log_severity = LOGSEVERITY_ERROR;
    settings.persist_session_cookies = 1;

    char* exe_dir = dirname(exe_buf);
    char fw_dir[4096];
    snprintf(fw_dir, sizeof(fw_dir), "%s/../Frameworks/Chromium Embedded Framework.framework", exe_dir);
    cef_string_utf8_to_utf16(fw_dir, strlen(fw_dir), &settings.framework_dir_path);

    char helper_path[4096];
    snprintf(helper_path, sizeof(helper_path),
             "%s/../Frameworks/wick Helper.app/Contents/MacOS/wick Helper", exe_dir);
    char helper_real[4096];
    if (realpath(helper_path, helper_real)) {
        cef_string_utf8_to_utf16(helper_real, strlen(helper_real), &settings.browser_subprocess_path);
    }

    char bundle_path[4096];
    snprintf(bundle_path, sizeof(bundle_path), "%s/../..", exe_dir);
    char bundle_real[4096];
    if (realpath(bundle_path, bundle_real)) {
        cef_string_utf8_to_utf16(bundle_real, strlen(bundle_real), &settings.main_bundle_path);
    }

    char cache_path[4096];
    if (cache_dir_arg && cache_dir_arg[0]) {
        snprintf(cache_path, sizeof(cache_path), "%s", cache_dir_arg);
    } else {
        const char* state_home = getenv("MESH_WICK_HOME");
        if (state_home && state_home[0]) {
            snprintf(cache_path, sizeof(cache_path), "%s/cef-cache", state_home);
        } else {
            snprintf(cache_path, sizeof(cache_path), "%s/.mesh-wick/cef-cache",
                     getenv("HOME") ? getenv("HOME") : "/tmp");
        }
    }
    cef_string_utf8_to_utf16(cache_path, strlen(cache_path), &settings.root_cache_path);

    char profile_path[4096];
    snprintf(profile_path, sizeof(profile_path), "%s/Default", cache_path);
    cef_string_utf8_to_utf16(profile_path, strlen(profile_path), &settings.cache_path);

    if (!cef_initialize(&main_args, &settings, NULL, NULL)) {
        wick_write_ready(stdout, 0, "cef_initialize failed");
        free(new_argv);
        return 1;
    }

    cef_window_info_t window_info = {};
    window_info.size = sizeof(cef_window_info_t);
    window_info.windowless_rendering_enabled = 1;
    window_info.hidden = 1;

    cef_browser_settings_t browser_settings = {};
    browser_settings.size = sizeof(cef_browser_settings_t);
    browser_settings.windowless_frame_rate = 1;

    cef_string_t cef_url = {};
    cef_string_utf8_to_utf16("about:blank", 11, &cef_url);
    cef_browser_host_create_browser_sync(
        &window_info, &g_client, &cef_url, &browser_settings, NULL, NULL);
    cef_string_clear(&cef_url);

    // Wait for initial about:blank creation + idle load before advertising ready.
    for (int i = 0; i < 500 && (!g_browser || !g_load_done); i++) {
        cef_do_message_loop_work();
        usleep(10000);
    }
    if (!g_browser) {
        wick_write_ready(stdout, 0, "failed to create CEF browser");
        cef_shutdown();
        free(new_argv);
        return 1;
    }

    wick_write_ready(stdout, 1, NULL);

    char* line = NULL;
    int too_long = 0;
    while (wick_read_line(stdin, &line, WICK_MAX_REQUEST_LINE_BYTES, &too_long)) {
        if (too_long) {
            wick_write_response(stdout, 0, "", NULL, 0, "request line exceeds 64 KiB limit");
            continue;
        }
        if (line[0] == '\0') {
            free(line);
            break;
        }

        WickRenderRequest req = {};
        char err_buf[256] = {};
        if (wick_parse_request_line(line, &req, err_buf, sizeof(err_buf)) != 0) {
            wick_write_response(stdout, 0, "", NULL, 0, err_buf);
            free(line);
            continue;
        }
        free(line);
        line = NULL;

        render_url_v4(&req);
        wick_render_request_free(&req);
    }

    free(g_html);
    free(g_selector_error);
    cef_shutdown();
    free(new_argv);
    return 0;
}
