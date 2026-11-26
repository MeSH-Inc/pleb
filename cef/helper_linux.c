// wick Helper (Linux): CEF subprocess with Chrome API stealth patches.
// Pure C — no Cocoa/Objective-C dependencies.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "stealth.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_render_process_handler_capi.h"
#include "include/cef_api_hash.h"

// ── Ref counting ──────────────────────────────────────────────

static void CEF_CALLBACK add_ref(cef_base_ref_counted_t* s) { (void)s; }
static int CEF_CALLBACK release_fn(cef_base_ref_counted_t* s) { (void)s; return 1; }
static int CEF_CALLBACK has_one(cef_base_ref_counted_t* s) { (void)s; return 1; }
static int CEF_CALLBACK has_any(cef_base_ref_counted_t* s) { (void)s; return 1; }

static void init_base(cef_base_ref_counted_t* base, size_t size) {
    base->size = size;
    base->add_ref = add_ref;
    base->release = release_fn;
    base->has_one_ref = has_one;
    base->has_at_least_one_ref = has_any;
}

// ── Render process handler ────────────────────────────────────

static void execute_js_utf8(cef_frame_t* frame, const char* js) {
    if (!frame || !js || !js[0]) return;
    cef_string_t code = {};
    cef_string_utf8_to_utf16(js, strlen(js), &code);
    cef_string_t url = {};
    cef_string_utf8_to_utf16("", 0, &url);
    frame->execute_java_script(frame, &code, &url, 0);
    cef_string_clear(&code);
    cef_string_clear(&url);
}

static void CEF_CALLBACK on_context_created(
    cef_render_process_handler_t* self,
    cef_browser_t* browser,
    cef_frame_t* frame,
    struct _cef_v8_context_t* context) {
    (void)self; (void)browser; (void)context;

    if (frame) {
        execute_js_utf8(frame, STEALTH_JS);
        char* tz_js = wick_build_timezone_js(getenv("MESH_WICK_TIMEZONE"));
        if (tz_js) {
            execute_js_utf8(frame, tz_js);
            free(tz_js);
        }
    }
}

static cef_render_process_handler_t g_render_process_handler;

// ── App handler ───────────────────────────────────────────────

static cef_render_process_handler_t* CEF_CALLBACK get_render_process_handler(
    cef_app_t* self) {
    (void)self;
    return &g_render_process_handler;
}

static cef_app_t g_app;

// ── Main ──────────────────────────────────────────────────────

int main(int argc, char* argv[]) {
    cef_api_hash(CEF_API_VERSION, 0);

    init_base(&g_render_process_handler.base,
               sizeof(cef_render_process_handler_t));
    g_render_process_handler.on_context_created = on_context_created;

    init_base(&g_app.base, sizeof(cef_app_t));
    g_app.get_render_process_handler = get_render_process_handler;

    cef_main_args_t main_args = { .argc = argc, .argv = argv };
    int exit_code = cef_execute_process(&main_args, &g_app, NULL);
    return exit_code;
}
