async function __plebRevealAndScroll(cfg) {
  cfg = cfg || {};
  var mode = cfg.mode || "none";
  var scrollSteps = Math.max(0, Number(cfg.scrollSteps || 0));
  var maxActions = Math.max(1, Number(cfg.maxActions || 16));
  var maxPerGroup = Math.max(1, Number(cfg.maxPerGroup || 8));
  var maxPanelBytes = Math.max(1024, Number(cfg.maxPanelBytes || 262144));
  var maxTotalBytes = Math.max(4096, Number(cfg.maxTotalBytes || 2097152));
  var budgetMs = Math.max(200, Number(cfg.budgetMs || 15000));

  var startedAt = Date.now();
  var deadline = startedAt + budgetMs;
  var loc = typeof location !== "undefined" ? location : { origin: "", pathname: "" };
  var startOrigin = loc.origin || "";
  var startPath = loc.pathname || "";

  var out = {
    revealed_panels: [],
    reveal_capped: false,
    scroll_steps_performed: 0,
    navigated: false,
    challenge_detected: false,
    warning: null
  };

  function didNavigate() {
    if (typeof location === "undefined") return false;
    return (location.origin || "") !== startOrigin || (location.pathname || "") !== startPath;
  }

  function isChallenge() {
    try {
      var body = document.body;
      var txt = body && typeof body.innerText === "string" ? body.innerText : "";
      var lower = txt.toLowerCase();
      if (lower.length < 4000 && (
        lower.indexOf("performing security verification") >= 0 ||
        lower.indexOf("checking your browser") >= 0 ||
        lower.indexOf("verify you are human") >= 0 ||
        lower.indexOf("just a moment") >= 0
      )) {
        return true;
      }
      if (document.querySelector && document.querySelector(
        '#cf-wrapper, iframe[src*="challenges.cloudflare.com"], iframe[src*="turnstile"], iframe[src*="hcaptcha"], iframe[src*="recaptcha"]'
      )) {
        return true;
      }
    } catch (_) {}
    return false;
  }

  if (isChallenge()) {
    out.challenge_detected = true;
    return out;
  }

  function sleep(ms) {
    return new Promise(function (resolve) { setTimeout(resolve, ms); });
  }

  function nextFrame() {
    if (typeof requestAnimationFrame === "function") {
      return new Promise(function (resolve) {
        var settled = false;
        var t = setTimeout(function () { if (!settled) { settled = true; resolve(); } }, 50);
        try {
          requestAnimationFrame(function () { if (!settled) { settled = true; clearTimeout(t); resolve(); } });
        } catch (_) {
          if (!settled) { settled = true; clearTimeout(t); resolve(); }
        }
      });
    }
    return sleep(16);
  }

  async function waitSettle(quietMs, capMs) {
    var stepStart = Date.now();
    var stepDeadline = Math.min(deadline, stepStart + capMs);
    var lastMut = Date.now();
    var obs = null;
    if (typeof MutationObserver === "function") {
      try {
        var root = document.documentElement || document.body;
        if (root) {
          obs = new MutationObserver(function () { lastMut = Date.now(); });
          obs.observe(root, { childList: true, subtree: true, characterData: true, attributes: true });
        }
      } catch (_) { obs = null; }
    }
    try {
      while (Date.now() < stepDeadline) {
        if (didNavigate() || isChallenge()) break;
        var inflight = (typeof window !== "undefined" && typeof window.__plebInflight === "number")
          ? window.__plebInflight : 0;
        if (inflight > 0) {
          lastMut = Date.now();
        } else if (Date.now() - lastMut >= quietMs) {
          break;
        }
        await sleep(30);
      }
    } finally {
      if (obs) { try { obs.disconnect(); } catch (_) {} }
    }
    await nextFrame();
  }

  function safeRectOk(el) {
    if (typeof el.getBoundingClientRect !== "function") return true;
    try {
      var r = el.getBoundingClientRect();
      return !r || (r.width > 0 && r.height > 0);
    } catch (_) { return true; }
  }

  function isElementHidden(el) {
    if (!el || el.nodeType !== 1) return true;
    if (el.hidden) return true;
    if (typeof el.getAttribute === "function" && el.getAttribute("aria-hidden") === "true") return true;
    if (typeof el.hasAttribute === "function" && (el.hasAttribute("hidden") || el.hasAttribute("inert"))) return true;
    if (typeof getComputedStyle === "function") {
      try {
        var cs = getComputedStyle(el);
        if (cs) {
          if (cs.display === "none" || cs.visibility === "hidden") return true;
          if (cs.opacity !== undefined && cs.opacity !== "" && parseFloat(cs.opacity) <= 0) return true;
        }
      } catch (_) {}
    }
    return false;
  }

  function isVisibleControl(el) {
    if (isElementHidden(el)) return false;
    if (typeof el.closest === "function" && el.closest('[hidden],[inert],[aria-hidden="true"]')) return false;
    var cur = el.parentElement;
    while (cur) {
      if (isElementHidden(cur)) return false;
      cur = cur.parentElement;
    }
    return safeRectOk(el);
  }

  var CHROME_SELECTOR = 'nav, header, footer, aside, dialog, [role="navigation"], [role="banner"], [role="contentinfo"], [role="complementary"], [role="dialog"], [role="alertdialog"], [role="menu"], [role="menubar"], [role="listbox"], [role="tooltip"]';

  function getLandmarkRoots() {
    if (!document.querySelectorAll) return [document.body || document.documentElement];
    var list = Array.prototype.slice.call(document.querySelectorAll('main, [role="main"], article, [role="article"]'));
    return list.length > 0 ? list : [document.body || document.documentElement];
  }

  function isInLandmarkScope(el, landmarks) {
    if (!el) return false;
    if (typeof el.closest === "function" && el.closest(CHROME_SELECTOR)) return false;
    for (var i = 0; i < landmarks.length; i++) {
      var lm = landmarks[i];
      if (!lm) continue;
      if (lm === el || (typeof lm.contains === "function" && lm.contains(el))) return true;
    }
    return false;
  }

  function extractVisibleText(node, maxLen) {
    var parts = [];
    function walk(n) {
      if (!n) return;
      if (n.nodeType === 3) {
        if (n.nodeValue) parts.push(n.nodeValue);
        return;
      }
      if (n.nodeType !== 1) return;
      var tag = (n.tagName || "").toLowerCase();
      if (tag === "script" || tag === "style" || tag === "noscript" || tag === "template" || tag === "svg") return;
      if (isElementHidden(n)) return;
      if (typeof getComputedStyle === "function") {
        try {
          var cs = getComputedStyle(n);
          if (cs && cs.fontSize !== undefined && parseFloat(cs.fontSize) === 0) return;
        } catch (_) {}
      }
      var kids = n.childNodes || [];
      for (var i = 0; i < kids.length; i++) walk(kids[i]);
    }
    walk(node);
    var joined = parts.join(" ").replace(/\s+/g, " ").trim();
    if (!joined && typeof node.getAttribute === "function") {
      joined = String(node.getAttribute("aria-label") || "").replace(/\s+/g, " ").trim();
    }
    if (joined.length > maxLen) joined = joined.slice(0, maxLen).trim();
    return joined;
  }

  var FORBIDDEN_LABEL_RE = /\b(sign\s*in|log\s*in|login|sign\s*up|register|buy|checkout|purchase|delete|remove|submit|accept\s*all|reject\s*all|cookie|verify|captcha|i\s*am\s*human)\b/i;

  function isSafeInteractiveControl(el) {
    if (!el || !isVisibleControl(el)) return false;
    if (el.disabled) return false;
    if (typeof el.getAttribute === "function") {
      if (el.getAttribute("aria-disabled") === "true") return false;
      if (el.getAttribute("aria-haspopup")) return false;
      var tag = (el.tagName || "").toLowerCase();
      if (tag === "input" || tag === "select" || tag === "textarea") return false;
      if (tag === "a") {
        var href = String(el.getAttribute("href") || "").trim();
        if (href && href[0] !== "#") return false;
      }
      var tp = String(el.getAttribute("type") || "").toLowerCase();
      if ((tp === "submit" || (tag === "button" && !tp)) && typeof el.closest === "function" && el.closest("form")) return false;
    }
    var lbl = extractVisibleText(el, 64);
    if (!lbl || FORBIDDEN_LABEL_RE.test(lbl)) return false;
    return true;
  }

  function utf8Len(str) {
    if (!str) return 0;
    if (typeof TextEncoder !== "undefined") {
      try { return new TextEncoder().encode(str).length; } catch (_) {}
    }
    return str.length;
  }

  function uniqueElementById(id) {
    if (!id || !document.querySelectorAll) return null;
    try {
      var escaped = id.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
      var matches = document.querySelectorAll('[id="' + escaped + '"]');
      return matches && matches.length === 1 ? matches[0] : null;
    } catch (_) {
      return document.getElementById ? document.getElementById(id) : null;
    }
  }

  function isStructHiddenInDom(el) {
    if (!el) return false;
    var text = String(el.textContent || "").trim();
    if (!text) return false;
    var hasHiddenAttr = typeof el.hasAttribute === "function" && el.hasAttribute("hidden");
    var dispNone = el.style && el.style.display === "none";
    var cls = String(el.className || "");
    var clsHidden = /(?:^|\s)(hidden|d-none|sr-only|visually-hidden)(?:\s|$)/.test(cls);
    var ariaHidden = typeof el.getAttribute === "function" && el.getAttribute("aria-hidden") === "true";
    var styleStr = typeof el.getAttribute === "function" ? String(el.getAttribute("style") || "").toLowerCase() : "";
    var opacityZero = styleStr.indexOf("opacity:0") >= 0 || styleStr.indexOf("opacity: 0") >= 0;
    return !opacityZero && (hasHiddenAttr || dispNone || clsHidden || ariaHidden);
  }

  function nearestSectionHeading(el) {
    if (!el || !document.querySelectorAll) return { heading: null, level: 2 };
    var heads = document.querySelectorAll("h1, h2, h3, h4, h5, h6");
    var lastText = null;
    var lastLevel = 2;
    for (var i = 0; i < heads.length; i++) {
      var h = heads[i];
      if (typeof h.compareDocumentPosition === "function") {
        var pos = h.compareDocumentPosition(el);
        if ((pos & 4) === 0 && (pos & 16) === 0) break;
      }
      var t = extractVisibleText(h, 120);
      if (t) {
        lastText = t;
        var parsed = parseInt((h.tagName || "H2").slice(1), 10);
        if (parsed >= 1 && parsed <= 6) lastLevel = parsed;
      }
    }
    return { heading: lastText, level: lastLevel };
  }

  // Phase A: Bounded Progressive Scroll
  if (scrollSteps > 0) {
    var scrollEl = document.scrollingElement || document.documentElement || document.body;
    var winH = (typeof window !== "undefined" && Number(window.innerHeight)) ||
      (document.documentElement && Number(document.documentElement.clientHeight)) || 800;
    if (scrollEl && Number(scrollEl.scrollHeight || 0) <= winH + 10) {
      var lmList = getLandmarkRoots();
      for (var li = 0; li < lmList.length; li++) {
        var rootLm = lmList[li];
        if (!rootLm || !rootLm.querySelectorAll) continue;
        var candidates = rootLm.querySelectorAll("*");
        for (var ci = 0; ci < candidates.length; ci++) {
          var cand = candidates[ci];
          if (typeof getComputedStyle === "function") {
            var ccs = getComputedStyle(cand);
            if (ccs && (ccs.overflowY === "auto" || ccs.overflowY === "scroll") &&
                Number(cand.scrollHeight || 0) > Number(cand.clientHeight || 0) + 50) {
              scrollEl = cand;
              break;
            }
          }
        }
      }
    }

    var stagnant = 0;
    for (var step = 0; step < scrollSteps; step++) {
      if (Date.now() >= deadline - 200 || didNavigate() || isChallenge()) {
        if (didNavigate()) out.navigated = true;
        if (isChallenge()) out.challenge_detected = true;
        break;
      }
      if (!scrollEl) break;
      var prevH = Number(scrollEl.scrollHeight || 0);
      var prevTop = Number(scrollEl.scrollTop || 0);
      var stepDelta = Math.max(Math.floor(winH * 0.85), 600);
      scrollEl.scrollTop = prevTop + stepDelta;
      if (typeof window !== "undefined" && typeof window.scrollTo === "function" &&
          (scrollEl === document.scrollingElement || scrollEl === document.documentElement || scrollEl === document.body)) {
        try { window.scrollTo(0, prevTop + stepDelta); } catch (_) {}
      }
      out.scroll_steps_performed += 1;
      await waitSettle(180, 1200);
      if (didNavigate()) {
        out.navigated = true;
        return out;
      }
      if (isChallenge()) {
        out.challenge_detected = true;
        return out;
      }
      var newH = Number(scrollEl.scrollHeight || 0);
      var newTop = Number(scrollEl.scrollTop || 0);
      var ch = Number(scrollEl.clientHeight || winH);
      var atBottom = newTop + ch >= newH - 4;
      if (newH <= prevH && (newTop <= prevTop || atBottom)) {
        stagnant += 1;
        if (stagnant >= 2) break;
      } else {
        stagnant = 0;
      }
    }
  }

  if (mode === "none" || out.navigated || out.challenge_detected) {
    return out;
  }

  // Phase B: Bounded In-Landmark Reveal
  var totalBytes = 0;
  var totalActions = 0;
  var landmarks = getLandmarkRoots();

  function recordPanel(kind, label, meta, html) {
    if (!html || !label) return true;
    var panelBytes = utf8Len(html);
    if (panelBytes === 0 || panelBytes > maxPanelBytes) return false;
    if (totalBytes + panelBytes > maxTotalBytes) {
      out.reveal_capped = true;
      return false;
    }
    totalBytes += panelBytes;
    out.revealed_panels.push({
      kind: kind,
      label: label,
      section_heading: meta.heading,
      heading_level: Math.min(6, Math.max(2, (meta.level || 2) + 1)),
      html: html
    });
    return true;
  }

  function resolveTabPanelEl(tab, scopeRoot) {
    if (tab && typeof tab.getAttribute === "function") {
      var ctrl = String(tab.getAttribute("aria-controls") || "").trim();
      if (ctrl && ctrl.indexOf(" ") < 0) {
        var target = uniqueElementById(ctrl);
        if (target && isInLandmarkScope(target, landmarks)) return target;
      }
    }
    var root = scopeRoot || document.body || document.documentElement;
    if (root && root.querySelectorAll) {
      var panels = root.querySelectorAll('[role="tabpanel"]');
      for (var i = 0; i < panels.length; i++) {
        if (isVisibleControl(panels[i])) return panels[i];
      }
    }
    return null;
  }

  function snapshotVisibleTabpanels(scopeRoot) {
    var map = [];
    var root = scopeRoot || document.body || document.documentElement;
    if (!root || !root.querySelectorAll) return map;
    var panels = root.querySelectorAll('[role="tabpanel"]');
    for (var i = 0; i < panels.length; i++) {
      if (isVisibleControl(panels[i])) {
        map.push(panels[i].outerHTML || "");
      }
    }
    return map;
  }

  function findChangedVisibleTabpanel(scopeRoot, prevHtmls) {
    var root = scopeRoot || document.body || document.documentElement;
    if (!root || !root.querySelectorAll) return null;
    var panels = root.querySelectorAll('[role="tabpanel"]');
    for (var i = 0; i < panels.length; i++) {
      if (!isVisibleControl(panels[i])) continue;
      var html = panels[i].outerHTML || "";
      if (prevHtmls.indexOf(html) < 0) return panels[i];
    }
    for (var j = 0; j < panels.length; j++) {
      if (isVisibleControl(panels[j])) return panels[j];
    }
    return null;
  }

  if (mode === "tabs" || mode === "all") {
    var tablists = document.querySelectorAll ? document.querySelectorAll('[role="tablist"]') : [];
    for (var ti = 0; ti < tablists.length; ti++) {
      if (Date.now() >= deadline - 150) { out.reveal_capped = true; break; }
      var tl = tablists[ti];
      if (!isInLandmarkScope(tl, landmarks) || !isVisibleControl(tl)) continue;
      var tabs = tl.querySelectorAll ? tl.querySelectorAll('[role="tab"]') : [];
      var activeTab = null;
      var toClick = [];
      for (var bi = 0; bi < tabs.length; bi++) {
        var tab = tabs[bi];
        if (!isInLandmarkScope(tab, landmarks) || !isSafeInteractiveControl(tab)) continue;
        var sel = typeof tab.getAttribute === "function" ? tab.getAttribute("aria-selected") : null;
        if (sel === "true" && !activeTab) {
          activeTab = tab;
          continue;
        }
        var ctrlId = typeof tab.getAttribute === "function" ? String(tab.getAttribute("aria-controls") || "").trim() : "";
        var existingPanel = ctrlId && ctrlId.indexOf(" ") < 0 ? uniqueElementById(ctrlId) : null;
        if (existingPanel && isStructHiddenInDom(existingPanel)) {
          // Already recoverable via zero-network `--expand tabs`
          continue;
        }
        toClick.push(tab);
      }
      if (toClick.length === 0) continue;

      var scopeRoot = (tl.parentElement && tl.parentElement.parentElement) || tl.parentElement || document.body;
      var tlMeta = nearestSectionHeading(tl);

      if (activeTab) {
        var activePanel = resolveTabPanelEl(activeTab, scopeRoot);
        var activeLabel = extractVisibleText(activeTab, 64);
        if (activePanel && activePanel.outerHTML && activeLabel) {
          recordPanel("tab", activeLabel, tlMeta, activePanel.outerHTML);
        }
      }

      var groupActions = 0;
      for (var ci = 0; ci < toClick.length; ci++) {
        if (groupActions >= maxPerGroup || totalActions >= maxActions || Date.now() >= deadline - 150) {
          out.reveal_capped = true;
          break;
        }
        var targetTab = toClick[ci];
        var label = extractVisibleText(targetTab, 64);
        if (!label) continue;
        var beforeHtmls = snapshotVisibleTabpanels(scopeRoot);
        totalActions += 1;
        groupActions += 1;
        if (typeof targetTab.click === "function") targetTab.click();
        await waitSettle(180, 1500);
        if (didNavigate()) {
          out.navigated = true;
          out.warning = "aborted --reveal because a tab control triggered page navigation";
          return out;
        }
        if (isChallenge()) {
          out.challenge_detected = true;
          return out;
        }
        var tid = typeof targetTab.getAttribute === "function" ? String(targetTab.getAttribute("aria-controls") || "").trim() : "";
        var revealedEl = (tid && tid.indexOf(" ") < 0) ? uniqueElementById(tid) : null;
        if (!revealedEl || isElementHidden(revealedEl)) {
          revealedEl = findChangedVisibleTabpanel(scopeRoot, beforeHtmls);
        }
        if (revealedEl && revealedEl.outerHTML && !isElementHidden(revealedEl)) {
          recordPanel("tab", label, tlMeta, revealedEl.outerHTML);
        }
      }

      if (activeTab && typeof activeTab.click === "function" && !didNavigate()) {
        activeTab.click();
        await waitSettle(120, 600);
        if (didNavigate()) {
          out.navigated = true;
          out.warning = "aborted --reveal because restoring active tab triggered page navigation";
          return out;
        }
      }
    }
  }

  if ((mode === "disclosures" || mode === "all") && !out.navigated && !out.challenge_detected) {
    var disclosures = document.querySelectorAll ? document.querySelectorAll('[aria-expanded="false"]') : [];
    for (var di = 0; di < disclosures.length; di++) {
      if (totalActions >= maxActions || Date.now() >= deadline - 150) {
        out.reveal_capped = true;
        break;
      }
      var btn = disclosures[di];
      if (!btn || (typeof btn.getAttribute === "function" && btn.getAttribute("role") === "tab")) continue;
      if (!isInLandmarkScope(btn, landmarks) || !isSafeInteractiveControl(btn)) continue;
      var dLabel = extractVisibleText(btn, 64);
      if (!dLabel) continue;
      var dCtrl = typeof btn.getAttribute === "function" ? String(btn.getAttribute("aria-controls") || "").trim() : "";
      if (dCtrl && dCtrl.indexOf(" ") < 0) {
        var staticTarget = uniqueElementById(dCtrl);
        if (staticTarget && isStructHiddenInDom(staticTarget)) {
          continue;
        }
      }

      var parentScope = btn.parentElement || document.body;
      var prevExpandedSibs = [];
      if (parentScope && parentScope.children) {
        for (var si = 0; si < parentScope.children.length; si++) {
          var sib = parentScope.children[si];
          if (sib !== btn && isVisibleControl(sib)) prevExpandedSibs.push(sib.outerHTML || "");
        }
      }

      totalActions += 1;
      if (typeof btn.click === "function") btn.click();
      await waitSettle(180, 1500);
      if (didNavigate()) {
        out.navigated = true;
        out.warning = "aborted --reveal because a disclosure control triggered page navigation";
        return out;
      }
      if (isChallenge()) {
        out.challenge_detected = true;
        return out;
      }

      var discEl = (dCtrl && dCtrl.indexOf(" ") < 0) ? uniqueElementById(dCtrl) : null;
      if (!discEl || isElementHidden(discEl)) {
        discEl = null;
        if (parentScope && parentScope.children) {
          for (var sj = 0; sj < parentScope.children.length; sj++) {
            var candSib = parentScope.children[sj];
            if (candSib !== btn && isVisibleControl(candSib)) {
              var sHtml = candSib.outerHTML || "";
              if (prevExpandedSibs.indexOf(sHtml) < 0) {
                discEl = candSib;
                break;
              }
            }
          }
        }
      }
      if (discEl && discEl.outerHTML && !isElementHidden(discEl)) {
        recordPanel("disclosure", dLabel, nearestSectionHeading(btn), discEl.outerHTML);
      }
    }
  }

  return out;
}
