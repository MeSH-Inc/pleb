#ifndef WICK_BROWSER_ENVIRONMENT_H
#define WICK_BROWSER_ENVIRONMENT_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// Minimal environment defaults for Chromium offscreen rendering (OSR):
// - Non-zero viewport and screen geometry (headless OSR starts with 0x0 outer/screen bounds).
// - Non-empty navigator.languages fallback.
__attribute__((unused)) static const char* OSR_ENVIRONMENT_JS =
    "(function() {"
    "  try {"
    "    if (window.innerWidth === 0) {"
    "      Object.defineProperty(window, 'innerWidth', {get: function() { return 1920; }, configurable: true});"
    "      Object.defineProperty(window, 'innerHeight', {get: function() { return 1080; }, configurable: true});"
    "    }"
    "    if (window.outerWidth === 0) {"
    "      Object.defineProperty(window, 'outerWidth', {get: function() { return 1920; }, configurable: true});"
    "    }"
    "    if (window.outerHeight === 0 || window.outerHeight === window.innerHeight) {"
    "      Object.defineProperty(window, 'outerHeight', {get: function() { return (window.innerHeight || 1080) + 85; }, configurable: true});"
    "    }"
    "  } catch (e) {}"
    "  try {"
    "    if (screen.width === 0 || screen.height === 0) {"
    "      Object.defineProperty(screen, 'width', {get: function() { return 1920; }, configurable: true});"
    "      Object.defineProperty(screen, 'height', {get: function() { return 1080; }, configurable: true});"
    "      Object.defineProperty(screen, 'availWidth', {get: function() { return 1920; }, configurable: true});"
    "      Object.defineProperty(screen, 'availHeight', {get: function() { return 1040; }, configurable: true});"
    "      Object.defineProperty(screen, 'colorDepth', {get: function() { return 24; }, configurable: true});"
    "      Object.defineProperty(screen, 'pixelDepth', {get: function() { return 24; }, configurable: true});"
    "    }"
    "  } catch (e) {}"
    "  try {"
    "    if (!navigator.languages || navigator.languages.length === 0) {"
    "      Object.defineProperty(navigator, 'languages', {"
    "        get: function() { return ['en-US', 'en']; },"
    "        configurable: true"
    "      });"
    "    }"
    "  } catch (e) {}"
    "})();";

// Escapes `src` so it can be spliced inside single quotes `'...'` in JavaScript.
// Caller owns returned buffer and must free().
__attribute__((unused)) static char* wick_escape_js_string(const char* src) {
    if (!src) return NULL;
    size_t cap = strlen(src) * 6 + 1;
    char* out = (char*)malloc(cap);
    if (!out) return NULL;

    size_t j = 0;
    const unsigned char* p = (const unsigned char*)src;
    while (*p) {
        unsigned char c = *p;
        if (c == '\\') {
            out[j++] = '\\';
            out[j++] = '\\';
            p++;
        } else if (c == '\'') {
            out[j++] = '\\';
            out[j++] = '\'';
            p++;
        } else if (c == '"') {
            out[j++] = '\\';
            out[j++] = '"';
            p++;
        } else if (c == '\n') {
            out[j++] = '\\';
            out[j++] = 'n';
            p++;
        } else if (c == '\r') {
            out[j++] = '\\';
            out[j++] = 'r';
            p++;
        } else if (c == '\t') {
            out[j++] = '\\';
            out[j++] = 't';
            p++;
        } else if (c < 0x20 || c == 0x7f) {
            j += (size_t)snprintf(out + j, cap - j, "\\x%02x", c);
            p++;
        } else if (c == 0xE2 && p[1] == 0x80 && p[2] == 0xA8) {
            memcpy(out + j, "\\u2028", 6);
            j += 6;
            p += 3;
        } else if (c == 0xE2 && p[1] == 0x80 && p[2] == 0xA9) {
            memcpy(out + j, "\\u2029", 6);
            j += 6;
            p += 3;
        } else {
            out[j++] = (char)c;
            p++;
        }
    }
    out[j] = '\0';
    return out;
}

// Builds an optional Intl.DateTimeFormat override script when MESH_WICK_TIMEZONE
// is explicitly configured. Returns NULL when unset/empty so local TZ is untouched.
__attribute__((unused)) static char* wick_build_timezone_js(const char* tz) {
    if (!tz || !tz[0]) return NULL;
    char* escaped = wick_escape_js_string(tz);
    if (!escaped) return NULL;

    const char* fmt =
        "(function(){"
        "  var tz='%s';"
        "  var origDTF=Intl.DateTimeFormat;"
        "  var Wrapped=new Proxy(origDTF,{"
        "    apply:function(target,thisArg,args){"
        "      var next=args[1]?Object.assign({},args[1]):{};"
        "      if(!next.timeZone)next.timeZone=tz;"
        "      return Reflect.apply(target,thisArg,[args[0],next]);"
        "    },"
        "    construct:function(target,args,newTarget){"
        "      var next=args[1]?Object.assign({},args[1]):{};"
        "      if(!next.timeZone)next.timeZone=tz;"
        "      return Reflect.construct(target,[args[0],next],newTarget);"
        "    }"
        "  });"
        "  Intl.DateTimeFormat=Wrapped;"
        "})();";

    size_t len = (size_t)snprintf(NULL, 0, fmt, escaped) + 1;
    char* js = (char*)malloc(len);
    if (js) {
        snprintf(js, len, fmt, escaped);
    }
    free(escaped);
    return js;
}

#endif
