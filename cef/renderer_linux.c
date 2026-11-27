// pleb-renderer (Linux): CEF offscreen renderer with v5-meta stdin/stdout JSON+bytes framing.

#include <libgen.h>
#include <limits.h>

#include "renderer_core.h"

static int get_exe_dir(char* buf, size_t bufsize) {
    ssize_t len = readlink("/proc/self/exe", buf, bufsize - 1);
    if (len <= 0) return -1;
    buf[len] = '\0';
    char* dir = dirname(buf);
    if (dir != buf) {
        memmove(buf, dir, strlen(dir) + 1);
    }
    return 0;
}

int main(int argc, char* argv[]) {
    const char* display_env = getenv("DISPLAY");
    int has_display = (display_env != NULL && display_env[0] != '\0');
    int extra_args = has_display ? 7 : 9;

    const char* cache_dir_arg = NULL;
    char** new_argv = (char**)malloc(sizeof(char*) * ((size_t)argc + (size_t)extra_args + 1));
    if (!new_argv) {
        pleb_write_ready(stdout, 0, "out of memory");
        return 1;
    }

    int a = 0;
    new_argv[a++] = argv[0];
    for (int i = 1; i < argc; i++) {
        if (strncmp(argv[i], "--cache-dir=", 12) == 0) {
            cache_dir_arg = argv[i] + 12;
        } else if (strcmp(argv[i], "--cache-dir") == 0 && i + 1 < argc) {
            cache_dir_arg = argv[++i];
        } else {
            new_argv[a++] = argv[i];
        }
    }

    if (has_display) {
        new_argv[a++] = (char*)"--no-sandbox";
        new_argv[a++] = (char*)"--use-fake-ui-for-media-stream";
        new_argv[a++] = (char*)"--in-process-gpu";
        new_argv[a++] = (char*)"--disable-gpu-compositing";
        new_argv[a++] = (char*)"--ignore-gpu-blocklist";
    } else {
        new_argv[a++] = (char*)"--disable-gpu";
        new_argv[a++] = (char*)"--disable-gpu-compositing";
        new_argv[a++] = (char*)"--no-sandbox";
        new_argv[a++] = (char*)"--ozone-platform=headless";
        new_argv[a++] = (char*)"--use-fake-ui-for-media-stream";
        new_argv[a++] = (char*)"--in-process-gpu";
        new_argv[a++] = (char*)"--enable-webgl";
    }
    new_argv[a++] = (char*)"--mute-audio";
    new_argv[a++] = (char*)"--password-store=basic";
    new_argv[a] = NULL;

    cef_api_hash(CEF_API_VERSION, 0);
    pleb_init_cef_app();

    cef_main_args_t main_args = { .argc = a, .argv = new_argv };
    int exit_code = cef_execute_process(&main_args, &g_app, NULL);
    if (exit_code >= 0) {
        free(new_argv);
        return exit_code;
    }

    pleb_init_cef_handlers();

    cef_settings_t settings = {};
    settings.size = sizeof(cef_settings_t);
    settings.windowless_rendering_enabled = 1;
    settings.no_sandbox = 1;
    settings.log_severity = LOGSEVERITY_ERROR;
    settings.persist_session_cookies = 1;

    char exe_dir[PATH_MAX];
    if (get_exe_dir(exe_dir, sizeof(exe_dir)) == 0) {
        char helper_path[PATH_MAX];
        snprintf(helper_path, sizeof(helper_path), "%s/pleb-helper", exe_dir);
        if (access(helper_path, X_OK) == 0) {
            cef_string_utf8_to_utf16(helper_path, strlen(helper_path),
                                      &settings.browser_subprocess_path);
        } else {
            char exe_path[PATH_MAX];
            ssize_t exe_len = readlink("/proc/self/exe", exe_path, sizeof(exe_path) - 1);
            if (exe_len > 0) {
                exe_path[exe_len] = '\0';
                cef_string_utf8_to_utf16(exe_path, strlen(exe_path),
                                          &settings.browser_subprocess_path);
            }
        }

        char resources_path[PATH_MAX];
        snprintf(resources_path, sizeof(resources_path), "%s", exe_dir);
        cef_string_utf8_to_utf16(resources_path, strlen(resources_path),
                                  &settings.resources_dir_path);

        char locales_path[PATH_MAX];
        snprintf(locales_path, sizeof(locales_path), "%s/locales", exe_dir);
        cef_string_utf8_to_utf16(locales_path, strlen(locales_path),
                                  &settings.locales_dir_path);
    }

    char cache_path[PATH_MAX];
    pleb_resolve_cache_path(cache_dir_arg, cache_path, sizeof(cache_path));
    cef_string_utf8_to_utf16(cache_path, strlen(cache_path), &settings.root_cache_path);

    char profile_path[PATH_MAX];
    snprintf(profile_path, sizeof(profile_path), "%s/Default", cache_path);
    cef_string_utf8_to_utf16(profile_path, strlen(profile_path), &settings.cache_path);

    if (!cef_initialize(&main_args, &settings, &g_app, NULL)) {
        pleb_write_ready(stdout, 0, "cef_initialize failed");
        free(new_argv);
        return 1;
    }

    cef_window_info_t window_info = {};
    window_info.size = sizeof(cef_window_info_t);
    window_info.windowless_rendering_enabled = 1;

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
        pleb_write_ready(stdout, 0, "failed to create CEF browser");
        cef_shutdown();
        free(new_argv);
        return 1;
    }

    pleb_write_ready(stdout, 1, NULL);
    pleb_run_request_loop();

    cef_shutdown();
    free(new_argv);
    return 0;
}
