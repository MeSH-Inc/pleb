#ifndef WICK_PROTOCOL_H
#define WICK_PROTOCOL_H

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "browser_environment.h"

#define WICK_RENDERER_PROTOCOL_VERSION "v5-meta"
#define WICK_MAX_REQUEST_LINE_BYTES (64 * 1024)

__attribute__((used)) static const char WICK_RENDERER_PROTOCOL[] =
    "WICK_RENDERER_PROTOCOL=" WICK_RENDERER_PROTOCOL_VERSION;

typedef struct {
    char* url;
    char* wait_for_selector;
    int timeout_ms;
} WickRenderRequest;

static void wick_render_request_free(WickRenderRequest* req) {
    if (!req) return;
    free(req->url);
    free(req->wait_for_selector);
    req->url = NULL;
    req->wait_for_selector = NULL;
    req->timeout_ms = 0;
}

static char* wick_asprintf(const char* fmt, ...) {
    va_list ap1;
    va_start(ap1, fmt);
    int needed = vsnprintf(NULL, 0, fmt, ap1);
    va_end(ap1);
    if (needed < 0) return NULL;

    char* buf = (char*)malloc((size_t)needed + 1);
    if (!buf) return NULL;

    va_list ap2;
    va_start(ap2, fmt);
    vsnprintf(buf, (size_t)needed + 1, fmt, ap2);
    va_end(ap2);
    return buf;
}

// Reads one line up to `max_bytes`. Returns 1 if a line was read (or exceeded
// `max_bytes`, setting *out_too_long = 1 and consuming through '\n'), 0 on EOF.
static int wick_read_line(FILE* in, char** out_buf, size_t max_bytes, int* out_too_long) {
    *out_buf = NULL;
    *out_too_long = 0;

    size_t cap = 512;
    size_t len = 0;
    char* buf = (char*)malloc(cap);
    if (!buf) return 0;

    int ch;
    while ((ch = fgetc(in)) != EOF) {
        if (ch == '\n') {
            break;
        }
        if (len >= max_bytes) {
            *out_too_long = 1;
            while ((ch = fgetc(in)) != EOF && ch != '\n') {}
            free(buf);
            return 1;
        }
        if (len + 2 > cap) {
            size_t next_cap = cap * 2;
            if (next_cap > max_bytes + 1) next_cap = max_bytes + 1;
            char* grown = (char*)realloc(buf, next_cap);
            if (!grown) {
                free(buf);
                return 0;
            }
            buf = grown;
            cap = next_cap;
        }
        buf[len++] = (char)ch;
    }

    if (ch == EOF && len == 0) {
        free(buf);
        return 0;
    }

    while (len > 0 && buf[len - 1] == '\r') {
        len--;
    }
    buf[len] = '\0';
    *out_buf = buf;
    return 1;
}

static const char* wick_skip_ws(const char* p) {
    while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') {
        p++;
    }
    return p;
}

static int wick_hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return 10 + (c - 'a');
    if (c >= 'A' && c <= 'F') return 10 + (c - 'A');
    return -1;
}

static int wick_parse_hex4(const char* p, unsigned int* out) {
    unsigned int val = 0;
    for (int i = 0; i < 4; i++) {
        int d = wick_hex_digit(p[i]);
        if (d < 0) return -1;
        val = (val << 4) | (unsigned int)d;
    }
    *out = val;
    return 0;
}

static int wick_append_utf8(char* dst, size_t* len, size_t cap, unsigned int cp) {
    if (cp == 0) return -1; // Reject embedded NUL for C strings.
    if (cp <= 0x7F) {
        if (*len + 1 >= cap) return -1;
        dst[(*len)++] = (char)cp;
    } else if (cp <= 0x7FF) {
        if (*len + 2 >= cap) return -1;
        dst[(*len)++] = (char)(0xC0 | (cp >> 6));
        dst[(*len)++] = (char)(0x80 | (cp & 0x3F));
    } else if (cp <= 0xFFFF) {
        if (*len + 3 >= cap) return -1;
        dst[(*len)++] = (char)(0xE0 | (cp >> 12));
        dst[(*len)++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        dst[(*len)++] = (char)(0x80 | (cp & 0x3F));
    } else if (cp <= 0x10FFFF) {
        if (*len + 4 >= cap) return -1;
        dst[(*len)++] = (char)(0xF0 | (cp >> 18));
        dst[(*len)++] = (char)(0x80 | ((cp >> 12) & 0x3F));
        dst[(*len)++] = (char)(0x80 | ((cp >> 6) & 0x3F));
        dst[(*len)++] = (char)(0x80 | (cp & 0x3F));
    } else {
        return -1;
    }
    return 0;
}

static int wick_parse_json_string(const char** cursor, char** out_str, char* err_buf, size_t err_cap) {
    const char* p = *cursor;
    if (*p != '"') {
        snprintf(err_buf, err_cap, "expected JSON string");
        return -1;
    }
    p++;

    // Decoded UTF-8 is always <= raw JSON string length (+4 margin for safety).
    size_t raw_cap = strlen(p) + 4;
    char* buf = (char*)malloc(raw_cap);
    if (!buf) {
        snprintf(err_buf, err_cap, "out of memory parsing JSON string");
        return -1;
    }
    size_t out_len = 0;

    while (*p) {
        unsigned char c = (unsigned char)*p;
        if (c == '"') {
            p++;
            buf[out_len] = '\0';
            *cursor = p;
            *out_str = buf;
            return 0;
        }
        if (c < 0x20) {
            free(buf);
            snprintf(err_buf, err_cap, "unescaped control character in JSON string");
            return -1;
        }
        if (c != '\\') {
            if (out_len + 1 >= raw_cap) {
                free(buf);
                snprintf(err_buf, err_cap, "JSON string buffer overflow");
                return -1;
            }
            buf[out_len++] = (char)c;
            p++;
            continue;
        }

        p++;
        char esc = *p++;
        switch (esc) {
            case '"': buf[out_len++] = '"'; break;
            case '\\': buf[out_len++] = '\\'; break;
            case '/': buf[out_len++] = '/'; break;
            case 'b': buf[out_len++] = '\b'; break;
            case 'f': buf[out_len++] = '\f'; break;
            case 'n': buf[out_len++] = '\n'; break;
            case 'r': buf[out_len++] = '\r'; break;
            case 't': buf[out_len++] = '\t'; break;
            case 'u': {
                unsigned int cp = 0;
                if (wick_parse_hex4(p, &cp) != 0) {
                    free(buf);
                    snprintf(err_buf, err_cap, "invalid \\uXXXX escape");
                    return -1;
                }
                p += 4;
                if (cp >= 0xD800 && cp <= 0xDBFF) {
                    if (p[0] != '\\' || p[1] != 'u') {
                        free(buf);
                        snprintf(err_buf, err_cap, "unpaired UTF-16 high surrogate");
                        return -1;
                    }
                    p += 2;
                    unsigned int low = 0;
                    if (wick_parse_hex4(p, &low) != 0 || low < 0xDC00 || low > 0xDFFF) {
                        free(buf);
                        snprintf(err_buf, err_cap, "invalid UTF-16 low surrogate");
                        return -1;
                    }
                    p += 4;
                    cp = 0x10000u + (((cp - 0xD800u) << 10) | (low - 0xDC00u));
                } else if (cp >= 0xDC00 && cp <= 0xDFFF) {
                    free(buf);
                    snprintf(err_buf, err_cap, "unexpected UTF-16 low surrogate");
                    return -1;
                }
                if (wick_append_utf8(buf, &out_len, raw_cap, cp) != 0) {
                    free(buf);
                    snprintf(err_buf, err_cap, "invalid Unicode codepoint in JSON string");
                    return -1;
                }
                break;
            }
            default:
                free(buf);
                snprintf(err_buf, err_cap, "unsupported JSON escape \\%c", esc ? esc : '?');
                return -1;
        }
    }

    free(buf);
    snprintf(err_buf, err_cap, "unterminated JSON string");
    return -1;
}

static int wick_parse_request_line(
    const char* line,
    WickRenderRequest* out,
    char* err_buf,
    size_t err_cap
) {
    memset(out, 0, sizeof(*out));
    out->timeout_ms = 60000;

    const char* p = wick_skip_ws(line);
    if (*p != '{') {
        snprintf(err_buf, err_cap, "request must be a JSON object");
        return -1;
    }
    p = wick_skip_ws(p + 1);
    if (*p == '}') {
        snprintf(err_buf, err_cap, "request object missing required field 'url'");
        return -1;
    }

    for (;;) {
        char* key = NULL;
        if (wick_parse_json_string(&p, &key, err_buf, err_cap) != 0) {
            wick_render_request_free(out);
            return -1;
        }
        p = wick_skip_ws(p);
        if (*p != ':') {
            free(key);
            wick_render_request_free(out);
            snprintf(err_buf, err_cap, "expected ':' after JSON key");
            return -1;
        }
        p = wick_skip_ws(p + 1);

        if (strcmp(key, "url") == 0) {
            char* val = NULL;
            if (wick_parse_json_string(&p, &val, err_buf, err_cap) != 0) {
                free(key);
                wick_render_request_free(out);
                return -1;
            }
            free(out->url);
            out->url = val;
        } else if (strcmp(key, "wait_for_selector") == 0) {
            if (strncmp(p, "null", 4) == 0) {
                p += 4;
                free(out->wait_for_selector);
                out->wait_for_selector = NULL;
            } else {
                char* val = NULL;
                if (wick_parse_json_string(&p, &val, err_buf, err_cap) != 0) {
                    free(key);
                    wick_render_request_free(out);
                    return -1;
                }
                free(out->wait_for_selector);
                out->wait_for_selector = (val[0] != '\0') ? val : NULL;
                if (!out->wait_for_selector) free(val);
            }
        } else if (strcmp(key, "timeout_ms") == 0) {
            char* endptr = NULL;
            long val = strtol(p, &endptr, 10);
            if (endptr == p || val <= 0) {
                free(key);
                wick_render_request_free(out);
                snprintf(err_buf, err_cap, "timeout_ms must be a positive integer");
                return -1;
            }
            if (val > 600000) val = 600000;
            out->timeout_ms = (int)val;
            p = endptr;
        } else {
            snprintf(err_buf, err_cap, "unknown request field '%s'", key);
            free(key);
            wick_render_request_free(out);
            return -1;
        }

        free(key);
        p = wick_skip_ws(p);
        if (*p == ',') {
            p = wick_skip_ws(p + 1);
            continue;
        }
        if (*p == '}') {
            p = wick_skip_ws(p + 1);
            break;
        }
        wick_render_request_free(out);
        snprintf(err_buf, err_cap, "expected ',' or '}' in request object");
        return -1;
    }

    if (*p != '\0') {
        wick_render_request_free(out);
        snprintf(err_buf, err_cap, "trailing characters after JSON request object");
        return -1;
    }

    if (!out->url || out->url[0] == '\0') {
        wick_render_request_free(out);
        snprintf(err_buf, err_cap, "request field 'url' must be a non-empty string");
        return -1;
    }

    return 0;
}

static void wick_write_json_escaped(FILE* out, const char* s) {
    if (!s) {
        fputs("null", out);
        return;
    }
    fputc('"', out);
    for (const unsigned char* p = (const unsigned char*)s; *p; p++) {
        unsigned char c = *p;
        switch (c) {
            case '"': fputs("\\\"", out); break;
            case '\\': fputs("\\\\", out); break;
            case '\b': fputs("\\b", out); break;
            case '\f': fputs("\\f", out); break;
            case '\n': fputs("\\n", out); break;
            case '\r': fputs("\\r", out); break;
            case '\t': fputs("\\t", out); break;
            default:
                if (c < 0x20) {
                    fprintf(out, "\\u%04x", c);
                } else {
                    fputc((int)c, out);
                }
        }
    }
    fputc('"', out);
}

static void wick_write_ready(FILE* out, int ok, const char* error) {
    fprintf(out, "{\"ready\":%s,\"protocol\":\"%s\"",
            ok ? "true" : "false", WICK_RENDERER_PROTOCOL_VERSION);
    if (error && error[0]) {
        fputs(",\"error\":", out);
        wick_write_json_escaped(out, error);
    } else {
        fputs(",\"error\":null", out);
    }
    fputs("}\n", out);
    fflush(out);
}

static void wick_write_response(
    FILE* out,
    int status_code,
    const char* final_url,
    const char* html,
    size_t html_len,
    const char* error,
    const char* error_kind
) {
    fprintf(out, "{\"status_code\":%d,\"final_url\":", status_code);
    wick_write_json_escaped(out, final_url ? final_url : "");
    fprintf(out, ",\"html_bytes\":%zu,\"error\":", (error && error[0]) ? (size_t)0 : html_len);
    wick_write_json_escaped(out, (error && error[0]) ? error : NULL);
    fputs(",\"error_kind\":", out);
    wick_write_json_escaped(out, (error_kind && error_kind[0]) ? error_kind : NULL);
    fputs("}\n", out);
    if ((!error || !error[0]) && html && html_len > 0) {
        fwrite(html, 1, html_len, out);
    }
    fflush(out);
}

static void wick_clamp_timeouts(
    int timeout_ms,
    int has_selector,
    int* total_cap,
    int* post_load_cap,
    int* js_cap,
    int* cf_cap
) {
    int default_total = has_selector ? 60000 : 45000;
    int default_post = has_selector ? 30000 : 12000;
    int default_js = has_selector ? 20000 : 8000;
    int default_cf = 30000;

    int budget = (timeout_ms > 2000) ? (timeout_ms - 1500) : ((timeout_ms * 3) / 4);
    if (budget < 200) budget = 200;

    int eff_total = (budget < default_total) ? budget : default_total;
    int eff_post = (eff_total < default_post) ? eff_total : default_post;
    int eff_js = (eff_post < default_js) ? eff_post : default_js;
    int eff_cf = (eff_total < default_cf) ? eff_total : default_cf;

    *total_cap = eff_total;
    *post_load_cap = eff_post;
    *js_cap = eff_js;
    *cf_cap = eff_cf;
}

static char* wick_build_poller_js(
    const char* selector_or_null,
    const char* marker,
    const char* sel_err_prefix,
    int js_cap_ms,
    int cf_cap_ms
) {
    char* escaped_sel = NULL;
    const char* sel_expr = "null";
    char* quoted_sel = NULL;
    if (selector_or_null && selector_or_null[0]) {
        escaped_sel = wick_escape_js_string(selector_or_null);
        if (!escaped_sel) return NULL;
        quoted_sel = wick_asprintf("'%s'", escaped_sel);
        free(escaped_sel);
        if (!quoted_sel) return NULL;
        sel_expr = quoted_sel;
    }

    char* js = wick_asprintf(
        "(function(){"
        "var sel=%s;"
        "var last=0,stable=0,t=0;"
        "var iv=setInterval(function(){"
        "  var body=document.body;"
        "  var bt=body?body.innerText:'';"
        "  var len=bt.length;"
        "  var blt=bt.toLowerCase();"
        "  var isCF=blt.indexOf('performing security verification')>=0||"
        "           blt.indexOf('checking your browser')>=0;"
        "  t+=300;"
        "  if(isCF){stable=0;last=len;}"
        "  else if(len>500&&len===last){stable+=300;}"
        "  else{stable=0;last=len;}"
        "  var hit=true;"
        "  if(sel){"
        "    try{hit=!!document.querySelector(sel);}"
        "    catch(e){"
        "      clearInterval(iv);"
        "      console.log('%s'+(e&&e.message?e.message:String(e)));"
        "      return;"
        "    }"
        "  }"
        "  var stableHit=!isCF&&len>500&&stable>=1000&&hit;"
        "  var cap=isCF?%d:%d;"
        "  if(stableHit||t>=cap){"
        "    clearInterval(iv);"
        "    console.log('%s'+(document.documentElement?document.documentElement.outerHTML:''));"
        "  }"
        "},300);"
        "})();",
        sel_expr,
        sel_err_prefix,
        cf_cap_ms,
        js_cap_ms,
        marker
    );

    free(quoted_sel);
    return js;
}

#endif
