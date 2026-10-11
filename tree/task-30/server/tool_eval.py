#!/usr/bin/env python3
"""Agentic docs-tool eval for chat model candidates (MODEL_TRIAL.md).

Drives the same three docs tools the TUI attaches (docs_list/docs_read/
docs_search, schemas from src/docs_mcp.rs) against any OpenAI-compatible
endpoint and scores answers on docs/control.json with its machine-checkable
`must` / `wrong` patterns. Tools run locally over pdftotext output.

    tool_eval.py URL [--ids 1,5,...] [--label name] [--key KEY]
"""
import argparse, json, re, subprocess, sys, time, urllib.request
from pathlib import Path

DOCS = Path(__file__).resolve().parent.parent / "docs"
SYSTEM = ("You are a helpful assistant. Tools give you the raw documents of a folder (PDF papers). "
          "To answer a question about them: `docs_list` to see files, pages and section names, then "
          "`docs_read` the pages that hold the answer (or `docs_search` for a term first). Cite the file "
          "and pages you used. If the documents do not cover the question, say so instead of guessing.")
TOOLS = [
    {"name": "docs_list", "description": "List the documents of the folder: file name, title, pages, size and section names with their pages.",
     "parameters": {"type": "object", "properties": {}}},
    {"name": "docs_read", "description": "Read the raw text of a document, a page range at a time (PDF pages). Long ranges are cut at max_chars with a hint where to continue.",
     "parameters": {"type": "object", "properties": {
         "file": {"type": "string", "description": "File name from docs_list, e.g. paper.pdf."},
         "from_page": {"type": "integer", "minimum": 1, "description": "First page, default 1."},
         "to_page": {"type": "integer", "minimum": 1, "description": "Last page, default the last one."},
         "max_chars": {"type": "integer", "minimum": 500, "maximum": 20000, "description": "Cap on returned text, default 10000."}},
         "required": ["file"]}},
    {"name": "docs_search", "description": "Find lines that contain every word of the query (case-insensitive), with file and page.",
     "parameters": {"type": "object", "properties": {
         "query": {"type": "string"},
         "file": {"type": "string", "description": "Only this file (default: all)."},
         "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Default 20."}},
         "required": ["query"]}},
]
PAGES = {}


def pages(f):
    if f not in PAGES:
        out = subprocess.run(["pdftotext", "-layout", str(DOCS / f), "-"], capture_output=True, text=True).stdout
        PAGES[f] = out.split("\f")[:-1] or [out]
    return PAGES[f]


def run_tool(name, a):
    files = sorted(p.name for p in DOCS.glob("*.pdf"))
    if name == "docs_list":
        return "\n".join(f"{f} — {len(pages(f))} pages" for f in files)
    f = a.get("file")
    if f and f not in files:
        return f"error: no file {f!r}; call docs_list"
    if name == "docs_read":
        ps = pages(f)
        lo, hi = int(a.get("from_page", 1)), int(a.get("to_page", len(ps)))
        txt = "".join(f"\n--- page {i} ---\n{ps[i-1]}" for i in range(max(lo, 1), min(hi, len(ps)) + 1))
        cap = int(a.get("max_chars", 10000))
        return txt[:cap] + (f"\n[cut at {cap} chars; continue with a later from_page]" if len(txt) > cap else "")
    if name == "docs_search":
        words = str(a.get("query", "")).lower().split()
        hits = []
        for g in ([f] if f else files):
            for i, p in enumerate(pages(g), 1):
                for line in p.splitlines():
                    if words and all(w in line.lower() for w in words):
                        hits.append(f"{g}:{i}: {line.strip()[:200]}")
        return "\n".join(hits[: int(a.get("limit", 20))]) or "no matches"
    return f"error: unknown tool {name}"


def post(url, key, body):
    req = urllib.request.Request(url + "/v1/chat/completions", json.dumps(body).encode(),
                                 {"Content-Type": "application/json", **({"Authorization": f"Bearer {key}"} if key else {})})
    with urllib.request.urlopen(req, timeout=600) as r:
        return json.load(r)


def ask(url, key, model, q, max_steps=8):
    msgs = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": q}]
    calls, prompt_tok, gen_tok = [], 0, 0
    for _ in range(max_steps):
        r = post(url, key, {"model": model, "messages": msgs, "temperature": 0.2, "max_tokens": 1024,
                            "tools": [{"type": "function", "function": t} for t in TOOLS]})
        u = r.get("usage", {})
        prompt_tok, gen_tok = u.get("prompt_tokens", 0), gen_tok + u.get("completion_tokens", 0)
        m = r["choices"][0]["message"]
        tcs = m.get("tool_calls") or []
        if not tcs:
            return m.get("content") or "", calls, prompt_tok, gen_tok
        msgs.append({"role": "assistant", "content": m.get("content") or "", "tool_calls": tcs})
        for tc in tcs:
            try:
                a = json.loads(tc["function"].get("arguments") or "{}")
            except json.JSONDecodeError:
                a = {}
            calls.append(f'{tc["function"]["name"]}({json.dumps(a, ensure_ascii=False)})')
            msgs.append({"role": "tool", "tool_call_id": tc.get("id", ""), "content": run_tool(tc["function"]["name"], a)[:12000]})
    return "[step limit]", calls, prompt_tok, gen_tok


def score(item, ans):
    a = ans.lower()
    if item["must"]:
        return all(re.search(g, a) for g in item["must"])
    return not any(re.search(w, a) for w in item.get("wrong", []))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("url")
    ap.add_argument("--key", default="")
    ap.add_argument("--model", default="m")
    ap.add_argument("--ids", default="")
    ap.add_argument("--label", default="")
    o = ap.parse_args()
    items = json.load(open(DOCS / "control.json"))
    if o.ids:
        keep = {int(x) for x in o.ids.split(",")}
        items = [i for i in items if i["id"] in keep]
    ok = used = 0
    t0 = time.time()
    for it in items:
        t = time.time()
        try:
            ans, calls, pt, gt = ask(o.url, o.key, o.model, it["q"])
        except Exception as e:  # report and keep going
            ans, calls, pt, gt = f"[error {e}]", [], 0, 0
        good = score(it, ans)
        ok += good
        used += bool(calls)
        print(json.dumps({"id": it["id"], "pass": good, "calls": calls, "s": round(time.time() - t, 1),
                          "gen_tok": gt, "answer": ans[:400]}, ensure_ascii=False), flush=True)
    print(f"# {o.label}: pass {ok}/{len(items)}, used tools {used}/{len(items)}, {time.time() - t0:.0f}s", flush=True)


if __name__ == "__main__":
    sys.exit(main())
