#ifndef WICK_HELPER_CORE_H
#define WICK_HELPER_CORE_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "browser_environment.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_render_process_handler_capi.h"
#include "include/cef_api_hash.h"

// Static handler structs are never heap-freed; release returns 0 ("still alive").
static void CEF_CALLBACK helper_add_ref(cef_base_ref_counted_t* s) { (void)s; }
static int CEF_CALLBACK helper_release(cef_base_ref_counted_t* s) { (void)s; return 0; }
static int CEF_CALLBACK helper_has_one(cef_base_ref_counted_t* s) { (void)s; return 1; }
static int CEF_CALLBACK helper_has_any(cef_base_ref_counted_t* s) { (void)s; return 1; }

static void helper_init_base(cef_base_ref_counted_t* base, size_t size) {
    base->size = size;
    base->add_ref = helper_add_ref;
    base->release = helper_release;
    base->has_one_ref = helper_has_one;
    base->has_at_least_one_ref = helper_has_any;
}

static void helper_execute_js_utf8(cef_frame_t* frame, const char* js) {
    if (!frame || !js || !js[0]) return;
    cef_string_t code = {};
    cef_string_utf8_to_utf16(js, strlen(js), &code);
    cef_string_t url = {};
    cef_string_utf8_to_utf16("", 0, &url);
    frame->execute_java_script(frame, &code, &url, 0);
    cef_string_clear(&code);
    cef_string_clear(&url);
}

static void CEF_CALLBACK helper_on_context_created(
    cef_render_process_handler_t* self,
    cef_browser_t* browser,
    cef_frame_t* frame,
    struct _cef_v8_context_t* context) {
    (void)self; (void)browser; (void)context;

    if (frame) {
        helper_execute_js_utf8(frame, OSR_ENVIRONMENT_JS);
        char* tz_js = wick_build_timezone_js(getenv("MESH_WICK_TIMEZONE"));
        if (tz_js) {
            helper_execute_js_utf8(frame, tz_js);
            free(tz_js);
        }
    }
}

static cef_render_process_handler_t g_helper_render_process_handler;

static cef_render_process_handler_t* CEF_CALLBACK helper_get_render_process_handler(
    cef_app_t* self) {
    (void)self;
    return &g_helper_render_process_handler;
}

static cef_app_t g_helper_app;

static int wick_run_helper_process(int argc, char* argv[]) {
    cef_api_hash(CEF_API_VERSION, 0);

    helper_init_base(&g_helper_render_process_handler.base,
                     sizeof(cef_render_process_handler_t));
    g_helper_render_process_handler.on_context_created = helper_on_context_created;

    helper_init_base(&g_helper_app.base, sizeof(cef_app_t));
    g_helper_app.get_render_process_handler = helper_get_render_process_handler;

    cef_main_args_t main_args = { .argc = argc, .argv = argv };
    return cef_execute_process(&main_args, &g_helper_app, NULL);
}

#endif
