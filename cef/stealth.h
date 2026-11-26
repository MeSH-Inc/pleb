#ifndef WICK_STEALTH_H
#define WICK_STEALTH_H

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char* STEALTH_JS =
    "(function() {"
    // Idempotency guard: if webdriver is already locked down non-configurable,
    // stealth patches have already executed in this V8 context.
    "  try {"
    "    var wd = Object.getOwnPropertyDescriptor(navigator, 'webdriver');"
    "    if (wd && wd.configurable === false) return;"
    "  } catch (e) {}"

    // Helper: make spoofed functions report [native code] via Function.prototype.toString.
    // Kept strictly local inside this closure so nothing leaks onto window.
    "  var nativeFn = Function.prototype.toString;"
    "  var fakeNatives = new WeakSet();"
    "  function markNative(fn) { fakeNatives.add(fn); return fn; }"
    "  Function.prototype.toString = function() {"
    "    if (fakeNatives.has(this)) return 'function ' + (this.name || '') + '() { [native code] }';"
    "    return nativeFn.call(this);"
    "  };"
    "  markNative(Function.prototype.toString);"

    // 1. chrome.runtime (Cloudflare's primary check)
    "  if (!window.chrome) window.chrome = {};"
    "  if (!window.chrome.runtime) {"
    "    window.chrome.runtime = {"
    "      OnInstalledReason: {CHROME_UPDATE:'chrome_update',INSTALL:'install',SHARED_MODULE_UPDATE:'shared_module_update',UPDATE:'update'},"
    "      OnRestartRequiredReason: {APP_UPDATE:'app_update',OS_UPDATE:'os_update',PERIODIC:'periodic'},"
    "      PlatformArch: {ARM:'arm',ARM64:'arm64',MIPS:'mips',MIPS64:'mips64',X86_32:'x86-32',X86_64:'x86-64'},"
    "      PlatformOs: {ANDROID:'android',CROS:'cros',LINUX:'linux',MAC:'mac',OPENBSD:'openbsd',WIN:'win'},"
    "      RequestUpdateCheckStatus: {NO_UPDATE:'no_update',THROTTLED:'throttled',UPDATE_AVAILABLE:'update_available'},"
    "      connect: markNative(function connect() { return {onDisconnect:{addListener:function(){}},onMessage:{addListener:function(){}},postMessage:function(){}}; }),"
    "      sendMessage: markNative(function sendMessage() {}),"
    "      id: undefined"
    "    };"
    "  }"

    // 2. chrome.app (secondary Cloudflare/BotD check)
    "  if (!window.chrome.app) {"
    "    window.chrome.app = {"
    "      isInstalled: false,"
    "      InstallState: {DISABLED:'disabled',INSTALLED:'installed',NOT_INSTALLED:'not_installed'},"
    "      RunningState: {CANNOT_RUN:'cannot_run',READY_TO_RUN:'ready_to_run',RUNNING:'running'},"
    "      getDetails: markNative(function getDetails() { return null; }),"
    "      getIsInstalled: markNative(function getIsInstalled() { return false; }),"
    "      runningState: markNative(function runningState() { return 'cannot_run'; })"
    "    };"
    "  }"

    // 3. navigator.plugins (with PluginArray prototype intact)
    "  try {"
    "    Object.defineProperty(navigator, 'plugins', {"
    "      get: markNative(function plugins() {"
    "        var p = ["
    "          {name:'Chrome PDF Plugin',filename:'internal-pdf-viewer',description:'Portable Document Format',length:1,0:{type:'application/x-google-chrome-pdf',suffixes:'pdf',description:'Portable Document Format',enabledPlugin:null}},"
    "          {name:'Chrome PDF Viewer',filename:'mhjfbmdgcfjbbpaeojofohoefgiehjai',description:'',length:1,0:{type:'application/pdf',suffixes:'pdf',description:'',enabledPlugin:null}},"
    "          {name:'Native Client',filename:'internal-nacl-plugin',description:'',length:2,0:{type:'application/x-nacl',suffixes:'',description:'Native Client Executable',enabledPlugin:null},1:{type:'application/x-pnacl',suffixes:'',description:'Portable Native Client Executable',enabledPlugin:null}}"
    "        ];"
    "        p.item = function(i) { return this[i] || null; };"
    "        p.namedItem = function(n) { for(var i=0;i<this.length;i++) if(this[i].name===n) return this[i]; return null; };"
    "        p.refresh = function() {};"
    "        if (typeof PluginArray !== 'undefined' && PluginArray.prototype) {"
    "          try { Object.setPrototypeOf(p, PluginArray.prototype); } catch (e) {}"
    "        }"
    "        return p;"
    "      }),"
    "      configurable: false"
    "    });"
    "  } catch (e) {}"

    // 4. navigator.mimeTypes
    "  try {"
    "    Object.defineProperty(navigator, 'mimeTypes', {"
    "      get: markNative(function mimeTypes() {"
    "        var m = ["
    "          {type:'application/pdf',suffixes:'pdf',description:'',enabledPlugin:{name:'Chrome PDF Viewer'}},"
    "          {type:'application/x-google-chrome-pdf',suffixes:'pdf',description:'Portable Document Format',enabledPlugin:{name:'Chrome PDF Plugin'}},"
    "          {type:'application/x-nacl',suffixes:'',description:'Native Client Executable',enabledPlugin:{name:'Native Client'}},"
    "          {type:'application/x-pnacl',suffixes:'',description:'Portable Native Client Executable',enabledPlugin:{name:'Native Client'}}"
    "        ];"
    "        m.item = function(i) { return this[i] || null; };"
    "        m.namedItem = function(n) { for(var i=0;i<this.length;i++) if(this[i].type===n) return this[i]; return null; };"
    "        if (typeof MimeTypeArray !== 'undefined' && MimeTypeArray.prototype) {"
    "          try { Object.setPrototypeOf(m, MimeTypeArray.prototype); } catch (e) {}"
    "        }"
    "        return m;"
    "      }),"
    "      configurable: false"
    "    });"
    "  } catch (e) {}"

    // 5. chrome.csi / chrome.loadTimes (legacy APIs)
    "  window.chrome.csi = markNative(function csi() {"
    "    return {startE:Date.now(),onloadT:Date.now(),pageT:Date.now()/1000,tran:15};"
    "  });"
    "  window.chrome.loadTimes = markNative(function loadTimes() {"
    "    return {"
    "      get requestTime(){return Date.now()/1000},"
    "      get startLoadTime(){return Date.now()/1000},"
    "      get commitLoadTime(){return Date.now()/1000},"
    "      get finishDocumentLoadTime(){return Date.now()/1000},"
    "      get finishLoadTime(){return Date.now()/1000},"
    "      get firstPaintTime(){return Date.now()/1000},"
    "      get firstPaintAfterLoadTime(){return 0},"
    "      get navigationType(){return 'Other'},"
    "      get wasFetchedViaSpdy(){return true},"
    "      get wasNpnNegotiated(){return true},"
    "      get npnNegotiatedProtocol(){return 'h2'},"
    "      get wasAlternateProtocolAvailable(){return false},"
    "      get connectionInfo(){return 'h2'}"
    "    };"
    "  });"

    // 6. Permissions API consistency
    "  (function() {"
    "    var origQuery = navigator.permissions && navigator.permissions.query;"
    "    if (origQuery) {"
    "      navigator.permissions.query = markNative(function query(desc) {"
    "        if (desc && desc.name === 'notifications') {"
    "          var perm = (typeof Notification !== 'undefined' && Notification.permission) ? Notification.permission : 'default';"
    "          return Promise.resolve({state: perm === 'default' ? 'prompt' : perm, onchange: null});"
    "        }"
    "        return origQuery.apply(this, arguments);"
    "      });"
    "    }"
    "  })();"

    // 7. navigator.webdriver = false
    "  try {"
    "    Object.defineProperty(navigator, 'webdriver', {"
    "      get: markNative(function webdriver() { return false; }),"
    "      configurable: false"
    "    });"
    "  } catch (e) {}"

    // 8. navigator.connection.rtt (BotD headless check)
    "  try {"
    "    if (navigator.connection && navigator.connection.rtt === 0) {"
    "      Object.defineProperty(navigator.connection, 'rtt', {get: function() { return 100; }, configurable: false});"
    "    }"
    "  } catch (e) {}"

    // 9. Remove CEF-specific globals
    "  delete window.RunPerfTest;"
    "  delete window.CefSharp;"
    "  delete window.domAutomation;"
    "  delete window.domAutomationController;"

    // 10. WebGL vendor/renderer spoofing
    "  (function() {"
    "    if (typeof WebGLRenderingContext !== 'undefined') {"
    "      var origGetParam = WebGLRenderingContext.prototype.getParameter;"
    "      WebGLRenderingContext.prototype.getParameter = markNative(function getParameter(param) {"
    "        if (param === 37445) return 'Intel Inc.';"
    "        if (param === 37446) return 'Intel Iris OpenGL Engine';"
    "        return origGetParam.call(this, param);"
    "      });"
    "    }"
    "    if (typeof WebGL2RenderingContext !== 'undefined') {"
    "      var origGetParam2 = WebGL2RenderingContext.prototype.getParameter;"
    "      WebGL2RenderingContext.prototype.getParameter = markNative(function getParameter(param) {"
    "        if (param === 37445) return 'Intel Inc.';"
    "        if (param === 37446) return 'Intel Iris OpenGL Engine';"
    "        return origGetParam2.call(this, param);"
    "      });"
    "    }"
    "  })();"

    // 11. Window dimensions + outerHeight toolbar offset for offscreen mode
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

    // 12. navigator.languages
    "  try {"
    "    if (!navigator.languages || navigator.languages.length === 0) {"
    "      Object.defineProperty(navigator, 'languages', {"
    "        get: function() { return ['en-US', 'en']; },"
    "        configurable: false"
    "      });"
    "    }"
    "  } catch (e) {}"

    // 13. navigator.deviceMemory
    "  try {"
    "    if (!navigator.deviceMemory || navigator.deviceMemory === 0) {"
    "      Object.defineProperty(navigator, 'deviceMemory', {"
    "        get: function() { return 8; },"
    "        configurable: false"
    "      });"
    "    }"
    "  } catch (e) {}"

    // 14. Notification.permission consistency
    "  try {"
    "    if (typeof Notification === 'undefined') {"
    "      window.Notification = {permission: 'default'};"
    "    } else if (Notification.permission === 'denied') {"
    "      Object.defineProperty(Notification, 'permission', {"
    "        get: function() { return 'default'; }, configurable: false"
    "      });"
    "    }"
    "  } catch (e) {}"

    // 15. navigator.productSub
    "  try {"
    "    if (navigator.productSub !== '20030107') {"
    "      Object.defineProperty(navigator, 'productSub', {"
    "        get: function() { return '20030107'; }, configurable: false"
    "      });"
    "    }"
    "  } catch (e) {}"

    // 16. screen dimensions for OSR
    "  try {"
    "    if (screen.width === 0 || screen.height === 0) {"
    "      Object.defineProperty(screen, 'width', {get: function(){return 1920;}, configurable:true});"
    "      Object.defineProperty(screen, 'height', {get: function(){return 1080;}, configurable:true});"
    "      Object.defineProperty(screen, 'availWidth', {get: function(){return 1920;}, configurable:true});"
    "      Object.defineProperty(screen, 'availHeight', {get: function(){return 1040;}, configurable:true});"
    "      Object.defineProperty(screen, 'colorDepth', {get: function(){return 24;}, configurable:true});"
    "      Object.defineProperty(screen, 'pixelDepth', {get: function(){return 24;}, configurable:true});"
    "    }"
    "  } catch (e) {}"

    // 17. Broken image dimensions
    "  try {"
    "    Object.defineProperty(HTMLImageElement.prototype, 'naturalWidth', {"
    "      get: function() {"
    "        var w = this.getAttribute('data-natural-width');"
    "        if (w) return parseInt(w);"
    "        return this.complete && this.src ? 16 : 0;"
    "      }, configurable: false"
    "    });"
    "    Object.defineProperty(HTMLImageElement.prototype, 'naturalHeight', {"
    "      get: function() {"
    "        var h = this.getAttribute('data-natural-height');"
    "        if (h) return parseInt(h);"
    "        return this.complete && this.src ? 16 : 0;"
    "      }, configurable: false"
    "    });"
    "  } catch (e) {}"

    // 18. Remove leftover automation globals
    "  delete window._phantom;"
    "  delete window.__nightmare;"
    "  delete window._selenium;"
    "  delete window.callPhantom;"
    "  delete window._Recaptcha;"
    "  delete window.emit;"
    "  delete window.spawn;"
    "  delete window.Buffer;"
    "})();";

// Escapes `src` so it can be spliced inside single quotes `'...'` in JavaScript.
// Caller owns returned buffer and must free().
static char* wick_escape_js_string(const char* src) {
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
static char* wick_build_timezone_js(const char* tz) {
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
