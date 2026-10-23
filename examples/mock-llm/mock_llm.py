#!/usr/bin/env python3
"""Deterministic OpenAI-compatible mock LLM for tests and offline demos.

The *model name* selects a script in ``scripts/<model>.json``. A script is a
list of steps; each assistant turn emits the next step that has not run yet.
Steps are counted per conversation by looking at how many assistant turns
already exist after the most recent user message. Step kinds:

    {"tool": "write", "args": {...}}   -> one tool call
    {"text": "..."}                    -> final text, ends the turn
    any step may add "delay": seconds  -> sleep before answering (to demo steering)

A script may instead be ``{"phases": [{"match": "text", "steps": [...]}, ...]}``:
the first phase whose ``match`` occurs in the latest user message is used (a
phase without ``match`` is the default). This lets one model react to the
cluster's follow-up messages (e.g. "Session X (builder) finished a turn").

String values may contain ``{{last_user}}`` (the last user message text).
Usage is reported with simple len/4 token estimates so metering can be tested.

Usage: mock_llm.py --port 8099 [--scripts DIR] [--require-key KEY]
"""
import argparse
import json
import os
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCRIPTS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "scripts")
LOG = []
REQUIRE_KEY = None


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
        return value
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
    if done >= len(steps):
        return {"text": "done."}
    return render(steps[done], ctx)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.endswith("/models"):
            names = sorted(f[:-5] for f in os.listdir(SCRIPTS) if f.endswith(".json"))
            return self._json(200, {"object": "list", "data": [{"id": n, "object": "model"} for n in names]})
        if self.path.endswith("/_log"):
            return self._json(200, LOG[-50:])
        self._json(404, {"error": "not found"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        LOG.append({"path": self.path, "auth": self.headers.get("authorization"), "model": body.get("model")})
        if not self.path.endswith("/chat/completions"):
            return self._json(404, {"error": "not found"})
        if REQUIRE_KEY and self.headers.get("authorization") != f"Bearer {REQUIRE_KEY}":
            return self._json(401, {"error": {"message": "mock: wrong or missing API key"}})
        model = body.get("model", "default")
        messages = body.get("messages", [])
        step = next_step(model, messages)
        if step.get("delay"):
            time.sleep(float(step["delay"]))
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
                 "total_tokens": prompt_tokens + completion_tokens}
        if not body.get("stream"):
            msg = {k: v for k, v in delta.items()}
            return self._json(200, {"id": cid, "object": "chat.completion", "created": int(time.time()), "model": model,
                                    "choices": [{"index": 0, "message": msg, "finish_reason": finish}], "usage": usage})
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        chunks = [
            {"choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
            {"choices": [], "usage": usage},
        ]
        for c in chunks:
            c.update({"id": cid, "object": "chat.completion.chunk", "created": int(time.time()), "model": model})
            self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def main():
    global SCRIPTS, REQUIRE_KEY
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8099)
    ap.add_argument("--scripts", default=SCRIPTS)
    ap.add_argument("--require-key", default=os.environ.get("MOCK_REQUIRE_KEY"),
                    help="reject requests whose bearer token is not this key (proves keyproxy injection)")
    a = ap.parse_args()
    SCRIPTS = a.scripts
    REQUIRE_KEY = a.require_key
    print(f"mock-llm listening on http://{a.host}:{a.port}/v1 scripts={SCRIPTS}", flush=True)
    ThreadingHTTPServer((a.host, a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
