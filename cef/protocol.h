#ifndef PLEB_PROTOCOL_H
#define PLEB_PROTOCOL_H

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "browser_environment.h"
#include "reveal_script.h"

#define PLEB_RENDERER_PROTOCOL_VERSION "v6-meta"
#define PLEB_MAX_REQUEST_LINE_BYTES (64 * 1024)
#define PLEB_MAX_SCROLL_STEPS 20
#define PLEB_MAX_REVEAL_META_BYTES (4 * 1024 * 1024)

__attribute__((used)) static const char PLEB_RENDERER_PROTOCOL[] =
    "PLEB_RENDERER_PROTOCOL=" PLEB_RENDERER_PROTOCOL_VERSION;

typedef struct {
    char* url;
    char* wait_for_selector;
    int timeout_ms;
    char* reveal;
    int scroll;
} PlebRenderRequest;

static void pleb_render_request_free(PlebRenderRequest* req) {
    if (!req) return;
    free(req->url);
    free(req->wait_for_selector);
    free(req->reveal);
    req->url = NULL;
    req->wait_for_selector = NULL;
    req->reveal = NULL;
    req->timeout_ms = 0;
    req->scroll = 0;
}

static char* pleb_asprintf(const char* fmt, ...) {
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

static char* pleb_strdup(const char* s) {
    if (!s) return NULL;
    size_t len = strlen(s);
    char* copy = (char*)malloc(len + 1);
    if (copy) memcpy(copy, s, len + 1);
    return copy;
}

// Reads one line up to `max_bytes`. Returns 1 if a line was read (or exceeded
// `max_bytes`, setting *out_too_long = 1 and consuming through '\n'), 0 on EOF.
static int pleb_read_line(FILE* in, char** out_buf, size_t max_bytes, int* out_too_long) {
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

static const char* pleb_skip_ws(const char* p) {
    while (*p == ' ' || *p == '\t' || *p == '\r' || *p == '\n') {
        p++;
    }
    return p;
}

static int pleb_hex_digit(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return 10 + (c - 'a');
    if (c >= 'A' && c <= 'F') return 10 + (c - 'A');
    return -1;
}

static int pleb_parse_hex4(const char* p, unsigned int* out) {
    unsigned int val = 0;
    for (int i = 0; i < 4; i++) {
        int d = pleb_hex_digit(p[i]);
        if (d < 0) return -1;
        val = (val << 4) | (unsigned int)d;
    }
    *out = val;
    return 0;
}

static int pleb_append_utf8(char* dst, size_t* len, size_t cap, unsigned int cp) {
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

static int pleb_parse_json_string(const char** cursor, char** out_str, char* err_buf, size_t err_cap) {
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
                if (pleb_parse_hex4(p, &cp) != 0) {
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
                    if (pleb_parse_hex4(p, &low) != 0 || low < 0xDC00 || low > 0xDFFF) {
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
                if (pleb_append_utf8(buf, &out_len, raw_cap, cp) != 0) {
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

static int pleb_parse_request_line(
    const char* line,
    PlebRenderRequest* out,
    char* err_buf,
    size_t err_cap
) {
    memset(out, 0, sizeof(*out));
    out->timeout_ms = 60000;

    const char* p = pleb_skip_ws(line);
    if (*p != '{') {
        snprintf(err_buf, err_cap, "request must be a JSON object");
        return -1;
    }
    p = pleb_skip_ws(p + 1);
    if (*p == '}') {
        snprintf(err_buf, err_cap, "request object missing required field 'url'");
        return -1;
    }

    for (;;) {
        char* key = NULL;
        if (pleb_parse_json_string(&p, &key, err_buf, err_cap) != 0) {
            pleb_render_request_free(out);
            return -1;
        }
        p = pleb_skip_ws(p);
        if (*p != ':') {
            free(key);
            pleb_render_request_free(out);
            snprintf(err_buf, err_cap, "expected ':' after JSON key");
            return -1;
        }
        p = pleb_skip_ws(p + 1);

        if (strcmp(key, "url") == 0) {
            char* val = NULL;
            if (pleb_parse_json_string(&p, &val, err_buf, err_cap) != 0) {
                free(key);
                pleb_render_request_free(out);
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
                if (pleb_parse_json_string(&p, &val, err_buf, err_cap) != 0) {
                    free(key);
                    pleb_render_request_free(out);
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
                pleb_render_request_free(out);
                snprintf(err_buf, err_cap, "timeout_ms must be a positive integer");
                return -1;
            }
            if (val > 600000) val = 600000;
            out->timeout_ms = (int)val;
            p = endptr;
        } else if (strcmp(key, "reveal") == 0) {
            if (strncmp(p, "null", 4) == 0) {
                p += 4;
                free(out->reveal);
                out->reveal = NULL;
            } else {
                char* val = NULL;
                if (pleb_parse_json_string(&p, &val, err_buf, err_cap) != 0) {
                    free(key);
                    pleb_render_request_free(out);
                    return -1;
                }
                if (strcmp(val, "none") != 0 && strcmp(val, "tabs") != 0
                    && strcmp(val, "disclosures") != 0 && strcmp(val, "all") != 0) {
                    free(val);
                    free(key);
                    pleb_render_request_free(out);
                    snprintf(err_buf, err_cap, "reveal must be one of none, tabs, disclosures, all");
                    return -1;
                }
                free(out->reveal);
                out->reveal = val;
            }
        } else if (strcmp(key, "scroll") == 0) {
            char* endptr = NULL;
            long val = strtol(p, &endptr, 10);
            if (endptr == p || val < 0 || val > PLEB_MAX_SCROLL_STEPS) {
                free(key);
                pleb_render_request_free(out);
                snprintf(err_buf, err_cap, "scroll must be an integer between 0 and %d", PLEB_MAX_SCROLL_STEPS);
                return -1;
            }
            out->scroll = (int)val;
            p = endptr;
        } else {
            snprintf(err_buf, err_cap, "unknown request field '%s'", key);
            free(key);
            pleb_render_request_free(out);
            return -1;
        }

        free(key);
        p = pleb_skip_ws(p);
        if (*p == ',') {
            p = pleb_skip_ws(p + 1);
            continue;
        }
        if (*p == '}') {
            p = pleb_skip_ws(p + 1);
            break;
        }
        pleb_render_request_free(out);
        snprintf(err_buf, err_cap, "expected ',' or '}' in request object");
        return -1;
    }

    if (*p != '\0') {
        pleb_render_request_free(out);
        snprintf(err_buf, err_cap, "trailing characters after JSON request object");
        return -1;
    }

    if (!out->url || out->url[0] == '\0') {
        pleb_render_request_free(out);
        snprintf(err_buf, err_cap, "request field 'url' must be a non-empty string");
        return -1;
    }

    if (!out->reveal) {
        out->reveal = pleb_strdup("none");
        if (!out->reveal) {
            pleb_render_request_free(out);
            snprintf(err_buf, err_cap, "out of memory");
            return -1;
        }
    }

    return 0;
}

static void pleb_write_json_escaped(FILE* out, const char* s) {
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

static void pleb_write_ready(FILE* out, int ok, const char* error) {
    fprintf(out, "{\"ready\":%s,\"protocol\":\"%s\"",
            ok ? "true" : "false", PLEB_RENDERER_PROTOCOL_VERSION);
    if (error && error[0]) {
        fputs(",\"error\":", out);
        pleb_write_json_escaped(out, error);
    } else {
        fputs(",\"error\":null", out);
    }
    fputs("}\n", out);
    fflush(out);
}

static void pleb_write_response(
    FILE* out,
    int status_code,
    const char* final_url,
    const char* html,
    size_t html_len,
    const char* meta_json,
    size_t meta_len,
    const char* error,
    const char* error_kind
) {
    int has_err = (error && error[0]) ? 1 : 0;
    fprintf(out, "{\"status_code\":%d,\"final_url\":", status_code);
    pleb_write_json_escaped(out, final_url ? final_url : "");
    fprintf(out, ",\"html_bytes\":%zu,\"meta_bytes\":%zu,\"error\":",
            has_err ? (size_t)0 : html_len,
            (has_err || !meta_json) ? (size_t)0 : meta_len);
    pleb_write_json_escaped(out, has_err ? error : NULL);
    fputs(",\"error_kind\":", out);
    pleb_write_json_escaped(out, (error_kind && error_kind[0]) ? error_kind : NULL);
    fputs("}\n", out);
    if (!has_err && html && html_len > 0) {
        fwrite(html, 1, html_len, out);
    }
    if (!has_err && meta_json && meta_len > 0) {
        fwrite(meta_json, 1, meta_len, out);
    }
    fflush(out);
}

static void pleb_clamp_timeouts(
    int timeout_ms,
    int has_selector,
    int has_reveal,
    int* total_cap,
    int* post_load_cap,
    int* js_cap
) {
    int needs_extended = has_selector || has_reveal;
    int default_total = needs_extended ? 60000 : 45000;
    int default_post = needs_extended ? 30000 : 12000;
    int default_js = has_selector ? 20000 : 8000;

    int budget = (timeout_ms > 2000) ? (timeout_ms - 1500) : ((timeout_ms * 3) / 4);
    if (budget < 200) budget = 200;

    int eff_total = (budget < default_total) ? budget : default_total;
    int eff_post = (eff_total < default_post) ? eff_total : default_post;
    int eff_js = (eff_post < default_js) ? eff_post : default_js;

    *total_cap = eff_total;
    *post_load_cap = eff_post;
    *js_cap = eff_js;
}

static char* pleb_build_poller_js(
    const char* selector_or_null,
    const char* marker,
    const char* sel_err_prefix,
    const char* meta_marker,
    const char* pre_reveal_marker,
    const char* reveal_mode_or_null,
    int scroll_steps,
    int js_cap_ms,
    int reveal_budget_ms
) {
    char* escaped_sel = NULL;
    const char* sel_expr = "null";
    char* quoted_sel = NULL;
    if (selector_or_null && selector_or_null[0]) {
        escaped_sel = pleb_escape_js_string(selector_or_null);
        if (!escaped_sel) return NULL;
        quoted_sel = pleb_asprintf("'%s'", escaped_sel);
        free(escaped_sel);
        if (!quoted_sel) return NULL;
        sel_expr = quoted_sel;
    }

    const char* eff_reveal = (reveal_mode_or_null && reveal_mode_or_null[0])
        ? reveal_mode_or_null : "none";
    int clamped_scroll = (scroll_steps < 0) ? 0
        : (scroll_steps > PLEB_MAX_SCROLL_STEPS ? PLEB_MAX_SCROLL_STEPS : scroll_steps);
    int has_reveal = (strcmp(eff_reveal, "none") != 0 || clamped_scroll > 0) ? 1 : 0;
    const char* reveal_lib = has_reveal ? PLEB_REVEAL_SCRIPT_JS : "";
    int r_budget = (reveal_budget_ms < 200) ? 200
        : (reveal_budget_ms > 15000 ? 15000 : reveal_budget_ms);

    char* js = pleb_asprintf(
        "(function(){"
        "if(window.__plebPollerActive)return;"
        "window.__plebPollerActive=true;"
        "%s\n"
        "var sel=%s;"
        "var revealMode='%s',scrollSteps=%d,hasReveal=%d,revealBudgetMs=%d;"
        "var metaMarker='%s',preRevealMarker='%s';"
        "var start=Date.now(),lastMut=start,selMatchedAt=0,done=false,scheduled=false,revealStarted=false;"
        "var QUIET_MS=180;"
        "var obs=null,iv=null;"
        "var cleanup=function(){"
        "  if(iv){clearInterval(iv);iv=null;}"
        "  if(obs){try{obs.disconnect();}catch(_){}obs=null;}"
        "  window.__plebPollerActive=false;"
        "};"
        "var checkChallenge=function(blt){"
        "  if(blt.length<4000&&(blt.indexOf('performing security verification')>=0||blt.indexOf('checking your browser')>=0||blt.indexOf('verify you are human')>=0||blt.indexOf('just a moment')>=0))return true;"
        "  if(document.querySelector&&document.querySelector('#cf-wrapper, iframe[src*=\"challenges.cloudflare.com\"], iframe[src*=\"turnstile\"], iframe[src*=\"hcaptcha\"], iframe[src*=\"recaptcha\"]'))return true;"
        "  return false;"
        "};"
        "var dump=function(){"
        "  if(done)return;"
        "  done=true;"
        "  cleanup();"
        "  var curHtml=(document.documentElement?document.documentElement.outerHTML:'');"
        "  var body=document.body;"
        "  var bt=body&&body.innerText?body.innerText:'';"
        "  if(!hasReveal||checkChallenge(bt.toLowerCase())){"
        "    console.log('%s'+curHtml);"
        "    return;"
        "  }"
        "  if(revealStarted)return;"
        "  revealStarted=true;"
        "  if(preRevealMarker){console.log(preRevealMarker+curHtml);}"
        "  __plebRevealAndScroll({"
        "    mode:revealMode,"
        "    scrollSteps:scrollSteps,"
        "    maxActions:16,"
        "    maxPerGroup:8,"
        "    maxPanelBytes:262144,"
        "    maxTotalBytes:2097152,"
        "    budgetMs:revealBudgetMs"
        "  }).then(function(res){"
        "    res=res||{};"
        "    var outHtml=(res.navigated)?curHtml:(document.documentElement?document.documentElement.outerHTML:'');"
        "    var panels=(res.navigated||res.challenge_detected)?[]:(res.revealed_panels||[]);"
        "    var warn=res.warning||(res.navigated?'aborted --reveal because a control triggered page navigation':null);"
        "    if(metaMarker){"
        "      console.log(metaMarker+JSON.stringify({"
        "        revealed_panels:panels,"
        "        reveal_capped:!!res.reveal_capped,"
        "        scroll_steps_performed:res.scroll_steps_performed||0,"
        "        reveal_warning:warn"
        "      }));"
        "    }"
        "    console.log('%s'+outHtml);"
        "  }).catch(function(e){"
        "    if(metaMarker){"
        "      console.log(metaMarker+JSON.stringify({"
        "        revealed_panels:[],"
        "        reveal_capped:false,"
        "        scroll_steps_performed:0,"
        "        reveal_warning:'reveal script exception: '+(e&&e.message?e.message:String(e))"
        "      }));"
        "    }"
        "    console.log('%s'+curHtml);"
        "  });"
        "};"
        "var finishAfterRaf=function(){"
        "  if(done||scheduled)return;"
        "  scheduled=true;"
        "  if(typeof requestAnimationFrame==='function'){"
        "    requestAnimationFrame(dump);"
        "  }else{"
        "    dump();"
        "  }"
        "};"
        "if(typeof MutationObserver==='function'){"
        "  try{"
        "    var root=document.documentElement||document.body;"
        "    if(root){"
        "      obs=new MutationObserver(function(){lastMut=Date.now();});"
        "      obs.observe(root,{childList:true,subtree:true,characterData:true});"
        "    }"
        "  }catch(_){}"
        "}"
        "var check=function(){"
        "  if(done)return;"
        "  var now=Date.now();"
        "  var body=document.body;"
        "  var bt=body&&body.innerText?body.innerText:'';"
        "  var blt=bt.toLowerCase();"
        "  if(checkChallenge(blt)){dump();return;}"
        "  if(now-start>=%d){dump();return;}"
        "  if(sel){"
        "    var el=null;"
        "    try{el=document.querySelector(sel);}"
        "    catch(e){"
        "      done=true;cleanup();"
        "      console.log('%s'+(e&&e.message?e.message:String(e)));"
        "      return;"
        "    }"
        "    if(el){"
        "      if(!selMatchedAt)selMatchedAt=now;"
        "      var hasContent=((el.innerText||'').trim().length>0)||"
        "                     (el.children&&el.children.length>0)||"
        "                     (now-selMatchedAt>=350);"
        "      if(hasContent){finishAfterRaf();return;}"
        "    }"
        "    return;"
        "  }"
        "  var spa=document.querySelector('#root,#app,#__next,#__nuxt,app-root,[data-reactroot]');"
        "  var hasEmptySpaRoot=(!!spa&&((spa.innerText||'').trim().length<50))||"
        "                      (bt.trim().length===0&&document.scripts&&document.scripts.length>0);"
        "  var inflight=(typeof window.__plebInflight==='number')?window.__plebInflight:0;"
        "  if(hasEmptySpaRoot||inflight>0){lastMut=now;return;}"
        "  if(now-lastMut>=QUIET_MS){finishAfterRaf();}"
        "};"
        "iv=setInterval(check,40);"
        "})();",
        reveal_lib,
        sel_expr,
        eff_reveal,
        clamped_scroll,
        has_reveal,
        r_budget,
        meta_marker ? meta_marker : "",
        pre_reveal_marker ? pre_reveal_marker : "",
        marker,
        marker,
        marker,
        js_cap_ms,
        sel_err_prefix
    );

    free(quoted_sel);
    return js;
}

#endif
