#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "tiktoken>=0.8.0",
# ]
# ///
"""Held-out task-level evaluation harness for pleb (external to the production binary).

Evaluates S(B) (evidence-supported task completions within pinned model-visible token budget B),
C_success (model-visible tokens per supported completion), false-usable / usable-rejected
confusion counts, concealed-injection leakage, and wall latency across:
  - pleb_clean_full:     unbounded default clean Markdown
  - pleb_article_full:   unbounded article extraction
  - pleb_prefix_budget:  existing --max-bytes prefix truncation at budget B
  - pleb_focus_proto:    prototype heading-aware BM25 + enclosing-heading projection over clean Markdown

Run with pinned o200k_base tokenization via uv:
  uv run bench/task_eval/evaluate.py --pleb-bin ./target/release/pleb
"""

from __future__ import annotations

import argparse
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


def project_focus_markdown(
    markdown: str, query: str, budget_tokens: int, counter: TokenCounter
) -> str:
    """Prototype heading-aware BM25 projection preserving intact sections, code, tables, and TOC."""
    if counter.count(markdown) <= budget_tokens:
        return markdown

    preamble, sections = split_markdown_sections(markdown)
    if not sections:
        return markdown

    q_terms = tokenize_terms(query)
    if not q_terms:
        return markdown

    doc_tokens = [tokenize_terms(f"{s.heading}\n{s.body}") for s in sections]
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
        heading_terms = set(tokenize_terms(sections[idx].heading))
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
        # Modest lead prior for the document title / overview section (index 0)
        if idx == 0:
            score += 0.35
        scores.append(score)

    def render_selected(selected_indices: set[int]) -> str:
        parts: list[str] = []
        if preamble:
            parts.append(preamble)
        collapsed_headings: list[str] = []
        for s in sections:
            if s.index in selected_indices:
                block = f"{s.heading}\n\n{s.body}" if s.body else s.heading
                parts.append(block)
            else:
                clean_h = re.sub(r"^#+\s*", "", s.heading).strip()
                collapsed_headings.append(clean_h)
        if collapsed_headings:
            parts.append(
                "<!-- Collapsed sections: " + " | ".join(collapsed_headings) + " -->"
            )
        return "\n\n".join(p for p in parts if p)

    # Always keep the document title heading (if H1 is section 0) as framing when affordable.
    ranked = sorted(range(n_docs), key=lambda i: (-scores[i], i))
    selected: set[int] = set()
    if sections[0].level == 1:
        cand = render_selected({0})
        if counter.count(cand) <= budget_tokens:
            selected.add(0)

    for idx in ranked:
        if idx in selected:
            continue
        trial = set(selected)
        trial.add(idx)
        # Include immediate parent heading if this is a nested section
        if sections[idx].level > 1:
            for prev in range(idx - 1, -1, -1):
                if sections[prev].level < sections[idx].level:
                    if not sections[prev].body or len(sections[prev].body) < 400:
                        trial.add(prev)
                    break
        rendered = render_selected(trial)
        if counter.count(rendered) <= budget_tokens:
            selected = trial

    if not selected:
        # Fallback: pick highest-scoring section and trim cleanly by lines inside budget.
        top = ranked[0]
        raw = render_selected({top})
        lines = raw.splitlines()
        kept: list[str] = []
        for line in lines:
            candidate = "\n".join(kept + [line])
            if counter.count(candidate) > budget_tokens:
                break
            kept.append(line)
        return "\n".join(kept)

    return render_selected(selected)


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
    return re.sub(r"\\([\\`*_{}\[\]()#+\-.!~|])", r"\1", text)


def run_pleb(
    pleb_bin: str,
    state_dir: str,
    url: str,
    extra_args: list[str],
) -> tuple[int, dict[str, Any] | None, dict[str, Any] | None, float]:
    env = os.environ.copy()
    env["PLEB_NO_HOME_CEF"] = "1"
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
        "cronet",
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

                norm_visible = normalize_ws(visible)
                hits = sum(
                    1
                    for needle in task["required_evidence"]
                    if normalize_ws(needle) in norm_visible
                )
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

        total_tasks = len(rows)
        supported_count = sum(1 for r in rows if r["supported"])
        ans_rows = [r for r in rows if r["answerable"]]
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
    finally:
        shutil.rmtree(state_dir, ignore_errors=True)


def print_markdown_report(report: dict[str, Any]) -> None:
    print("# pleb Held-Out Task Evaluation Scorecard\n")
    print(f"- **Tokenizer:** `{report['tokenizer']}`")
    print(f"- **Binary:** `{report['pleb_bin']}`")
    print(
        f"- **Tasks:** {report['task_count']} ({report['answerable_count']} answerable, "
        f"{report['unavailable_count']} unavailable-evidence)\n"
    )
    print(
        "| Budget (tok) | Strategy | S(B) | Answerable | Unavailable Honest | "
        "False-Usable | Usable-Rejected | Leaks | Mean Tok | Tok / Supported | p95 ms |"
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
            f"{run['unavailable_honest']}/{run['unavailable_total']} | "
            f"{run['false_usable_count']} | {run['usable_rejected_count']} | "
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
        "--no-warmup",
        action="store_true",
        help="Skip single warm-up CLI invocation before timing (measures cold binary load on first run)",
    )
    parser.add_argument("--json-out", help="Optional path to write full JSON report")
    args = parser.parse_args()

    manifest = json.loads(TASKS_PATH.read_text(encoding="utf-8"))
    tasks = manifest["tasks"]
    budgets = [int(x.strip()) for x in args.budgets.split(",") if x.strip()]
    counter = TokenCounter()

    strategies = [
        "pleb_clean_full",
        "pleb_article_full",
        "pleb_prefix_budget",
        "pleb_focus_proto",
    ]

    runs: list[dict[str, Any]] = []
    with FixtureHTTPServer(tasks) as srv:
        if not args.no_warmup and tasks:
            warm_dir = tempfile.mkdtemp(prefix=f"pleb-warm-{os.getpid()}-", dir="/tmp")
            try:
                run_pleb(args.pleb_bin, warm_dir, f"{srv.base_url}{tasks[0]['path']}", [])
            finally:
                shutil.rmtree(warm_dir, ignore_errors=True)
        for budget in budgets:
            for strategy in strategies:
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

    report = {
        "tokenizer": counter.name,
        "pleb_bin": args.pleb_bin,
        "task_count": len(tasks),
        "answerable_count": sum(1 for t in tasks if t["answerable"]),
        "unavailable_count": sum(1 for t in tasks if not t["answerable"]),
        "runs": runs,
    }
    print_markdown_report(report)
    if args.json_out:
        Path(args.json_out).write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
