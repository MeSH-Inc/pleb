#ifndef PLEB_RENDERER_CORE_H
#define PLEB_RENDERER_CORE_H

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "protocol.h"
#include "include/capi/cef_app_capi.h"
#include "include/capi/cef_browser_capi.h"
#include "include/capi/cef_browser_process_handler_capi.h"
#include "include/capi/cef_client_capi.h"
#include "include/capi/cef_command_line_capi.h"
#include "include/capi/cef_display_handler_capi.h"
#include "include/capi/cef_frame_capi.h"
#include "include/capi/cef_life_span_handler_capi.h"
#include "include/capi/cef_load_handler_capi.h"
#include "include/capi/cef_render_handler_capi.h"
#include "include/capi/cef_request_handler_capi.h"
#include "include/capi/cef_resource_request_handler_capi.h"
#include "include/cef_api_hash.h"

// ── Shared renderer state ─────────────────────────────────────

static cef_browser_t* g_browser = NULL;
static int g_load_done = 0;
static int g_source_done = 0;
static int g_status_code = 0;
static int g_committed_main_frame = 0;
static int g_load_error = 0;
static int g_nav_count = 0;
static volatile int g_inflight_subresources = 0;
static char* g_html = NULL;
static size_t g_html_len = 0;
static char* g_selector_error = NULL;
static char* g_reveal_meta = NULL;
static size_t g_reveal_meta_len = 0;
static char* g_pre_reveal_html = NULL;
static size_t g_pre_reveal_html_len = 0;
static char* g_pre_reveal_url = NULL;
static int g_reveal_started = 0;
static char g_marker[32];
static char g_sel_err_prefix[48];
static char g_meta_marker[48];
static char g_pre_reveal_marker[48];

#define PLEB_MAX_CAPTURED_DATASETS 32
typedef struct {
    char* url;
    char* mime_type;
    size_t byte_count;
    int has_byte_count;
} PlebCapturedDataset;

static pthread_mutex_t g_dataset_mutex = PTHREAD_MUTEX_INITIALIZER;
static PlebCapturedDataset g_datasets[PLEB_MAX_CAPTURED_DATASETS];
static size_t g_dataset_count = 0;

static void pleb_clear_datasets(void) {
    pthread_mutex_lock(&g_dataset_mutex);
    for (size_t i = 0; i < g_dataset_count; i++) {
        free(g_datasets[i].url);
        free(g_datasets[i].mime_type);
        g_datasets[i].url = NULL;
        g_datasets[i].mime_type = NULL;
        g_datasets[i].byte_count = 0;
        g_datasets[i].has_byte_count = 0;
    }
    g_dataset_count = 0;
    pthread_mutex_unlock(&g_dataset_mutex);
}

static char* pleb_cef_string_to_cstr(cef_string_userfree_t s) {
    if (!s) return NULL;
    char* out = NULL;
    if (s->str && s->length > 0) {
        cef_string_utf8_t utf8 = {};
        cef_string_utf16_to_utf8(s->str, s->length, &utf8);
        if (utf8.str && utf8.length > 0) {
            out = (char*)malloc(utf8.length + 1);
            if (out) {
                memcpy(out, utf8.str, utf8.length);
                out[utf8.length] = '\0';
            }
        }
        cef_string_utf8_clear(&utf8);
    }
    cef_string_userfree_free(s);
    return out;
}

static int pleb_is_tracked_subresource(cef_resource_type_t rt) {
    return rt == RT_XHR || rt == RT_SCRIPT || rt == RT_SUB_RESOURCE;
}

static void pleb_inflight_dec_clamped(void) {
    int cur = __atomic_load_n(&g_inflight_subresources, __ATOMIC_ACQUIRE);
    while (cur > 0) {
        if (__atomic_compare_exchange_n(
                &g_inflight_subresources, &cur, cur - 1, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            return;
        }
    }
}

static void pleb_init_markers(void) {
    static unsigned long fallback_seq = 0;
    FILE* f = fopen("/dev/urandom", "r");
    unsigned char buf[48];
    if (f && fread(buf, 1, sizeof(buf), f) == sizeof(buf)) {
        for (int i = 0; i < 12; i++) {
            snprintf(g_marker + i * 2, 3, "%02x", buf[i]);
            snprintf(g_sel_err_prefix + i * 2, 3, "%02x", buf[12 + i]);
            snprintf(g_meta_marker + i * 2, 3, "%02x", buf[24 + i]);
            snprintf(g_pre_reveal_marker + i * 2, 3, "%02x", buf[36 + i]);
        }
        fclose(f);
    } else {
        if (f) fclose(f);
        fallback_seq++;
        snprintf(g_marker, sizeof(g_marker), "_m%d_%ld_%lu_",
                 getpid(), (long)time(NULL), fallback_seq);
        snprintf(g_sel_err_prefix, sizeof(g_sel_err_prefix), "_e%d_%ld_%lu_",
                 getpid(), (long)time(NULL), fallback_seq);
        snprintf(g_meta_marker, sizeof(g_meta_marker), "_j%d_%ld_%lu_",
                 getpid(), (long)time(NULL), fallback_seq);
        snprintf(g_pre_reveal_marker, sizeof(g_pre_reveal_marker), "_p%d_%ld_%lu_",
                 getpid(), (long)time(NULL), fallback_seq);
    }
}

// ── Ref counting (static structs are never heap-freed) ────────

static void CEF_CALLBACK add_ref(cef_base_ref_counted_t* self) { (void)self; }
static int CEF_CALLBACK release_fn(cef_base_ref_counted_t* self) { (void)self; return 0; }
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

static char* copy_main_frame_url(void);

// ── Display handler — captures DOM dump and selector errors ───

static int CEF_CALLBACK on_console_message(
    cef_display_handler_t* self, cef_browser_t* browser,
    cef_log_severity_t level, const cef_string_t* message,
    const cef_string_t* source, int line) {
    (void)self; (void)browser; (void)level; (void)source; (void)line;
    if (!message || !message->str || g_source_done) return 0;

    cef_string_utf8_t utf8 = {};
    cef_string_utf16_to_utf8(message->str, message->length, &utf8);

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

    size_t prlen = strlen(g_pre_reveal_marker);
    if (utf8.length >= prlen && strncmp(utf8.str, g_pre_reveal_marker, prlen) == 0) {
        const char* html = utf8.str + prlen;
        size_t html_len = utf8.length - prlen;
        free(g_pre_reveal_html);
        g_pre_reveal_html = NULL;
        g_pre_reveal_html_len = 0;
        if (html_len > 0) {
            g_pre_reveal_html = (char*)malloc(html_len);
            if (g_pre_reveal_html) {
                memcpy(g_pre_reveal_html, html, html_len);
                g_pre_reveal_html_len = html_len;
            }
        }
        free(g_pre_reveal_url);
        g_pre_reveal_url = copy_main_frame_url();
        g_reveal_started = 1;
        cef_string_utf8_clear(&utf8);
        return 1;
    }

    size_t jlen = strlen(g_meta_marker);
    if (utf8.length >= jlen && strncmp(utf8.str, g_meta_marker, jlen) == 0) {
        const char* meta = utf8.str + jlen;
        size_t meta_len = utf8.length - jlen;
        if (meta_len <= PLEB_MAX_REVEAL_META_BYTES) {
            free(g_reveal_meta);
            g_reveal_meta = (char*)malloc(meta_len + 1);
            if (g_reveal_meta) {
                memcpy(g_reveal_meta, meta, meta_len);
                g_reveal_meta[meta_len] = '\0';
                g_reveal_meta_len = meta_len;
            } else {
                g_reveal_meta_len = 0;
            }
        }
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

// ── Load handler ──────────────────────────────────────────────

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

static void CEF_CALLBACK on_load_start(
    cef_load_handler_t* self, cef_browser_t* browser,
    cef_frame_t* frame, cef_transition_type_t tt) {
    (void)self; (void)browser; (void)tt;
    if (frame && frame->is_main(frame)) {
        g_status_code = 0;
        g_load_error = 0;
        g_committed_main_frame = 0;
        __atomic_store_n(&g_inflight_subresources, 0, __ATOMIC_RELEASE);
    }
}

static void CEF_CALLBACK on_load_end(
    cef_load_handler_t* self, cef_browser_t* browser,
    cef_frame_t* frame, int httpStatusCode) {
    (void)self; (void)browser;
    if (frame && frame->is_main(frame)) {
        g_status_code = httpStatusCode;
        g_committed_main_frame = 1;
    }
}

static void CEF_CALLBACK on_load_error(
    cef_load_handler_t* self, cef_browser_t* browser,
    cef_frame_t* frame, cef_errorcode_t errorCode,
    const cef_string_t* errorText, const cef_string_t* failedUrl) {
    (void)self; (void)browser; (void)errorText; (void)failedUrl;
    if (frame && frame->is_main(frame)) {
        if (errorCode == -3) { // ERR_ABORTED: expected during redirects / challenge reloads
            return;
        }
        fprintf(stderr, "pleb-renderer: main frame load error %d\n", errorCode);
        g_load_error = (int)errorCode;
        g_load_done = 1;
        g_source_done = 1;
    }
}

static cef_load_handler_t g_load_handler;

// ── Render handler (OSR 1920x1080) ────────────────────────────

static void CEF_CALLBACK get_view_rect(
    cef_render_handler_t* self, cef_browser_t* browser, cef_rect_t* rect) {
    (void)self; (void)browser;
    rect->x = 0; rect->y = 0; rect->width = 1920; rect->height = 1080;
}

static void CEF_CALLBACK on_paint(
    cef_render_handler_t* self, cef_browser_t* browser,
    cef_paint_element_type_t type, size_t n, const cef_rect_t* d,
    const void* buf, int w, int h) {
    (void)self; (void)browser; (void)type; (void)n; (void)d; (void)buf; (void)w; (void)h;
}

static int CEF_CALLBACK get_screen_info(
    cef_render_handler_t* self, cef_browser_t* browser, cef_screen_info_t* info) {
    (void)self; (void)browser; (void)info;
    return 0;
}

static cef_render_handler_t g_render_handler;

// ── Life span handler ─────────────────────────────────────────

static void CEF_CALLBACK on_after_created(
    cef_life_span_handler_t* self, cef_browser_t* browser) {
    (void)self;
    g_browser = browser;
}

static int CEF_CALLBACK do_close(
    cef_life_span_handler_t* self, cef_browser_t* browser) {
    (void)self; (void)browser;
    return 0;
}

static void CEF_CALLBACK on_before_close(
    cef_life_span_handler_t* self, cef_browser_t* browser) {
    (void)self; (void)browser;
    g_source_done = 1;
}

static cef_life_span_handler_t g_life_span_handler;

// ── Request / Resource handler — tracks in-flight subresources ─

static cef_return_value_t CEF_CALLBACK on_before_resource_load(
    cef_resource_request_handler_t* self,
    cef_browser_t* browser,
    cef_frame_t* frame,
    cef_request_t* request,
    cef_callback_t* callback) {
    (void)self; (void)browser; (void)frame; (void)callback;
    if (request && pleb_is_tracked_subresource(request->get_resource_type(request))) {
        __atomic_fetch_add(&g_inflight_subresources, 1, __ATOMIC_ACQ_REL);
    }
    return RV_CONTINUE;
}

static int pleb_ascii_lower(unsigned char c) {
    return (c >= 'A' && c <= 'Z') ? (int)(c + ('a' - 'A')) : (int)c;
}

static int pleb_streq_ascii_ci(const char* a, const char* b) {
    if (!a || !b) return a == b;
    while (*a && *b) {
        if (pleb_ascii_lower((unsigned char)*a) != pleb_ascii_lower((unsigned char)*b)) return 0;
        a++;
        b++;
    }
    return *a == *b;
}

static int pleb_contains_ascii_ci(const char* haystack, const char* needle) {
    if (!haystack || !needle) return 0;
    if (!needle[0]) return 1;
    size_t nlen = strlen(needle);
    for (const char* p = haystack; *p; p++) {
        size_t i = 0;
        while (i < nlen && p[i] &&
               pleb_ascii_lower((unsigned char)p[i]) == pleb_ascii_lower((unsigned char)needle[i])) {
            i++;
        }
        if (i == nlen) return 1;
        if (!p[i]) break;
    }
    return 0;
}

static void CEF_CALLBACK on_resource_load_complete(
    cef_resource_request_handler_t* self,
    cef_browser_t* browser,
    cef_frame_t* frame,
    cef_request_t* request,
    cef_response_t* response,
    cef_urlrequest_status_t status,
    int64_t received_content_length) {
    (void)self; (void)browser; (void)frame;
    if (!request) return;
    cef_resource_type_t rt = request->get_resource_type(request);
    if (pleb_is_tracked_subresource(rt)) {
        pleb_inflight_dec_clamped();
    }
    if (rt != RT_XHR || status != UR_SUCCESS || !response) return;

    int http_status = response->get_status(response);
    if (http_status < 200 || http_status >= 300) return;

    char* method = pleb_cef_string_to_cstr(request->get_method(request));
    int is_get = (!method || pleb_streq_ascii_ci(method, "GET"));
    free(method);
    if (!is_get) return;

    char* mime = pleb_cef_string_to_cstr(response->get_mime_type(response));
    int looks_json = pleb_contains_ascii_ci(mime, "json");
    if (!looks_json) {
        free(mime);
        return;
    }

    char* url = response->get_url ? pleb_cef_string_to_cstr(response->get_url(response)) : NULL;
    if (!url || !url[0]) {
        free(url);
        url = pleb_cef_string_to_cstr(request->get_url(request));
    }
    if (!url || !url[0]) {
        free(url);
        free(mime);
        return;
    }

    pthread_mutex_lock(&g_dataset_mutex);
    if (g_dataset_count < PLEB_MAX_CAPTURED_DATASETS) {
        g_datasets[g_dataset_count].url = url;
        g_datasets[g_dataset_count].mime_type = mime;
        if (received_content_length > 0) {
            g_datasets[g_dataset_count].byte_count = (size_t)received_content_length;
            g_datasets[g_dataset_count].has_byte_count = 1;
        } else {
            g_datasets[g_dataset_count].byte_count = 0;
            g_datasets[g_dataset_count].has_byte_count = 0;
        }
        g_dataset_count++;
    } else {
        free(url);
        free(mime);
    }
    pthread_mutex_unlock(&g_dataset_mutex);
}

static cef_resource_request_handler_t g_resource_request_handler;

static cef_resource_request_handler_t* CEF_CALLBACK get_resource_request_handler(
    cef_request_handler_t* self,
    cef_browser_t* browser,
    cef_frame_t* frame,
    cef_request_t* request,
    int is_navigation,
    int is_download,
    const cef_string_t* request_initiator,
    int* disable_default_handling) {
    (void)self; (void)browser; (void)frame; (void)request;
    (void)is_navigation; (void)is_download; (void)request_initiator;
    (void)disable_default_handling;
    return &g_resource_request_handler;
}

static cef_request_handler_t g_request_handler;

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
static cef_request_handler_t* CEF_CALLBACK get_request_handler(cef_client_t* s) {
    (void)s; return &g_request_handler;
}

static cef_client_t g_client;

static void pleb_init_cef_handlers(void) {
    pleb_init_markers();

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

    init_base(&g_resource_request_handler.base, sizeof(cef_resource_request_handler_t));
    g_resource_request_handler.on_before_resource_load = on_before_resource_load;
    g_resource_request_handler.on_resource_load_complete = on_resource_load_complete;

    init_base(&g_request_handler.base, sizeof(cef_request_handler_t));
    g_request_handler.get_resource_request_handler = get_resource_request_handler;

    init_base(&g_client.base, sizeof(cef_client_t));
    g_client.get_life_span_handler = get_life_span_handler;
    g_client.get_load_handler = get_load_handler;
    g_client.get_render_handler = get_render_handler;
    g_client.get_display_handler = get_display_handler;
    g_client.get_request_handler = get_request_handler;
}

// ── App & browser process handler — subprocess keyring isolation

static void pleb_append_switch_if_missing(cef_command_line_t* cmd, const char* name) {
    if (!cmd || !name || !name[0]) return;
    cef_string_t sw = {};
    cef_string_utf8_to_utf16(name, strlen(name), &sw);
    if (cmd->has_switch && cmd->append_switch && !cmd->has_switch(cmd, &sw)) {
        cmd->append_switch(cmd, &sw);
    }
    cef_string_clear(&sw);
}

static void pleb_append_switch_value_if_missing(
    cef_command_line_t* cmd, const char* name, const char* value) {
    if (!cmd || !name || !name[0] || !value) return;
    cef_string_t sw = {};
    cef_string_utf8_to_utf16(name, strlen(name), &sw);
    if (cmd->has_switch && cmd->append_switch_with_value && !cmd->has_switch(cmd, &sw)) {
        cef_string_t val = {};
        cef_string_utf8_to_utf16(value, strlen(value), &val);
        cmd->append_switch_with_value(cmd, &sw, &val);
        cef_string_clear(&val);
    }
    cef_string_clear(&sw);
}

static void pleb_append_keyring_isolation_switches(cef_command_line_t* cmd) {
    if (!cmd || !cmd->is_valid || !cmd->is_valid(cmd)) return;
#ifdef __APPLE__
    pleb_append_switch_if_missing(cmd, "use-mock-keychain");
#endif
    pleb_append_switch_value_if_missing(cmd, "password-store", "basic");
}

static void CEF_CALLBACK on_before_child_process_launch(
    cef_browser_process_handler_t* self,
    cef_command_line_t* command_line) {
    (void)self;
    pleb_append_keyring_isolation_switches(command_line);
}

static cef_browser_process_handler_t g_browser_process_handler;

static void CEF_CALLBACK on_before_command_line_processing(
    cef_app_t* self,
    const cef_string_t* process_type,
    cef_command_line_t* command_line) {
    (void)self;
    if (!process_type || process_type->length == 0) {
        pleb_append_keyring_isolation_switches(command_line);
    }
}

static cef_browser_process_handler_t* CEF_CALLBACK get_browser_process_handler(
    cef_app_t* self) {
    (void)self;
    return &g_browser_process_handler;
}

static cef_app_t g_app;

static void pleb_init_cef_app(void) {
    init_base(&g_browser_process_handler.base, sizeof(cef_browser_process_handler_t));
    g_browser_process_handler.on_before_child_process_launch = on_before_child_process_launch;

    init_base(&g_app.base, sizeof(cef_app_t));
    g_app.on_before_command_line_processing = on_before_command_line_processing;
    g_app.get_browser_process_handler = get_browser_process_handler;
}

// ── Render a URL with v6-meta response ────────────────────────

static void force_dump_dom(void) {
    char* dump_js = pleb_asprintf(
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

typedef struct {
    char* data;
    size_t len;
    size_t cap;
} PlebStrBuf;

static int pleb_buf_append(PlebStrBuf* b, const char* s, size_t n) {
    if (n == 0) return 0;
    if (b->len + n + 1 > b->cap) {
        size_t next_cap = b->cap ? b->cap * 2 : 512;
        while (next_cap < b->len + n + 1) next_cap *= 2;
        char* grown = (char*)realloc(b->data, next_cap);
        if (!grown) return -1;
        b->data = grown;
        b->cap = next_cap;
    }
    memcpy(b->data + b->len, s, n);
    b->len += n;
    b->data[b->len] = '\0';
    return 0;
}

static int pleb_buf_append_cstr(PlebStrBuf* b, const char* s) {
    return s ? pleb_buf_append(b, s, strlen(s)) : 0;
}

static int pleb_buf_append_json_str(PlebStrBuf* b, const char* s) {
    if (!s) return pleb_buf_append_cstr(b, "null");
    if (pleb_buf_append(b, "\"", 1) != 0) return -1;
    for (const unsigned char* p = (const unsigned char*)s; *p; p++) {
        unsigned char c = *p;
        char esc[8];
        const char* rep = NULL;
        size_t rlen = 0;
        switch (c) {
            case '"': rep = "\\\""; rlen = 2; break;
            case '\\': rep = "\\\\"; rlen = 2; break;
            case '\b': rep = "\\b"; rlen = 2; break;
            case '\f': rep = "\\f"; rlen = 2; break;
            case '\n': rep = "\\n"; rlen = 2; break;
            case '\r': rep = "\\r"; rlen = 2; break;
            case '\t': rep = "\\t"; rlen = 2; break;
            default:
                if (c < 0x20) {
                    snprintf(esc, sizeof(esc), "\\u%04x", c);
                    rep = esc;
                    rlen = 6;
                } else {
                    char ch = (char)c;
                    if (pleb_buf_append(b, &ch, 1) != 0) return -1;
                    continue;
                }
        }
        if (pleb_buf_append(b, rep, rlen) != 0) return -1;
    }
    return pleb_buf_append(b, "\"", 1);
}

static char* pleb_build_combined_meta_json(size_t* out_len) {
    *out_len = 0;
    pthread_mutex_lock(&g_dataset_mutex);
    size_t ds_count = g_dataset_count;
    int has_reveal_meta = (g_reveal_meta && g_reveal_meta_len >= 2
        && g_reveal_meta[0] == '{' && g_reveal_meta[g_reveal_meta_len - 1] == '}');
    if (!has_reveal_meta && ds_count == 0) {
        pthread_mutex_unlock(&g_dataset_mutex);
        return NULL;
    }

    PlebStrBuf sb = {NULL, 0, 0};
    if (has_reveal_meta) {
        if (pleb_buf_append(&sb, g_reveal_meta, g_reveal_meta_len - 1) != 0) {
            pthread_mutex_unlock(&g_dataset_mutex);
            free(sb.data);
            return NULL;
        }
    } else {
        if (pleb_buf_append_cstr(&sb, "{") != 0) {
            pthread_mutex_unlock(&g_dataset_mutex);
            free(sb.data);
            return NULL;
        }
    }

    if (ds_count > 0) {
        if (has_reveal_meta) {
            if (pleb_buf_append_cstr(&sb, ",") != 0) {
                pthread_mutex_unlock(&g_dataset_mutex);
                free(sb.data);
                return NULL;
            }
        }
        if (pleb_buf_append_cstr(&sb, "\"datasets\":[") != 0) {
            pthread_mutex_unlock(&g_dataset_mutex);
            free(sb.data);
            return NULL;
        }
        for (size_t i = 0; i < ds_count; i++) {
            if (i > 0 && pleb_buf_append_cstr(&sb, ",") != 0) {
                pthread_mutex_unlock(&g_dataset_mutex);
                free(sb.data);
                return NULL;
            }
            if (pleb_buf_append_cstr(&sb, "{\"url\":") != 0
                || pleb_buf_append_json_str(&sb, g_datasets[i].url) != 0
                || pleb_buf_append_cstr(&sb, ",\"mime_type\":") != 0
                || pleb_buf_append_json_str(&sb, g_datasets[i].mime_type) != 0) {
                pthread_mutex_unlock(&g_dataset_mutex);
                free(sb.data);
                return NULL;
            }
            if (g_datasets[i].has_byte_count) {
                char num_buf[48];
                snprintf(num_buf, sizeof(num_buf), ",\"byte_count\":%zu", g_datasets[i].byte_count);
                if (pleb_buf_append_cstr(&sb, num_buf) != 0) {
                    pthread_mutex_unlock(&g_dataset_mutex);
                    free(sb.data);
                    return NULL;
                }
            }
            if (pleb_buf_append_cstr(&sb, "}") != 0) {
                pthread_mutex_unlock(&g_dataset_mutex);
                free(sb.data);
                return NULL;
            }
        }
        if (pleb_buf_append_cstr(&sb, "]") != 0) {
            pthread_mutex_unlock(&g_dataset_mutex);
            free(sb.data);
            return NULL;
        }
    }
    pthread_mutex_unlock(&g_dataset_mutex);

    if (pleb_buf_append_cstr(&sb, "}") != 0 || sb.len > PLEB_MAX_REVEAL_META_BYTES) {
        free(sb.data);
        return NULL;
    }
    *out_len = sb.len;
    return sb.data;
}

static void render_url_v6(const PlebRenderRequest* req) {
    pleb_init_markers();

    g_load_done = 0;
    g_source_done = 0;
    g_status_code = 0;
    g_committed_main_frame = 0;
    g_load_error = 0;
    g_reveal_started = 0;
    __atomic_store_n(&g_inflight_subresources, 0, __ATOMIC_RELEASE);
    free(g_html);
    g_html = NULL;
    g_html_len = 0;
    free(g_selector_error);
    g_selector_error = NULL;
    free(g_reveal_meta);
    g_reveal_meta = NULL;
    g_reveal_meta_len = 0;
    free(g_pre_reveal_html);
    g_pre_reveal_html = NULL;
    g_pre_reveal_html_len = 0;
    free(g_pre_reveal_url);
    g_pre_reveal_url = NULL;
    pleb_clear_datasets();

    cef_frame_t* frame = g_browser->get_main_frame(g_browser);
    if (!frame) {
        pleb_write_response(stdout, 0, req->url, NULL, 0, NULL, 0, "main frame unavailable", "network");
        return;
    }

    cef_string_t cef_url = {};
    cef_string_utf8_to_utf16(req->url, strlen(req->url), &cef_url);
    frame->load_url(frame, &cef_url);
    cef_string_clear(&cef_url);
    frame->base.release(&frame->base);

    int has_selector = (req->wait_for_selector && req->wait_for_selector[0]) ? 1 : 0;
    int has_reveal = ((req->reveal && strcmp(req->reveal, "none") != 0) || req->scroll > 0) ? 1 : 0;
    int total_cap = 0, post_load_cap = 0, js_cap = 0;
    pleb_clamp_timeouts(req->timeout_ms, has_selector, has_reveal, &total_cap, &post_load_cap, &js_cap);

    int post_load_ms = 0;
    const int tick_ms = 25;
    int poll_injected = 0;
    int last_nav = g_nav_count;
    int last_pushed_inflight = -1;
    int total_ms = 0;
    int timed_out_uncommitted = 0;
    char* saved_final_url = NULL;

    while (!g_source_done) {
        cef_do_message_loop_work();
        usleep(tick_ms * 1000);
        total_ms += tick_ms;

        if (g_nav_count != last_nav) {
            if (g_reveal_started) {
                saved_final_url = g_pre_reveal_url ? g_pre_reveal_url : copy_main_frame_url();
                g_pre_reveal_url = NULL;
                if (g_browser && g_browser->stop_load) {
                    g_browser->stop_load(g_browser);
                }
                free(g_html);
                g_html = g_pre_reveal_html;
                g_html_len = g_pre_reveal_html_len;
                g_pre_reveal_html = NULL;
                g_pre_reveal_html_len = 0;
                free(g_reveal_meta);
                g_reveal_meta = pleb_strdup("{\"revealed_panels\":[],\"reveal_warning\":\"aborted --reveal because a control triggered page navigation\"}");
                g_reveal_meta_len = g_reveal_meta ? strlen(g_reveal_meta) : 0;
                g_source_done = 1;
                g_load_error = 0;
                break;
            }
            last_nav = g_nav_count;
            post_load_ms = 0;
            poll_injected = 0;
            last_pushed_inflight = -1;
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

            int cur_inflight = __atomic_load_n(&g_inflight_subresources, __ATOMIC_ACQUIRE);
            if (cur_inflight != last_pushed_inflight && g_browser) {
                char sync_js[64];
                snprintf(sync_js, sizeof(sync_js), "window.__plebInflight=%d;", cur_inflight);
                execute_js_on_main_frame(sync_js);
                last_pushed_inflight = cur_inflight;
            }

            if (!poll_injected && g_browser) {
                int reveal_budget = (post_load_cap > js_cap) ? (post_load_cap - js_cap) : (post_load_cap / 2);
                char* poller_js = pleb_build_poller_js(
                    req->wait_for_selector,
                    g_marker,
                    g_sel_err_prefix,
                    g_meta_marker,
                    g_pre_reveal_marker,
                    req->reveal,
                    req->scroll,
                    js_cap,
                    reveal_budget
                );
                if (poller_js) {
                    execute_js_on_main_frame(poller_js);
                    free(poller_js);
                }
                poll_injected = 1;
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
        free(saved_final_url);
        char err_msg[96];
        snprintf(err_msg, sizeof(err_msg), "load error %d", g_load_error);
        pleb_write_response(stdout, 0, req->url, NULL, 0, NULL, 0, err_msg, "load_error");
        return;
    }

    if (g_selector_error && g_selector_error[0]) {
        free(saved_final_url);
        char* err_msg = pleb_asprintf("invalid selector: %s", g_selector_error);
        pleb_write_response(stdout, g_status_code, req->url, NULL, 0, NULL, 0,
                            err_msg ? err_msg : "invalid selector", "invalid_selector");
        free(err_msg);
        return;
    }

    if (timed_out_uncommitted || !g_source_done) {
        free(saved_final_url);
        const char* msg = timed_out_uncommitted
            ? "timed out before main frame commit"
            : "timed out capturing rendered DOM";
        pleb_write_response(stdout, g_status_code, req->url, NULL, 0, NULL, 0, msg, "timeout");
        return;
    }

    char* final_url = saved_final_url ? saved_final_url : copy_main_frame_url();
    const char* out_url = (final_url && final_url[0]) ? final_url : req->url;
    size_t meta_len = 0;
    char* meta_json = pleb_build_combined_meta_json(&meta_len);
    pleb_write_response(stdout, g_status_code, out_url, g_html, g_html_len, meta_json, meta_len, NULL, NULL);
    free(meta_json);
    free(final_url);
}

static void pleb_resolve_cache_path(const char* cache_dir_arg, char* out, size_t out_cap) {
    char raw[4096];
    if (cache_dir_arg && cache_dir_arg[0]) {
        snprintf(raw, sizeof(raw), "%s", cache_dir_arg);
    } else {
        const char* state_home = getenv("PLEB_HOME");
        const char* home = getenv("HOME");
        if (state_home && state_home[0]) {
            snprintf(raw, sizeof(raw), "%s/cef-cache", state_home);
        } else if (home && home[0]) {
            snprintf(raw, sizeof(raw), "%s/.pleb/cef-cache", home);
        } else {
            snprintf(raw, sizeof(raw), "/tmp/pleb-%lu/cef-cache", (unsigned long)getuid());
        }
    }
    char resolved[4096];
    if (realpath(raw, resolved)) {
        snprintf(out, out_cap, "%s", resolved);
    } else {
        snprintf(out, out_cap, "%s", raw);
    }
}

static void pleb_run_request_loop(void) {
    char* line = NULL;
    int too_long = 0;
    while (pleb_read_line(stdin, &line, PLEB_MAX_REQUEST_LINE_BYTES, &too_long)) {
        if (too_long) {
            pleb_write_response(stdout, 0, "", NULL, 0, NULL, 0,
                                "request line exceeds 64 KiB limit",
                                "protocol_mismatch");
            continue;
        }
        if (line[0] == '\0') {
            free(line);
            line = NULL;
            break;
        }

        PlebRenderRequest req = {};
        char err_buf[256] = {};
        if (pleb_parse_request_line(line, &req, err_buf, sizeof(err_buf)) != 0) {
            pleb_write_response(stdout, 0, "", NULL, 0, NULL, 0, err_buf, "protocol_mismatch");
            free(line);
            line = NULL;
            continue;
        }
        free(line);
        line = NULL;

        render_url_v6(&req);
        pleb_render_request_free(&req);
    }

    free(g_html);
    g_html = NULL;
    free(g_selector_error);
    g_selector_error = NULL;
    free(g_reveal_meta);
    g_reveal_meta = NULL;
    free(g_pre_reveal_html);
    g_pre_reveal_html = NULL;
    free(g_pre_reveal_url);
    g_pre_reveal_url = NULL;
    pleb_clear_datasets();
}

#endif
