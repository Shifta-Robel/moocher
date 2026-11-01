#!/usr/bin/env python3
"""Deterministic OpenAI-compatible mock LLM for tests and offline demos.

The *model name* selects a script in ``scripts/<model>.json``. A script is a
list of steps; each assistant turn emits the next step that has not run yet.
Steps are counted per conversation by looking at how many assistant turns
already exist after the most recent user message. Step kinds:

    {"tool": "write", "args": {...}}   -> one tool call
    {"text": "..."}                    -> final text, ends the turn
    {"text": "...", "usage": {"cost": 60}, "headers": {"x-neurons": 7}}
                                       -> extra numbers in the usage, extra response
                                          headers (streamed answers)
    {"error": {"status": 400, "body": {...}}, "times": 1}
                                       -> answer with that HTTP error, the first
                                          `times` times only (then it is skipped)
    any step may add "delay": seconds  -> sleep before answering (to demo steering)

A script may instead be ``{"phases": [{"match": "text", "steps": [...]}, ...]}``:
the first phase whose ``match`` occurs in the latest user message is used (a
phase without ``match`` is the default). This lets one model react to the
cluster's follow-up messages (e.g. "Session X (builder) finished a turn").

String values may contain ``{{last_user}}`` (the last user message text) and
``{{match:REGEX}}`` (the first match of REGEX in it, e.g. ``{{match:q-[0-9a-f]+}}``
to answer a question by id).
Usage is reported with simple len/4 token estimates so metering can be tested.

Rate limits like a keyless gateway: ``--ip-quota N --ip-window SECS`` allows N
chat requests per client IP per fixed window and answers with OpenAI-style
``x-ratelimit-*`` headers, then 429 + ``retry-after``. ``GET /ip`` echoes the
caller's address (like api.ipify.org); ``GET /_log`` shows recent requests.

Usage: mock_llm.py --port 8099 [--scripts DIR] [--require-key KEY] [--ip-quota N --ip-window SECS]
"""
import argparse
import json
import os
import re
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCRIPTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scripts")
LOG = []
REQUIRE_KEY = None
IP_QUOTA = 0
IP_WINDOW = 60
WINDOWS = {}  # client ip -> [window start, count]
STATS = {"chat": 0, "rate_limited": 0, "by_ip": {}}  # since start (the log keeps 50)
ERRORS_SENT = {}  # (model, phase, step index) -> how often that error step answered


def est_tokens(obj):
    return max(1, len(json.dumps(obj)) // 4)


def text_of(content):
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def render(value, ctx):
    if isinstance(value, str):
        for k, v in ctx.items():
            value = value.replace("{{" + k + "}}", v)
        def sub(m):
            found = re.search(m.group(1), ctx.get("last_user", ""))
            return found.group(0) if found else ""
        return re.sub(r"\{\{match:(.+?)\}\}", sub, value)
    if isinstance(value, dict):
        return {k: render(v, ctx) for k, v in value.items()}
    if isinstance(value, list):
        return [render(v, ctx) for v in value]
    return value


def next_step(model, messages):
    path = os.path.join(SCRIPTS, f"{model}.json")
    try:
        with open(path) as f:
            script = json.load(f)
    except FileNotFoundError:
        script = [{"text": f"mock: no script for model {model!r}"}]
    last_user = max((i for i, m in enumerate(messages) if m.get("role") == "user"), default=-1)
    done = sum(1 for m in messages[last_user + 1:] if m.get("role") == "assistant")
    ctx = {"last_user": text_of(messages[last_user]["content"]) if last_user >= 0 else ""}
    if isinstance(script, dict):
        steps = None
        for ph in script.get("phases", []):
            if "match" not in ph or ph["match"] in ctx["last_user"]:
                steps = ph["steps"]
                break
        steps = steps or [{"text": "mock: no phase matched"}]
    else:
        steps = script
    # Error steps that have answered `times` times are gone.
    phase = json.dumps(steps)[:200]
    live = [(i, st) for i, st in enumerate(steps) if "error" not in st or ERRORS_SENT.get((model, phase, i), 0) < st.get("times", 1)]
    if done >= len(live):
        return {"text": "done."}
    i, step = live[done]
    if "error" in step:
        ERRORS_SENT[(model, phase, i)] = ERRORS_SENT.get((model, phase, i), 0) + 1
    return render(step, ctx)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        for k, v in getattr(self, "_rl", {}).items():
            self.send_header(k, v)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _quota(self):
        """(allowed, headers) for this client's fixed-window request budget."""
        if not IP_QUOTA or (QUOTA_BY_KEY and self.headers.get("authorization") == f"Bearer {REQUIRE_KEY}"):
            return True, {}
        ip, now = (self.headers.get("authorization") if QUOTA_BY_KEY else self.client_address[0]), time.time()
        start, count = WINDOWS.get(ip, (now, 0))
        if now - start >= IP_WINDOW:
            start, count = now, 0
        reset = max(1, int(start + IP_WINDOW - now))
        allowed = count < IP_QUOTA
        if allowed:
            count += 1
        WINDOWS[ip] = (start, count)
        return allowed, {
            "x-ratelimit-limit-requests": str(IP_QUOTA),
            "x-ratelimit-remaining-requests": str(IP_QUOTA - count),
            "x-ratelimit-reset-requests": f"{reset}s",
            **({} if allowed else {"retry-after": str(reset)}),
        }

    def do_GET(self):
        if self.path.rstrip("/").endswith("/ip"):
            data = self.client_address[0].encode()
            self.send_response(200)
            self.send_header("content-type", "text/plain")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            return self.wfile.write(data)
        if self.path.endswith("/models"):
            names = sorted(f[:-5] for f in os.listdir(SCRIPTS) if f.endswith(".json"))
            return self._json(200, {"object": "list", "data": [{"id": n, "object": "model"} for n in names]})
        if self.path.endswith("/_stats"):
            return self._json(200, STATS)
        if self.path.endswith("/_log"):
            return self._json(200, LOG[-50:])
        self._json(404, {"error": "not found"})

    def do_POST(self):
        self._rl = {}
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        LOG.append({"path": self.path, "auth": self.headers.get("authorization"), "model": body.get("model"), "client": self.client_address[0],
                    "stream": bool(body.get("stream")), "include_usage": bool((body.get("stream_options") or {}).get("include_usage")),
                    "messages": body.get("messages", [])})
        if not self.path.endswith("/chat/completions"):
            return self._json(404, {"error": "not found"})
        if REQUIRE_KEY and self.headers.get("authorization") not in [f"Bearer {k}" for k in [REQUIRE_KEY, *ACCEPT_KEYS]]:
            return self._json(401, {"error": {"message": "mock: wrong or missing API key"}})
        allowed, rl = self._quota()
        ip = self.client_address[0]
        STATS["chat"] += 1
        STATS["by_ip"][ip] = STATS["by_ip"].get(ip, 0) + 1
        if not allowed:
            STATS["rate_limited"] += 1
            data = json.dumps({"error": {"message": "mock: rate limit exceeded for this IP", "type": "rate_limit"}}).encode()
            self.send_response(429)
            for k, v in rl.items():
                self.send_header(k, v)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            return self.wfile.write(data)
        self._rl = rl
        model = body.get("model", "default")
        messages = body.get("messages", [])
        step = next_step(model, messages)
        if step.get("delay"):
            time.sleep(float(step["delay"]))
        if "error" in step:
            e = step["error"]
            return self._json(int(e.get("status", 500)), e.get("body", {"error": {"message": "mock error"}}))
        cid = "chatcmpl-" + uuid.uuid4().hex[:12]
        prompt_tokens = est_tokens(messages)
        if "tool" in step:
            delta = {"role": "assistant", "content": None, "tool_calls": [{
                "index": 0, "id": "call_" + uuid.uuid4().hex[:12], "type": "function",
                "function": {"name": step["tool"], "arguments": json.dumps(step.get("args", {}))}}]}
            finish = "tool_calls"
        else:
            delta = {"role": "assistant", "content": step.get("text", "")}
            finish = "stop"
        completion_tokens = est_tokens(delta)
        usage = {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                 "total_tokens": prompt_tokens + completion_tokens, **step.get("usage", {})}
        if not body.get("stream"):
            msg = {k: v for k, v in delta.items()}
            return self._json(200, {"id": cid, "object": "chat.completion", "created": int(time.time()), "model": model,
                                    "choices": [{"index": 0, "message": msg, "finish_reason": finish}], "usage": usage})
        self.send_response(200)
        for k, v in self._rl.items():
            self.send_header(k, v)
        for k, v in step.get("headers", {}).items():
            self.send_header(k, str(v))
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        chunks = [
            {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
        ]
        # Like OpenAI: streams carry usage only when the request asks for it.
        if (body.get("stream_options") or {}).get("include_usage"):
            chunks.append({"choices": [], "usage": usage})
        for c in chunks:
            c.update({"id": cid, "object": "chat.completion.chunk", "created": int(time.time()), "model": model})
            self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


ACCEPT_KEYS, QUOTA_BY_KEY = [], False


def main():
    global SCRIPTS, REQUIRE_KEY, IP_QUOTA, IP_WINDOW, ACCEPT_KEYS, QUOTA_BY_KEY
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--scripts", default=SCRIPTS)
    ap.add_argument("--require-key", default=os.environ.get("MOCK_REQUIRE_KEY"),
                    help="reject requests whose bearer token is not this key (proves keyproxy injection)")
    ap.add_argument("--ip-quota", type=int, default=0, help="chat requests allowed per client IP per window (0 = unlimited)")
    ap.add_argument("--ip-window", type=int, default=60, help="quota window in seconds")
    ap.add_argument("--accept-key", action="append", default=[], help="another key --require-key lets through")
    ap.add_argument("--quota-by-key", action="store_true", help="--ip-quota counts per --accept-key, not per IP")
    a = ap.parse_args()
    ACCEPT_KEYS, QUOTA_BY_KEY = a.accept_key, a.quota_by_key
    SCRIPTS = a.scripts
    REQUIRE_KEY = a.require_key
    IP_QUOTA, IP_WINDOW = a.ip_quota, a.ip_window
    print(f"mock-llm listening on http://{a.host}:{a.port}/v1 scripts={SCRIPTS}", flush=True)
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
