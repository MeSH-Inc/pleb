#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "tiktoken>=0.8.0",
#   "trafilatura>=2.0.0",
#   "html2text>=2024.2.26",
#   "rank-bm25>=0.2.2",
#   "beautifulsoup4>=4.12.3",
# ]
# ///
"""Held-out task-level evaluation harness for pleb (external to the production binary).

Evaluates S(B) (evidence-supported task completions within pinned model-visible token budget B),
C_success (model-visible tokens per supported completion), false-usable / usable-rejected
confusion counts, concealed-injection leakage, and wall latency across pleb modes and
external static/BM25 extractors on identical frozen HTML inputs:
  - pleb_clean_full:     unbounded default clean Markdown
  - pleb_article_full:   unbounded article extraction
  - pleb_prefix_budget:  existing --max-bytes prefix truncation at budget B
  - pleb_focus_cli:      compiled `pleb fetch --focus <query> --max-bytes <N>` BM25 section projection
  - trafilatura_full:    Trafilatura Markdown extraction (favor_recall=True, include_tables=True)
  - html2text_full:      html2text Markdown conversion baseline
  - crawl4ai_bm25_ref:   BM25Okapi chunk-filter reference (Crawl4AI BM25ContentFilter pattern)

Multi-URL stdin batch track (runs offline by default unless `--no-batch`):
  Compares `pleb_batch_clean` vs. `pleb_batch_focus` over `pleb fetch - --jsonl`
  with mixed relevant, distractor, unavailable-evidence, and cloaked-injection URLs.

Optional CEF SPA hydration track (`--cef-track`):
  Verifies static `--render cronet` honest `incomplete_shell` triage vs. local
  `v5-meta` CEF `--render cef` full and `--focus` JS-hydrated recovery.

Optional paired live-URL track (`--live-track` / `--live-only`):
  Compares `pleb_live_clean` and `pleb_live_focus` head-to-head against direct
  `Accept: text/markdown` HTTP requests (Cloudflare Markdown-for-Agents baseline),
  Jina Reader (`r.jina.ai`, optional `JINA_API_KEY`), and Firecrawl (`/v1/scrape`
  when `FIRECRAWL_API_KEY` is set).

Run with pinned o200k_base tokenization via uv:
  uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb
"""

from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import math
import os
import re
import shutil
import socketserver
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any

ROOT_DIR = Path(__file__).resolve().parent
TASKS_PATH = ROOT_DIR / "tasks.json"


class TokenCounter:
    def __init__(self) -> None:
        try:
            import tiktoken  # type: ignore

            self._enc = tiktoken.get_encoding("o200k_base")
            self.name = "tiktoken:o200k_base"
        except Exception:
            self._enc = None
            self.name = "fallback:ceil(utf8_bytes/4)"

    def count(self, text: str) -> int:
        if self._enc is not None:
            return len(self._enc.encode(text, disallowed_special=()))
        return (len(text.encode("utf-8")) + 3) // 4


@dataclass
class SectionBlock:
    heading: str
    level: int
    body: str
    index: int


def split_markdown_sections(markdown: str) -> tuple[str, list[SectionBlock]]:
    lines = markdown.splitlines()
    preamble: list[str] = []
    sections: list[SectionBlock] = []
    cur_heading: str | None = None
    cur_level = 0
    cur_lines: list[str] = []
    in_fence = False
    fence_char = ""
    fence_len = 0

    for line in lines:
        stripped = line.lstrip()
        if not in_fence and len(stripped) >= 3 and stripped[0] in ("`", "~"):
            run = len(stripped) - len(stripped.lstrip(stripped[0]))
            if run >= 3:
                in_fence = True
                fence_char = stripped[0]
                fence_len = run
        elif in_fence:
            t = line.strip()
            if len(t) >= fence_len and set(t) == {fence_char}:
                in_fence = False

        heading_match = None if in_fence else re.match(r"^(#{1,6})\s+(.+)$", stripped)
        if heading_match:
            if cur_heading is not None:
                sections.append(
                    SectionBlock(
                        heading=cur_heading,
                        level=cur_level,
                        body="\n".join(cur_lines).strip(),
                        index=len(sections),
                    )
                )
            cur_heading = line
            cur_level = len(heading_match.group(1))
            cur_lines = []
        else:
            if cur_heading is None:
                preamble.append(line)
            else:
                cur_lines.append(line)

    if cur_heading is not None:
        sections.append(
            SectionBlock(
                heading=cur_heading,
                level=cur_level,
                body="\n".join(cur_lines).strip(),
                index=len(sections),
            )
        )
    return "\n".join(preamble).strip(), sections


def tokenize_terms(text: str) -> list[str]:
    return re.findall(r"[a-z0-9][a-z0-9_.:/\-+=]*", text.lower())


def strip_markdown_inline_links(text: str) -> str:
    # Unwrap [label](url "optional title") -> label while preserving image/code text.
    return re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)


def normalize_for_evidence(text: str) -> str:
    unlinked = strip_markdown_inline_links(unescape_markdown_punct(text))
    no_inline_ticks = re.sub(r"`([^`\n]+)`", r"\1", unlinked)
    return normalize_ws(no_inline_ticks)


def split_section_subblocks(body: str) -> list[str]:
    """Split a large section body into intact paragraphs / fenced code blocks / tables."""
    blocks: list[str] = []
    cur: list[str] = []
    in_fence = False
    fence_char = ""
    fence_len = 0
    for line in body.splitlines():
        stripped = line.lstrip()
        if not in_fence and len(stripped) >= 3 and stripped[0] in ("`", "~"):
            run = len(stripped) - len(stripped.lstrip(stripped[0]))
            if run >= 3:
                in_fence = True
                fence_char = stripped[0]
                fence_len = run
        elif in_fence:
            t = line.strip()
            if len(t) >= fence_len and set(t) == {fence_char}:
                in_fence = False
        if not in_fence and not line.strip():
            if cur:
                blocks.append("\n".join(cur).strip())
                cur = []
        else:
            cur.append(line)
    if cur:
        blocks.append("\n".join(cur).strip())
    return [b for b in blocks if b]


def project_focus_markdown(
    markdown: str, query: str, budget_tokens: int, counter: TokenCounter
) -> str:
    """Prototype heading-aware BM25 projection preserving intact sections, code, tables, and bounded TOC."""
    if counter.count(markdown) <= budget_tokens:
        return markdown

    preamble, sections = split_markdown_sections(markdown)
    if not sections:
        return markdown

    q_terms = tokenize_terms(query)
    if not q_terms:
        return markdown

    doc_tokens = [
        tokenize_terms(f"{strip_markdown_inline_links(s.heading)}\n{s.body}")
        for s in sections
    ]
    n_docs = len(sections)
    avgdl = sum(len(t) for t in doc_tokens) / max(1, n_docs)
    df: Counter[str] = Counter()
    for toks in doc_tokens:
        for term in set(toks):
            df[term] += 1

    k1 = 1.5
    b = 0.75
    scores: list[float] = []
    for idx, toks in enumerate(doc_tokens):
        tf = Counter(toks)
        dl = len(toks)
        score = 0.0
        heading_terms = set(tokenize_terms(strip_markdown_inline_links(sections[idx].heading)))
        for term in q_terms:
            f = tf.get(term, 0)
            if f == 0:
                continue
            idf = math.log((n_docs - df[term] + 0.5) / (df[term] + 0.5) + 1.0)
            denom = f + k1 * (1.0 - b + b * dl / max(1.0, avgdl))
            term_score = idf * (f * (k1 + 1.0)) / max(1e-9, denom)
            if term in heading_terms:
                term_score *= 1.6
            score += term_score
        if idx == 0:
            score += 0.35
        scores.append(score)

    toc_budget = max(40, min(140, budget_tokens // 10))

    def format_bounded_toc(selected_indices: set[int]) -> str:
        collapsed: list[str] = []
        for s in sections:
            if s.index in selected_indices:
                continue
            label = re.sub(r"^#+\s*", "", strip_markdown_inline_links(s.heading)).strip()
            label = normalize_ws(unescape_markdown_punct(label))
            if label:
                collapsed.append(label)
        if not collapsed:
            return ""
        kept: list[str] = []
        for idx_c, item in enumerate(collapsed):
            rem = len(collapsed) - idx_c - 1
            suffix = f" | ... (+{rem} more)" if rem > 0 else ""
            candidate = "<!-- Collapsed sections: " + " | ".join(kept + [item]) + suffix + " -->"
            if counter.count(candidate) > toc_budget:
                rem_all = len(collapsed) - len(kept)
                if kept:
                    return (
                        "<!-- Collapsed sections: "
                        + " | ".join(kept)
                        + f" | ... (+{rem_all} more) -->"
                    )
                return f"<!-- Collapsed sections: {rem_all} omitted -->"
            kept.append(item)
        return "<!-- Collapsed sections: " + " | ".join(kept) + " -->"

    def render_selected(selected_indices: set[int], overrides: dict[int, str] | None = None) -> str:
        parts: list[str] = []
        if preamble and counter.count(preamble) <= budget_tokens // 5:
            parts.append(preamble)
        for s in sections:
            if s.index in selected_indices:
                body = overrides[s.index] if overrides and s.index in overrides else s.body
                block = f"{s.heading}\n\n{body}" if body else s.heading
                parts.append(block)
        toc = format_bounded_toc(selected_indices)
        if toc:
            parts.append(toc)
        return "\n\n".join(p for p in parts if p)

    def compress_oversized_section(sec: SectionBlock, max_sec_tokens: int) -> str | None:
        subblocks = split_section_subblocks(sec.body)
        if len(subblocks) <= 1:
            return None
        sub_toks = [tokenize_terms(sb) for sb in subblocks]
        s_avgdl = sum(len(t) for t in sub_toks) / max(1, len(sub_toks))
        s_df: Counter[str] = Counter()
        for toks in sub_toks:
            for term in set(toks):
                s_df[term] += 1
        s_scores: list[float] = []
        for idx_b, toks in enumerate(sub_toks):
            tf = Counter(toks)
            dl = len(toks)
            sc = 0.0
            for term in q_terms:
                f = tf.get(term, 0)
                if f == 0:
                    continue
                idf = math.log((len(sub_toks) - s_df[term] + 0.5) / (s_df[term] + 0.5) + 1.0)
                denom = f + k1 * (1.0 - b + b * dl / max(1.0, s_avgdl))
                sc += idf * (f * (k1 + 1.0)) / max(1e-9, denom)
            # Keep lead paragraph / definition block slightly favored for context
            if idx_b == 0:
                sc += 0.25
            s_scores.append(sc)
        ranked_b = sorted(range(len(subblocks)), key=lambda i: (-s_scores[i], i))
        chosen: set[int] = set()
        for i in ranked_b:
            # Prefer keeping top-scoring block together with its immediate next block (e.g., code/table)
            trial = set(chosen)
            trial.add(i)
            cand = "\n\n".join(subblocks[j] for j in sorted(trial))
            if counter.count(f"{sec.heading}\n\n{cand}") <= max_sec_tokens:
                chosen = trial
        if not chosen:
            return None
        ordered = sorted(chosen)
        out_parts: list[str] = []
        prev = -1
        for j in ordered:
            if prev >= 0 and j > prev + 1:
                out_parts.append("[...]")
            out_parts.append(subblocks[j])
            prev = j
        return "\n\n".join(out_parts)

    ranked = sorted(range(n_docs), key=lambda i: (-scores[i], i))
    selected: set[int] = set()
    overrides: dict[int, str] = {}

    if sections[0].level == 1:
        cand = render_selected({0})
        if counter.count(cand) <= budget_tokens // 3:
            selected.add(0)

    for idx in ranked:
        if idx in selected:
            continue
        trial = set(selected)
        trial.add(idx)
        if sections[idx].level > 1:
            for prev in range(idx - 1, -1, -1):
                if sections[prev].level < sections[idx].level:
                    if not sections[prev].body or len(sections[prev].body) < 300:
                        trial.add(prev)
                    break
        rendered = render_selected(trial, overrides)
        if counter.count(rendered) <= budget_tokens:
            selected = trial
        elif scores[idx] > 0:
            # Section is high-scoring but too large intact: compress its sub-blocks to fit remaining budget.
            base_tokens = counter.count(render_selected(selected, overrides))
            rem_tokens = budget_tokens - base_tokens - 24
            if rem_tokens >= 180:
                compressed = compress_oversized_section(sections[idx], rem_tokens)
                if compressed is not None:
                    trial_ov = dict(overrides)
                    trial_ov[idx] = compressed
                    trial_sel = set(selected)
                    trial_sel.add(idx)
                    if counter.count(render_selected(trial_sel, trial_ov)) <= budget_tokens:
                        selected = trial_sel
                        overrides = trial_ov

    if not selected:
        top = ranked[0]
        compressed = compress_oversized_section(sections[top], budget_tokens - toc_budget - 16)
        if compressed is not None:
            return render_selected({top}, {top: compressed})
        raw = render_selected({top})
        lines = raw.splitlines()
        kept_lines: list[str] = []
        for line in lines:
            candidate = "\n".join(kept_lines + [line])
            if counter.count(candidate) > budget_tokens:
                break
            kept_lines.append(line)
        return "\n".join(kept_lines)

    return render_selected(selected, overrides)


class FixtureHTTPServer:
    def __init__(self, tasks: list[dict[str, Any]]) -> None:
        self.routes: dict[str, dict[str, Any]] = {}
        for task in tasks:
            fixture_bytes = (ROOT_DIR / task["fixture"]).read_bytes()
            if task.get("redirect_to"):
                self.routes[task["path"]] = {
                    "status": 302,
                    "location": task["redirect_to"],
                    "content_type": "text/plain",
                    "body": b"",
                }
                self.routes[task["redirect_to"]] = {
                    "status": task["status_code"],
                    "content_type": task["content_type"],
                    "body": fixture_bytes,
                }
            else:
                self.routes[task["path"]] = {
                    "status": task["status_code"],
                    "content_type": task["content_type"],
                    "body": fixture_bytes,
                }

        routes = self.routes

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                entry = routes.get(self.path)
                if entry is None:
                    self.send_response(404)
                    self.send_header("Content-Type", "text/plain")
                    self.end_headers()
                    self.wfile.write(b"not found")
                    return
                self.send_response(entry["status"])
                if "location" in entry:
                    self.send_header("Location", entry["location"])
                self.send_header("Content-Type", entry["content_type"])
                self.send_header("Content-Length", str(len(entry["body"])))
                self.end_headers()
                self.wfile.write(entry["body"])

            def log_message(self, format: str, *args: Any) -> None:
                return

        self._httpd = socketserver.TCPServer(("127.0.0.1", 0), Handler)
        self.port = self._httpd.server_address[1]
        self.base_url = f"http://127.0.0.1:{self.port}"
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)

    def __enter__(self) -> "FixtureHTTPServer":
        self._thread.start()
        return self

    def __exit__(self, *args: Any) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()


def normalize_ws(text: str) -> str:
    return " ".join(text.split())


def unescape_markdown_punct(text: str) -> str:
    return re.sub(r"\\([\\`*_{}\[\]()#+\-.!~|<>])", r"\1", text)


def run_pleb(
    pleb_bin: str,
    state_dir: str,
    url: str,
    extra_args: list[str],
    allow_home_cef: bool = False,
    render_mode: str = "cronet",
) -> tuple[int, dict[str, Any] | None, dict[str, Any] | None, float]:
    env = os.environ.copy()
    if not allow_home_cef:
        env["PLEB_NO_HOME_CEF"] = "1"
    else:
        env.pop("PLEB_NO_HOME_CEF", None)
    env["PLEB_NO_KEYCHAIN"] = "1"
    env.pop("PLEB_CDP_URL", None)
    cmd = [
        pleb_bin,
        "--state-dir",
        state_dir,
        "--no-daemon",
        "--json",
        "fetch",
        "--render",
        render_mode,
        *extra_args,
        url,
    ]
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, capture_output=True, text=True, env=env, check=False)
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    out_json = None
    err_json = None
    if proc.stdout.strip():
        try:
            out_json = json.loads(proc.stdout)
        except json.JSONDecodeError:
            out_json = None
    if proc.stderr.strip():
        try:
            err_json = json.loads(proc.stderr)
        except json.JSONDecodeError:
            err_json = None
    return proc.returncode, out_json, err_json, elapsed_ms


def run_pleb_batch(
    pleb_bin: str,
    state_dir: str,
    urls: list[str],
    extra_args: list[str],
) -> tuple[int, list[dict[str, Any]], float]:
    env = os.environ.copy()
    env["PLEB_NO_HOME_CEF"] = "1"
    env["PLEB_NO_KEYCHAIN"] = "1"
    env.pop("PLEB_CDP_URL", None)
    cmd = [
        pleb_bin,
        "--state-dir",
        state_dir,
        "--no-daemon",
        "fetch",
        "-",
        "--jsonl",
        "--render",
        "cronet",
        *extra_args,
    ]
    t0 = time.perf_counter()
    proc = subprocess.run(
        cmd,
        input="\n".join(urls) + "\n",
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    elapsed_ms = (time.perf_counter() - t0) * 1000.0
    items: list[dict[str, Any]] = []
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            items.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    return proc.returncode, items, elapsed_ms


def build_model_visible_envelope(payload: dict[str, Any], content: str) -> str:
    lines = [
        f"URL: {payload.get('final_url', payload.get('url', ''))}",
        f"Outcome: {payload.get('content_outcome', 'unknown')}",
    ]
    if payload.get("title"):
        lines.append(f"Title: {payload['title']}")
    if payload.get("warning"):
        lines.append(f"Warning: {payload['warning']}")
    if payload.get("resources"):
        res_summary = ", ".join(
            f"{r['identifier']}:{r['kind']}({r['url']})" for r in payload["resources"]
        )
        lines.append(f"Resources: {res_summary}")
    return "\n".join(lines) + "\n\n" + content


def evaluate_external_extractor(
    strategy: str,
    tasks: list[dict[str, Any]],
    budget_tokens: int,
    counter: TokenCounter,
) -> dict[str, Any]:
    """Evaluate frozen HTML directly with external Python extractors for head-to-head comparison."""
    import bs4  # type: ignore
    import html2text  # type: ignore
    import trafilatura  # type: ignore
    from rank_bm25 import BM25Okapi  # type: ignore

    rows: list[dict[str, Any]] = []
    for task in tasks:
        html = (ROOT_DIR / task["fixture"]).read_text(encoding="utf-8", errors="replace")
        t0 = time.perf_counter()
        if strategy == "trafilatura_full":
            extracted = (
                trafilatura.extract(
                    html,
                    output_format="markdown",
                    include_tables=True,
                    include_links=True,
                    favor_recall=True,
                )
                or ""
            )
        elif strategy == "html2text_full":
            conv = html2text.HTML2Text()
            conv.body_width = 0
            extracted = conv.handle(html)
        elif strategy == "crawl4ai_bm25_ref":
            # Faithful Crawl4AI BM25ContentFilter pattern: split HTML into text chunks,
            # rank with BM25Okapi against query, and pack highest-scoring chunks within budget B.
            soup = bs4.BeautifulSoup(html, "html.parser")
            for tag in soup(["script", "style", "noscript", "template"]):
                tag.decompose()
            raw_chunks: list[str] = []
            for el in soup.find_all(["h1", "h2", "h3", "h4", "p", "pre", "table", "li", "blockquote"]):
                if el.find_parent(["pre", "table"]) is not None:
                    continue
                text = " ".join(el.get_text(" ", strip=True).split())
                if text:
                    raw_chunks.append(text)
            if raw_chunks:
                corpus = [tokenize_terms(c) or ["_"] for c in raw_chunks]
                bm25 = BM25Okapi(corpus)
                q_toks = tokenize_terms(task["query"])
                scores = bm25.get_scores(q_toks) if q_toks else [0.0] * len(raw_chunks)
                ranked = sorted(range(len(raw_chunks)), key=lambda i: (-scores[i], i))
                kept_idx: list[int] = []
                used_toks = 0
                for i in ranked:
                    if scores[i] <= 0 and kept_idx:
                        break
                    c_toks = counter.count(raw_chunks[i])
                    if used_toks + c_toks > max(128, budget_tokens - 32):
                        continue
                    kept_idx.append(i)
                    used_toks += c_toks
                kept_idx.sort()
                extracted = "\n\n".join(raw_chunks[i] for i in kept_idx)
            else:
                extracted = ""
        else:
            raise ValueError(f"unknown external strategy: {strategy}")
        elapsed_ms = (time.perf_counter() - t0) * 1000.0

        visible = f"URL: https://frozen.local{task['path']}\n\n{extracted}"
        tokens = counter.count(visible)
        if task["answerable"]:
            norm_visible = normalize_ws(visible)
            hits = count_evidence_hits(visible, task["required_evidence"])
            res_ok = True
            if task.get("expected_resource_url_suffix"):
                res_ok = task["expected_resource_url_suffix"] in visible
            norm_lower = norm_visible.lower()
            norm_unescaped_lower = normalize_ws(unescape_markdown_punct(visible)).lower()
            leaked = any(
                normalize_ws(forb).lower() in norm_lower
                or normalize_ws(forb).lower() in norm_unescaped_lower
                for forb in task.get("forbidden_substrings", [])
            )
            usable_rejected = not extracted.strip()
            supported = (
                not usable_rejected
                and tokens <= budget_tokens
                and hits == len(task["required_evidence"])
                and res_ok
                and not leaked
            )
            rows.append(
                {
                    "task_id": task["id"],
                    "family": task["family"],
                    "answerable": True,
                    "supported": supported,
                    "usable_rejected": usable_rejected,
                    "false_usable": False,
                    "injection_leaked": leaked,
                    "evidence_hits": hits,
                    "evidence_total": len(task["required_evidence"]),
                    "resource_matched": res_ok,
                    "tokens": tokens,
                    "latency_ms": elapsed_ms,
                    "within_budget": tokens <= budget_tokens,
                }
            )
        else:
            # Static extractors lack HTTP status/challenge/login-wall triage:
            # non-empty output on an unavailable page is a silent false-usable result.
            non_empty = bool(extracted.strip())
            rows.append(
                {
                    "task_id": task["id"],
                    "family": task["family"],
                    "answerable": False,
                    "supported": not non_empty,
                    "usable_rejected": False,
                    "false_usable": non_empty,
                    "injection_leaked": False,
                    "evidence_hits": 0 if non_empty else 1,
                    "evidence_total": 1,
                    "tokens": tokens,
                    "latency_ms": elapsed_ms,
                    "within_budget": tokens <= budget_tokens,
                }
            )

    return summarize_rows(strategy, budget_tokens, rows)


def summarize_rows(
    strategy: str, budget_tokens: int, rows: list[dict[str, Any]]
) -> dict[str, Any]:
    total_tasks = len(rows)
    supported_count = sum(1 for r in rows if r["supported"])
    ans_rows = [r for r in rows if r["answerable"]]
    wild_rows = [r for r in ans_rows if r["family"] == "wild_production_docs"]
    unavail_rows = [r for r in rows if not r["answerable"]]
    total_tokens = sum(r["tokens"] for r in rows)
    latencies = sorted(r["latency_ms"] for r in rows)
    p95_idx = min(len(latencies) - 1, math.ceil(0.95 * len(latencies)) - 1)
    return {
        "strategy": strategy,
        "budget_tokens": budget_tokens,
        "tasks_attempted": total_tasks,
        "supported_completions": supported_count,
        "S_B": supported_count / max(1, total_tasks),
        "answerable_supported": sum(1 for r in ans_rows if r["supported"]),
        "answerable_total": len(ans_rows),
        "wild_supported": sum(1 for r in wild_rows if r["supported"]),
        "wild_total": len(wild_rows),
        "unavailable_honest": sum(1 for r in unavail_rows if r["supported"]),
        "unavailable_total": len(unavail_rows),
        "false_usable_count": sum(1 for r in rows if r["false_usable"]),
        "usable_rejected_count": sum(1 for r in rows if r["usable_rejected"]),
        "injection_leaked_count": sum(1 for r in rows if r["injection_leaked"]),
        "total_model_visible_tokens": total_tokens,
        "tokens_per_supported_completion": (
            total_tokens / supported_count if supported_count > 0 else None
        ),
        "mean_tokens": total_tokens / max(1, total_tasks),
        "p95_latency_ms": latencies[p95_idx] if latencies else 0.0,
        "tasks": rows,
    }


def evaluate_strategy(
    strategy: str,
    tasks: list[dict[str, Any]],
    base_url: str,
    pleb_bin: str,
    budget_tokens: int,
    counter: TokenCounter,
) -> dict[str, Any]:
    state_dir = tempfile.mkdtemp(prefix=f"pleb-ev-{os.getpid()}-", dir="/tmp")
    try:
        rows: list[dict[str, Any]] = []
        for task in tasks:
            url = f"{base_url}{task['path']}"
            extra: list[str] = []
            if strategy == "pleb_article_full":
                extra = ["--extract", "article"]
            elif strategy == "pleb_prefix_budget":
                # Conservative byte approximation for CLI --max-bytes baseline
                extra = ["--max-bytes", str(budget_tokens * 4)]
            elif strategy == "pleb_focus_cli":
                # Real compiled `pleb fetch --focus <query> --max-bytes <N>` CLI path
                # Reserve ~96 tokens (~384 bytes) for model-visible JSON/header envelope overhead.
                extra = [
                    "--focus",
                    task["query"],
                    "--max-bytes",
                    str(max(512, (budget_tokens - 140) * 3)),
                ]

            exit_code, out_json, err_json, elapsed_ms = run_pleb(
                pleb_bin, state_dir, url, extra
            )

            if task["answerable"]:
                if exit_code != 0 or out_json is None:
                    rows.append(
                        {
                            "task_id": task["id"],
                            "family": task["family"],
                            "answerable": True,
                            "supported": False,
                            "usable_rejected": True,
                            "false_usable": False,
                            "injection_leaked": False,
                            "evidence_hits": 0,
                            "evidence_total": len(task["required_evidence"]),
                            "tokens": counter.count(json.dumps(err_json or {})),
                            "latency_ms": elapsed_ms,
                            "within_budget": True,
                        }
                    )
                    continue

                content = out_json.get("content", "")
                if strategy == "pleb_focus_proto":
                    envelope_overhead = counter.count(
                        build_model_visible_envelope(out_json, "")
                    )
                    content_budget = max(256, budget_tokens - envelope_overhead)
                    content = project_focus_markdown(
                        content, task["query"], content_budget, counter
                    )

                visible = build_model_visible_envelope(out_json, content)
                tokens = counter.count(visible)
                within_budget = tokens <= budget_tokens
                outcome = out_json.get("content_outcome")
                usable_rejected = outcome != "complete"

                hits = count_evidence_hits(visible, task["required_evidence"])
                res_ok = True
                if task.get("expected_resource_url_suffix"):
                    suffix = task["expected_resource_url_suffix"]
                    res_urls = [r.get("url", "") for r in out_json.get("resources", [])]
                    res_ok = any(u.endswith(suffix) for u in res_urls)

                norm_lower = normalize_ws(visible).lower()
                norm_unescaped_lower = normalize_ws(unescape_markdown_punct(visible)).lower()
                leaked = any(
                    normalize_ws(forb).lower() in norm_lower
                    or normalize_ws(forb).lower() in norm_unescaped_lower
                    for forb in task.get("forbidden_substrings", [])
                )

                supported = (
                    not usable_rejected
                    and within_budget
                    and hits == len(task["required_evidence"])
                    and res_ok
                    and not leaked
                )
                rows.append(
                    {
                        "task_id": task["id"],
                        "family": task["family"],
                        "answerable": True,
                        "supported": supported,
                        "usable_rejected": usable_rejected,
                        "false_usable": False,
                        "injection_leaked": leaked,
                        "evidence_hits": hits,
                        "evidence_total": len(task["required_evidence"]),
                        "resource_matched": res_ok,
                        "tokens": tokens,
                        "latency_ms": elapsed_ms,
                        "within_budget": within_budget,
                    }
                )
            else:
                expected = task["expected_unavailable_signal"]
                honest = False
                if expected == "incomplete_shell":
                    honest = (
                        exit_code == 0
                        and out_json is not None
                        and out_json.get("content_outcome") == "incomplete_shell"
                    )
                elif expected == "exit_2":
                    honest = exit_code == 2
                elif expected == "exit_3":
                    honest = exit_code == 3

                false_usable = (
                    exit_code == 0
                    and out_json is not None
                    and out_json.get("content_outcome") == "complete"
                )
                visible_text = (
                    build_model_visible_envelope(out_json, out_json.get("content", ""))
                    if out_json is not None
                    else json.dumps(err_json or {})
                )
                tokens = counter.count(visible_text)
                rows.append(
                    {
                        "task_id": task["id"],
                        "family": task["family"],
                        "answerable": False,
                        "supported": honest and tokens <= budget_tokens,
                        "usable_rejected": False,
                        "false_usable": false_usable,
                        "injection_leaked": False,
                        "evidence_hits": 1 if honest else 0,
                        "evidence_total": 1,
                        "tokens": tokens,
                        "latency_ms": elapsed_ms,
                        "within_budget": tokens <= budget_tokens,
                    }
                )

        return summarize_rows(strategy, budget_tokens, rows)
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)


def count_evidence_hits(visible: str, needles: list[str]) -> int:
    norm_visible = normalize_ws(visible)
    norm_evidence = normalize_for_evidence(visible)
    return sum(
        1
        for needle in needles
        if normalize_ws(needle) in norm_visible or normalize_ws(needle) in norm_evidence
    )


def fetch_http_text(
    url: str,
    headers: dict[str, str],
    body: bytes | None = None,
    timeout_s: float = 20.0,
) -> tuple[int, str, str, float, str | None]:
    req = urllib.request.Request(url, data=body, headers=headers, method="POST" if body else "GET")
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout_s) as resp:
            raw = resp.read()
            ctype = resp.headers.get("Content-Type", "")
            elapsed_ms = (time.perf_counter() - t0) * 1000.0
            return resp.status, ctype, raw.decode("utf-8", errors="replace"), elapsed_ms, None
    except urllib.error.HTTPError as exc:
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        return exc.code, exc.headers.get("Content-Type", "") if exc.headers else "", "", elapsed_ms, f"HTTP {exc.code}"
    except Exception as exc:
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        return 0, "", "", elapsed_ms, str(exc)


def evaluate_live_track(
    live_tasks: list[dict[str, Any]],
    pleb_bin: str,
    budget_tokens: int,
    competitors: list[str],
    counter: TokenCounter,
) -> dict[str, Any]:
    """Opt-in paired live-URL comparison: pleb vs Accept:text/markdown, r.jina.ai, and Firecrawl."""
    strategies = ["pleb_live_clean", "pleb_live_focus"]
    if "accept_markdown" in competitors:
        strategies.append("accept_markdown_direct")
    if "jina" in competitors:
        strategies.append("jina_reader")
    if "firecrawl" in competitors:
        strategies.append("firecrawl_api")

    state_dir = tempfile.mkdtemp(prefix=f"pleb-live-{os.getpid()}-", dir="/tmp")
    per_strategy: dict[str, list[dict[str, Any]]] = {s: [] for s in strategies}
    firecrawl_key = os.environ.get("FIRECRAWL_API_KEY", "").strip()
    jina_key = os.environ.get("JINA_API_KEY", "").strip()

    try:
        for task in live_tasks:
            url = task["url"]
            needles = task["required_evidence"]

            for strat in ("pleb_live_clean", "pleb_live_focus"):
                extra = (
                    [
                        "--focus",
                        task["query"],
                        "--max-bytes",
                        str(max(512, (budget_tokens - 140) * 3)),
                    ]
                    if strat == "pleb_live_focus"
                    else []
                )
                exit_code, out_json, err_json, elapsed_ms = run_pleb(pleb_bin, state_dir, url, extra)
                if exit_code == 0 and out_json is not None:
                    visible = build_model_visible_envelope(out_json, out_json.get("content", ""))
                    tokens = counter.count(visible)
                    hits = count_evidence_hits(visible, needles)
                    within = tokens <= budget_tokens
                    complete = out_json.get("content_outcome") == "complete"
                    per_strategy[strat].append(
                        {
                            "task_id": task["id"],
                            "url": url,
                            "status": "ok" if complete else f"outcome:{out_json.get('content_outcome')}",
                            "representation": out_json.get("strategy", ""),
                            "sha256": (out_json.get("acquired_sha256") or "")[:12],
                            "evidence_hits": hits,
                            "evidence_total": len(needles),
                            "within_budget": within,
                            "supported": complete and within and hits == len(needles),
                            "tokens": tokens,
                            "latency_ms": elapsed_ms,
                        }
                    )
                else:
                    msg = (err_json or {}).get("error", f"exit_{exit_code}")
                    per_strategy[strat].append(
                        {
                            "task_id": task["id"],
                            "url": url,
                            "status": f"error:{msg[:40]}",
                            "representation": "",
                            "sha256": "",
                            "evidence_hits": 0,
                            "evidence_total": len(needles),
                            "within_budget": True,
                            "supported": False,
                            "tokens": counter.count(json.dumps(err_json or {})),
                            "latency_ms": elapsed_ms,
                        }
                    )

            if "accept_markdown_direct" in per_strategy:
                status, ctype, text, elapsed_ms, err = fetch_http_text(
                    url,
                    {
                        "Accept": "text/markdown, text/html;q=0.9",
                        "User-Agent": "pleb-bench/1.0",
                    },
                )
                visible = f"URL: {url}\n\n{text}" if not err else ""
                tokens = counter.count(visible) if visible else 0
                hits = count_evidence_hits(visible, needles) if visible else 0
                within = tokens <= budget_tokens
                per_strategy["accept_markdown_direct"].append(
                    {
                        "task_id": task["id"],
                        "url": url,
                        "status": "ok" if not err else f"error:{err}",
                        "representation": ctype.split(";")[0].strip(),
                        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()[:12] if text else "",
                        "evidence_hits": hits,
                        "evidence_total": len(needles),
                        "within_budget": within,
                        "supported": (not err) and within and hits == len(needles),
                        "tokens": tokens,
                        "latency_ms": elapsed_ms,
                    }
                )

            if "jina_reader" in per_strategy:
                jina_headers = {
                    "Accept": "text/markdown",
                    "User-Agent": "pleb-bench/1.0",
                }
                if jina_key:
                    jina_headers["Authorization"] = f"Bearer {jina_key}"
                status, ctype, text, elapsed_ms, err = fetch_http_text(
                    f"https://r.jina.ai/{url}",
                    jina_headers,
                    timeout_s=25.0,
                )
                visible = f"URL: {url}\n\n{text}" if not err else ""
                tokens = counter.count(visible) if visible else 0
                hits = count_evidence_hits(visible, needles) if visible else 0
                within = tokens <= budget_tokens
                row_status = (
                    "ok"
                    if not err
                    else (
                        "auth_required(set JINA_API_KEY)"
                        if status == 401 and not jina_key
                        else ("rate_limited" if status == 429 else f"error:{err}")
                    )
                )
                per_strategy["jina_reader"].append(
                    {
                        "task_id": task["id"],
                        "url": url,
                        "status": row_status,
                        "representation": ctype.split(";")[0].strip(),
                        "sha256": hashlib.sha256(text.encode("utf-8")).hexdigest()[:12] if text else "",
                        "evidence_hits": hits,
                        "evidence_total": len(needles),
                        "within_budget": within,
                        "supported": (not err) and within and hits == len(needles),
                        "tokens": tokens,
                        "latency_ms": elapsed_ms,
                    }
                )

            if "firecrawl_api" in per_strategy:
                if not firecrawl_key:
                    per_strategy["firecrawl_api"].append(
                        {
                            "task_id": task["id"],
                            "url": url,
                            "status": "skipped_no_api_key",
                            "representation": "",
                            "sha256": "",
                            "evidence_hits": 0,
                            "evidence_total": len(needles),
                            "within_budget": True,
                            "supported": False,
                            "tokens": 0,
                            "latency_ms": 0.0,
                        }
                    )
                else:
                    payload = json.dumps(
                        {"url": url, "formats": ["markdown"], "onlyMainContent": True}
                    ).encode("utf-8")
                    status, _, raw_resp, elapsed_ms, err = fetch_http_text(
                        "https://api.firecrawl.dev/v1/scrape",
                        {
                            "Authorization": f"Bearer {firecrawl_key}",
                            "Content-Type": "application/json",
                            "User-Agent": "pleb-bench/1.0",
                        },
                        body=payload,
                        timeout_s=30.0,
                    )
                    md = ""
                    if not err:
                        try:
                            parsed = json.loads(raw_resp)
                            md = (parsed.get("data") or {}).get("markdown", "")
                        except json.JSONDecodeError:
                            err = "invalid_json"
                    visible = f"URL: {url}\n\n{md}" if (not err and md) else ""
                    tokens = counter.count(visible) if visible else 0
                    hits = count_evidence_hits(visible, needles) if visible else 0
                    within = tokens <= budget_tokens
                    per_strategy["firecrawl_api"].append(
                        {
                            "task_id": task["id"],
                            "url": url,
                            "status": "ok" if (not err and md) else f"error:{err or 'empty'}",
                            "representation": "firecrawl:markdown",
                            "sha256": hashlib.sha256(md.encode("utf-8")).hexdigest()[:12] if md else "",
                            "evidence_hits": hits,
                            "evidence_total": len(needles),
                            "within_budget": within,
                            "supported": (not err) and bool(md) and within and hits == len(needles),
                            "tokens": tokens,
                            "latency_ms": elapsed_ms,
                        }
                    )
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)

    summaries: list[dict[str, Any]] = []
    for strat in strategies:
        rows = per_strategy[strat]
        attempted = sum(1 for r in rows if r["status"] != "skipped_no_api_key")
        supported = sum(1 for r in rows if r["supported"])
        ev_full = sum(1 for r in rows if r["evidence_hits"] == r["evidence_total"] and r["status"] == "ok")
        total_tokens = sum(r["tokens"] for r in rows if r["status"] != "skipped_no_api_key")
        lats = sorted(r["latency_ms"] for r in rows if r["status"] != "skipped_no_api_key")
        p95_idx = min(len(lats) - 1, math.ceil(0.95 * len(lats)) - 1) if lats else 0
        notes = sorted({r["status"] for r in rows if r["status"] != "ok"})
        summaries.append(
            {
                "strategy": strat,
                "budget_tokens": budget_tokens,
                "attempted": attempted,
                "total": len(rows),
                "supported": supported,
                "evidence_full": ev_full,
                "mean_tokens": total_tokens / attempted if attempted else 0.0,
                "tokens_per_supported": total_tokens / supported if supported else None,
                "p95_latency_ms": lats[p95_idx] if lats else 0.0,
                "notes": ", ".join(notes) if notes else "ok",
                "tasks": rows,
            }
        )
    return {"budget_tokens": budget_tokens, "task_count": len(live_tasks), "runs": summaries}


def print_live_markdown_report(live_report: dict[str, Any]) -> None:
    print("\n## Opt-In Live-Pair Track\n")
    print(
        f"- **Budget:** {live_report['budget_tokens']} tok | **Live URLs:** {live_report['task_count']}\n"
    )
    print(
        "| Strategy | Supported | Evidence Full | Mean Tok | Tok / Supported | p95 ms | Notes |"
    )
    print("| :--- | ---: | ---: | ---: | ---: | ---: | :--- |")
    for run in live_report["runs"]:
        c_succ = (
            f"{run['tokens_per_supported']:.0f}"
            if run["tokens_per_supported"] is not None
            else "inf"
        )
        print(
            f"| `{run['strategy']}` | {run['supported']}/{run['attempted']} | "
            f"{run['evidence_full']}/{run['attempted']} | {run['mean_tokens']:.0f} | "
            f"{c_succ} | {run['p95_latency_ms']:.1f} | {run['notes']} |"
        )
    print("\n### Per-URL Live Breakdown\n")
    print("| Task | Strategy | Status | Repr | Evidence | Tok | Budget OK | ms |")
    print("| :--- | :--- | :--- | :--- | ---: | ---: | :---: | ---: |")
    for run in live_report["runs"]:
        for r in run["tasks"]:
            ok_mark = "yes" if r["within_budget"] else "no"
            print(
                f"| `{r['task_id']}` | `{run['strategy']}` | {r['status']} | "
                f"`{r['representation']}` | {r['evidence_hits']}/{r['evidence_total']} | "
                f"{r['tokens']} | {ok_mark} | {r['latency_ms']:.1f} |"
            )


def evaluate_batch_tasks(
    batch_tasks: list[dict[str, Any]],
    base_url: str,
    pleb_bin: str,
    budget_tokens: int,
    counter: TokenCounter,
) -> dict[str, Any]:
    """Evaluate multi-URL stdin batch pipelines (`pleb fetch - --jsonl`) comparing clean vs --focus."""
    state_dir = tempfile.mkdtemp(prefix=f"pleb-batch-{os.getpid()}-", dir="/tmp")
    runs: list[dict[str, Any]] = []
    try:
        for strategy in ("pleb_batch_clean", "pleb_batch_focus"):
            rows: list[dict[str, Any]] = []
            for task in batch_tasks:
                urls = [f"{base_url}{p}" for p in task["paths"]]
                extra: list[str] = []
                if strategy == "pleb_batch_focus":
                    per_url_toks = max(250, (budget_tokens - len(urls) * 90) // max(1, len(urls)))
                    extra = [
                        "--focus",
                        task["query"],
                        "--max-bytes",
                        str(per_url_toks * 3),
                    ]
                _, items, elapsed_ms = run_pleb_batch(pleb_bin, state_dir, urls, extra)
                by_path: dict[str, dict[str, Any]] = {}
                visible_parts: list[str] = []
                for it in items:
                    u = it.get("url", "")
                    path = u[len(base_url) :] if u.startswith(base_url) else u
                    by_path[path] = it
                    if it.get("ok"):
                        visible_parts.append(
                            build_model_visible_envelope(it, it.get("content", ""))
                        )
                    else:
                        visible_parts.append(json.dumps(it))

                combined_visible = "\n\n".join(visible_parts)
                tokens = counter.count(combined_visible)
                within_budget = tokens <= budget_tokens
                hits = count_evidence_hits(combined_visible, task["required_evidence"])
                norm_lower = normalize_ws(combined_visible).lower()
                norm_unescaped_lower = normalize_ws(
                    unescape_markdown_punct(combined_visible)
                ).lower()
                leaked = any(
                    normalize_ws(forb).lower() in norm_lower
                    or normalize_ws(forb).lower() in norm_unescaped_lower
                    for forb in task.get("forbidden_substrings", [])
                )

                expected_unavail = task.get("expected_unavailable", {})
                unavail_honest = 0
                false_usable = 0
                for u_path, expected_sig in expected_unavail.items():
                    it = by_path.get(u_path)
                    if it is None:
                        continue
                    if expected_sig == "incomplete_shell":
                        if it.get("ok") and it.get("content_outcome") == "incomplete_shell":
                            unavail_honest += 1
                        elif it.get("ok") and it.get("content_outcome") == "complete":
                            false_usable += 1
                    elif expected_sig == "exit_2":
                        if (not it.get("ok")) and it.get("exit_code") == 2:
                            unavail_honest += 1
                        elif it.get("ok") and it.get("content_outcome") == "complete":
                            false_usable += 1

                supported = (
                    within_budget
                    and hits == len(task["required_evidence"])
                    and unavail_honest == len(expected_unavail)
                    and false_usable == 0
                    and not leaked
                )
                rows.append(
                    {
                        "task_id": task["id"],
                        "url_count": len(urls),
                        "supported": supported,
                        "evidence_hits": hits,
                        "evidence_total": len(task["required_evidence"]),
                        "unavailable_honest": unavail_honest,
                        "unavailable_total": len(expected_unavail),
                        "false_usable": false_usable,
                        "leaked": leaked,
                        "tokens": tokens,
                        "within_budget": within_budget,
                        "latency_ms": elapsed_ms,
                    }
                )
            sup_cnt = sum(1 for r in rows if r["supported"])
            tot_tok = sum(r["tokens"] for r in rows)
            runs.append(
                {
                    "strategy": strategy,
                    "budget_tokens": budget_tokens,
                    "supported": sup_cnt,
                    "total": len(rows),
                    "false_usable": sum(r["false_usable"] for r in rows),
                    "leaks": sum(1 for r in rows if r["leaked"]),
                    "mean_tokens": tot_tok / max(1, len(rows)),
                    "tokens_per_supported": tot_tok / sup_cnt if sup_cnt else None,
                    "tasks": rows,
                }
            )
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)
    return {"budget_tokens": budget_tokens, "runs": runs}


def print_batch_markdown_report(batch_report: dict[str, Any]) -> None:
    print("\n## Multi-URL Batch Track (`pleb fetch - --jsonl`)\n")
    print(f"- **Total Batch Budget:** {batch_report['budget_tokens']} tok across all URLs per task\n")
    print("| Strategy | Supported | False-Usable | Leaks | Mean Batch Tok | Tok / Supported |")
    print("| :--- | ---: | ---: | ---: | ---: | ---: |")
    for r in batch_report["runs"]:
        c_succ = f"{r['tokens_per_supported']:.0f}" if r["tokens_per_supported"] is not None else "inf"
        print(
            f"| `{r['strategy']}` | {r['supported']}/{r['total']} | {r['false_usable']} | "
            f"{r['leaks']} | {r['mean_tokens']:.0f} | {c_succ} |"
        )
    print("\n| Task | URLs | Strategy | Evidence | Unavail Honest | Leaks | Batch Tok | Budget OK | ms |")
    print("| :--- | ---: | :--- | ---: | ---: | ---: | ---: | :---: | ---: |")
    for run in batch_report["runs"]:
        for t in run["tasks"]:
            ok_mark = "yes" if t["within_budget"] else "no"
            print(
                f"| `{t['task_id']}` | {t['url_count']} | `{run['strategy']}` | "
                f"{t['evidence_hits']}/{t['evidence_total']} | "
                f"{t['unavailable_honest']}/{t['unavailable_total']} | "
                f"{1 if t['leaked'] else 0} | {t['tokens']} | {ok_mark} | {t['latency_ms']:.1f} |"
            )


def evaluate_cef_spa_track(
    spa_tasks: list[dict[str, Any]],
    base_url: str,
    pleb_bin: str,
    budget_tokens: int,
    counter: TokenCounter,
) -> dict[str, Any]:
    """Opt-in CEF SPA hydration track comparing static Cronet shell triage vs CEF hydrated full & focus."""
    state_dir = tempfile.mkdtemp(prefix=f"pleb-cef-{os.getpid()}-", dir="/tmp")
    rows: list[dict[str, Any]] = []
    try:
        # Probe whether a ready v5-meta CEF renderer exists via `pleb status --json`.
        probe_env = os.environ.copy()
        probe_env.pop("PLEB_NO_HOME_CEF", None)
        probe_env["PLEB_NO_KEYCHAIN"] = "1"
        probe = subprocess.run(
            [pleb_bin, "--state-dir", state_dir, "status", "--json"],
            capture_output=True,
            text=True,
            env=probe_env,
            check=False,
        )
        cef_ready = False
        if probe.returncode == 0:
            try:
                cef_ready = (json.loads(probe.stdout).get("cef_renderer") or {}).get("status") == "ready"
            except json.JSONDecodeError:
                cef_ready = False

        for task in spa_tasks:
            url = f"{base_url}{task['path']}"
            needles = task["required_evidence"]
            for strat, render_mode, allow_cef, extra in (
                ("pleb_cronet_only", "cronet", False, []),
                ("pleb_cef_full", "cef", True, []),
                (
                    "pleb_cef_focus",
                    "cef",
                    True,
                    ["--focus", task["query"], "--max-bytes", str(max(512, (budget_tokens - 140) * 3))],
                ),
            ):
                if allow_cef and not cef_ready:
                    rows.append(
                        {
                            "task_id": task["id"],
                            "strategy": strat,
                            "status": "skipped_no_cef_renderer",
                            "outcome": "",
                            "evidence_hits": 0,
                            "evidence_total": len(needles),
                            "leaked": False,
                            "tokens": 0,
                            "within_budget": True,
                            "supported": False,
                            "latency_ms": 0.0,
                        }
                    )
                    continue
                rc, out_json, err_json, elapsed_ms = run_pleb(
                    pleb_bin,
                    state_dir,
                    url,
                    extra,
                    allow_home_cef=allow_cef,
                    render_mode=render_mode,
                )
                if rc == 0 and out_json is not None:
                    visible = build_model_visible_envelope(out_json, out_json.get("content", ""))
                    tokens = counter.count(visible)
                    outcome = out_json.get("content_outcome", "")
                    hits = count_evidence_hits(visible, needles)
                    norm_lower = normalize_ws(visible).lower()
                    leaked = any(
                        normalize_ws(forb).lower() in norm_lower
                        for forb in task.get("forbidden_substrings", [])
                    )
                    within = tokens <= budget_tokens
                    if strat == "pleb_cronet_only":
                        supported = outcome == "incomplete_shell" and within
                    else:
                        supported = outcome == "complete" and within and hits == len(needles) and not leaked
                    rows.append(
                        {
                            "task_id": task["id"],
                            "strategy": strat,
                            "status": "ok",
                            "outcome": outcome,
                            "evidence_hits": hits,
                            "evidence_total": len(needles),
                            "leaked": leaked,
                            "tokens": tokens,
                            "within_budget": within,
                            "supported": supported,
                            "latency_ms": elapsed_ms,
                        }
                    )
                else:
                    msg = (err_json or {}).get("error", f"exit_{rc}")
                    rows.append(
                        {
                            "task_id": task["id"],
                            "strategy": strat,
                            "status": f"error:{msg[:40]}",
                            "outcome": "",
                            "evidence_hits": 0,
                            "evidence_total": len(needles),
                            "leaked": False,
                            "tokens": counter.count(json.dumps(err_json or {})),
                            "within_budget": True,
                            "supported": False,
                            "latency_ms": elapsed_ms,
                        }
                    )
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)
    return {"budget_tokens": budget_tokens, "cef_ready": cef_ready, "rows": rows}


def print_cef_markdown_report(cef_report: dict[str, Any]) -> None:
    print("\n## Opt-In CEF SPA Hydration Track\n")
    print(f"- **Budget:** {cef_report['budget_tokens']} tok | **Local CEF Ready:** `{cef_report['cef_ready']}`\n")
    print("| Task | Strategy | Status | Outcome | Evidence | Leaks | Tok | Budget OK | Supported | ms |")
    print("| :--- | :--- | :--- | :--- | ---: | ---: | ---: | :---: | :---: | ---: |")
    for r in cef_report["rows"]:
        print(
            f"| `{r['task_id']}` | `{r['strategy']}` | {r['status']} | `{r['outcome']}` | "
            f"{r['evidence_hits']}/{r['evidence_total']} | {1 if r['leaked'] else 0} | "
            f"{r['tokens']} | {'yes' if r['within_budget'] else 'no'} | "
            f"{'yes' if r['supported'] else 'no'} | {r['latency_ms']:.1f} |"
        )


def print_markdown_report(report: dict[str, Any]) -> None:
    print("# pleb Held-Out Task Evaluation Scorecard\n")
    print(f"- **Tokenizer:** `{report['tokenizer']}`")
    print(f"- **Binary:** `{report['pleb_bin']}`")
    print(
        f"- **Tasks:** {report['task_count']} ({report['answerable_count']} answerable "
        f"[incl. {report['wild_count']} wild 42-502 KB docs], "
        f"{report['unavailable_count']} unavailable-evidence)\n"
    )
    print(
        "| Budget (tok) | Strategy | S(B) | Answerable | Wild Docs | Unavailable Honest | "
        "False-Usable | Leaks | Mean Tok | Tok / Supported | p95 ms |"
    )
    print(
        "| ---: | :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    )
    for run in report["runs"]:
        c_succ = (
            f"{run['tokens_per_supported_completion']:.0f}"
            if run["tokens_per_supported_completion"] is not None
            else "inf"
        )
        print(
            f"| {run['budget_tokens']} | `{run['strategy']}` | "
            f"{run['supported_completions']}/{run['tasks_attempted']} ({run['S_B']:.3f}) | "
            f"{run['answerable_supported']}/{run['answerable_total']} | "
            f"{run['wild_supported']}/{run['wild_total']} | "
            f"{run['unavailable_honest']}/{run['unavailable_total']} | "
            f"{run['false_usable_count']} | "
            f"{run['injection_leaked_count']} | {run['mean_tokens']:.0f} | "
            f"{c_succ} | {run['p95_latency_ms']:.1f} |"
        )


def main() -> None:
    parser = argparse.ArgumentParser(description="Run held-out task evaluation for pleb.")
    parser.add_argument(
        "--pleb-bin",
        default=str(ROOT_DIR.parent.parent / "target" / "release" / "pleb"),
        help="Path to compiled pleb binary",
    )
    parser.add_argument(
        "--budgets",
        default="1200,2000,4000",
        help="Comma-separated model-visible token budgets",
    )
    parser.add_argument(
        "--no-competitors",
        action="store_true",
        help="Skip external Python competitor extractors (trafilatura, html2text, crawl4ai_bm25_ref)",
    )
    parser.add_argument(
        "--no-warmup",
        action="store_true",
        help="Skip single warm-up CLI invocation before timing (measures cold binary load on first run)",
    )
    parser.add_argument(
        "--live-track",
        action="store_true",
        help="Also run opt-in paired live-URL comparison (pleb vs Accept:text/markdown, r.jina.ai, Firecrawl)",
    )
    parser.add_argument(
        "--live-only",
        action="store_true",
        help="Run only the opt-in live-URL comparison track (skip offline frozen fixtures)",
    )
    parser.add_argument(
        "--live-competitors",
        default="accept_markdown,jina,firecrawl",
        help="Comma-separated live competitors: accept_markdown,jina,firecrawl",
    )
    parser.add_argument(
        "--live-budget",
        type=int,
        default=4000,
        help="Model-visible token budget for the live-pair track",
    )
    parser.add_argument(
        "--batch-budget",
        type=int,
        default=4000,
        help="Total model-visible token budget across all URLs in each multi-URL batch task",
    )
    parser.add_argument(
        "--no-batch",
        action="store_true",
        help="Skip multi-URL stdin batch (`pleb fetch - --jsonl`) evaluation track",
    )
    parser.add_argument(
        "--cef-track",
        action="store_true",
        help="Run opt-in local CEF SPA hydration track (requires installed v5-meta CEF renderer)",
    )
    parser.add_argument(
        "--cef-budget",
        type=int,
        default=1500,
        help="Model-visible token budget for the CEF SPA hydration track",
    )
    parser.add_argument("--json-out", help="Optional path to write full JSON report")
    args = parser.parse_args()

    manifest = json.loads(TASKS_PATH.read_text(encoding="utf-8"))
    tasks = manifest["tasks"]
    live_tasks = manifest.get("live_tasks", [])
    batch_tasks = manifest.get("batch_tasks", [])
    spa_tasks = manifest.get("spa_tasks", [])
    budgets = [int(x.strip()) for x in args.budgets.split(",") if x.strip()]
    counter = TokenCounter()

    pleb_strategies = [
        "pleb_clean_full",
        "pleb_article_full",
        "pleb_prefix_budget",
        "pleb_focus_cli",
    ]
    external_strategies = (
        []
        if args.no_competitors
        else ["trafilatura_full", "html2text_full", "crawl4ai_bm25_ref"]
    )

    runs: list[dict[str, Any]] = []
    batch_report = None
    cef_report = None
    if (not args.live_only) or args.cef_track:
        with FixtureHTTPServer(tasks + spa_tasks) as srv:
            if not args.live_only:
                if not args.no_warmup and tasks:
                    warm_dir = tempfile.mkdtemp(prefix=f"pleb-warm-{os.getpid()}-", dir="/tmp")
                    try:
                        run_pleb(args.pleb_bin, warm_dir, f"{srv.base_url}{tasks[0]['path']}", [])
                    finally:
                        shutil.rmtree(warm_dir, ignore_errors=True)
                for budget in budgets:
                    for strategy in pleb_strategies:
                        runs.append(
                            evaluate_strategy(
                                strategy=strategy,
                                tasks=tasks,
                                base_url=srv.base_url,
                                pleb_bin=args.pleb_bin,
                                budget_tokens=budget,
                                counter=counter,
                            )
                        )
                    for strategy in external_strategies:
                        runs.append(
                            evaluate_external_extractor(
                                strategy=strategy,
                                tasks=tasks,
                                budget_tokens=budget,
                                counter=counter,
                            )
                        )
                if batch_tasks and not args.no_batch:
                    batch_report = evaluate_batch_tasks(
                        batch_tasks=batch_tasks,
                        base_url=srv.base_url,
                        pleb_bin=args.pleb_bin,
                        budget_tokens=args.batch_budget,
                        counter=counter,
                    )
            if args.cef_track and spa_tasks:
                cef_report = evaluate_cef_spa_track(
                    spa_tasks=spa_tasks,
                    base_url=srv.base_url,
                    pleb_bin=args.pleb_bin,
                    budget_tokens=args.cef_budget,
                    counter=counter,
                )

    live_report = None
    if args.live_track or args.live_only:
        live_competitors = [
            c.strip() for c in args.live_competitors.split(",") if c.strip()
        ]
        live_report = evaluate_live_track(
            live_tasks=live_tasks,
            pleb_bin=args.pleb_bin,
            budget_tokens=args.live_budget,
            competitors=live_competitors,
            counter=counter,
        )

    report = {
        "tokenizer": counter.name,
        "pleb_bin": args.pleb_bin,
        "task_count": len(tasks),
        "answerable_count": sum(1 for t in tasks if t["answerable"]),
        "wild_count": sum(1 for t in tasks if t["family"] == "wild_production_docs"),
        "unavailable_count": sum(1 for t in tasks if not t["answerable"]),
        "runs": runs,
        "batch_track": batch_report,
        "cef_track": cef_report,
        "live_track": live_report,
    }
    if runs:
        print_markdown_report(report)
    if batch_report is not None:
        print_batch_markdown_report(batch_report)
    if cef_report is not None:
        print_cef_markdown_report(cef_report)
    if live_report is not None:
        print_live_markdown_report(live_report)
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
