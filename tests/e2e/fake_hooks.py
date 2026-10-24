#!/usr/bin/env python3
"""Records every POST (path, headers, body) as a JSON line: a stand-in for ntfy and webhooks.
Usage: fake_hooks.py PORT OUT.jsonl"""
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

port, out = int(sys.argv[1]), sys.argv[2]

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        with open(out, "a") as f:
            f.write(json.dumps({"path": self.path, "headers": {k.lower(): v for k, v in self.headers.items()}, "body": body.decode("utf-8", "replace")}) + "\n")
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
