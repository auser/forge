#!/usr/bin/env python3
"""Reference HTTP adapter exposing a Laya decision model to Forge.

Laya (https://github.com/artificial-intelligence-works/laya-jev) is an
open-source System One decision model with a Python SDK and no official
HTTP server. This adapter bridges Forge's `router = "laya"` mode to a
local Laya instance.

Requires: `pip install laya` (Python). Forge never requires Python; this
adapter is optional. Without it, use `router = "http"` against any System
One-compatible service, or the built-in `static`/`cheapest` routers.

Contract (Forge -> adapter):
  POST /decide  {"state": {"task": ..., "required_capabilities": [...]},
                 "questions": {"model": {"type": "choice",
                                          "instructions": ...,
                                          "criteria": {name: description}}}}
Adapter -> Forge:
  {"answers": {"model": {"choice": "<candidate>", "confidence": 0.0-1.0}},
   "routing": {"backend": "laya"}}

Listens on 127.0.0.1:8788 by default. Usage: `python3 laya-http.py [port]`.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DEFAULT_PORT = 8788


def load_router():
    """Load Laya; fail with actionable guidance when it is not installed."""
    try:
        import laya
    except ImportError:
        sys.stderr.write(
            "laya is not installed. Install it with: pip install laya\n"
        )
        sys.exit(2)
    return laya.Router(preload=True)


ROUTER = None  # lazily initialized in main()


def decide(payload: dict) -> dict:
    """Ask Laya the typed choice question and shape the answer for Forge."""
    state = payload.get("state", {})
    questions = payload.get("questions", {})
    # Laya answers typed questions: a "choice" question with per-candidate
    # criteria text returns {"choice": name, "confidence": float}.
    result = ROUTER.decide(state=state, questions=questions)
    answers = {}
    for name, question in questions.items():
        answer = getattr(result, "answers", {}).get(name, {})
        if isinstance(answer, dict) and "choice" in answer:
            answers[name] = {
                "choice": answer["choice"],
                "confidence": float(answer.get("confidence", 0.0)),
            }
    return {"answers": answers, "routing": {"backend": "laya"}}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        try:
            payload = json.loads(self.rfile.read(length) or b"{}")
        except json.JSONDecodeError:
            self._respond(400, {"error": "invalid JSON"})
            return
        if self.path.rstrip("/") not in ("/decide", ""):
            self._respond(404, {"error": "unknown path"})
            return
        try:
            self._respond(200, decide(payload))
        except Exception as exc:  # decision backend failure
            self._respond(500, {"error": str(exc)})

    def do_GET(self):  # cheap liveness for `forge doctor`
        self._respond(200, {"status": "ok", "backend": "laya"})

    def _respond(self, status: int, body: dict):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):  # quiet; Forge owns the logs
        pass


def main() -> None:
    global ROUTER
    import argparse

    parser = argparse.ArgumentParser(description="Laya HTTP adapter for Forge")
    parser.add_argument("port", nargs="?", type=int, default=DEFAULT_PORT)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    ROUTER = load_router()
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"laya-http adapter listening on http://{args.host}:{args.port}/decide")
    server.serve_forever()


if __name__ == "__main__":
    main()
