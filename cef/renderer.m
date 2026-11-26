// wick-renderer (macOS): CEF offscreen renderer with v4-meta stdin/stdout JSON+bytes framing.

#include <libgen.h>
#include <mach-o/dyld.h>
#import <Cocoa/Cocoa.h>

#include "renderer_core.h"

int main(int argc, char* argv[]) {
    char exe_buf[4096];
    uint32_t exe_buf_size = sizeof(exe_buf);
    if (_NSGetExecutablePath(exe_buf, &exe_buf_size) != 0) {
        wick_write_ready(stdout, 0, "executable path too long");
        return 1;
    }

    const char* cache_dir_arg = NULL;
    char** new_argv = (char**)malloc(sizeof(char*) * ((size_t)argc + 5));
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
    new_argv[new_argc++] = (char*)"--mute-audio";
    new_argv[new_argc++] = (char*)"--autoplay-policy=document-user-gesture-required";
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

    wick_init_cef_handlers();

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
    wick_resolve_cache_path(cache_dir_arg, cache_path, sizeof(cache_path));
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
    wick_run_request_loop();

    cef_shutdown();
    free(new_argv);
    return 0;
}
