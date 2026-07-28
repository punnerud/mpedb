#!/usr/bin/env python3.12
"""An HTTP face for `system.py`, so a client can be black-box.

    python3.12 serve.py --port 877 --seed 7 --initial 400 --lying 40 --tick-secs 2

Endpoints — deliberately the awkward set a real system gives you:

    GET  /cases?since=<ts>&limit=<n>      the delta. Cannot report deletes.
    GET  /cases/all?page=<n>&size=<n>     the paged dump. The only delete oracle.
    GET  /cases/<id>                      one row
    GET  /contracts?case_id=1,2,3         the derived call (batched)
    GET  /contracts/all?page=<n>          paged dump
    GET  /probe                           "did table X change" — imperfect
    POST /cases/<id>  {"state": "..."}    writeback
    GET  /_cost                           what you have spent (not an API a
                                          real system gives you; the lab's
                                          instrument, readable so you can see
                                          your own bill)

Rate limiting is real: every response carries `X-RateLimit-Remaining`, and
over budget you get 429 with `Retry-After`. Read the limit from SUCCESSFUL
responses — discovering it via 429 is the mistake the guide names.

The world advances on a timer (`--tick-secs`), so the source changes under
you exactly like a live system. Nothing here is threaded: one request at a
time, which is enough to build against.
"""

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from system import System

STATE = {"sys": None, "limit": 60, "window": 60.0, "spent": [], "lock": threading.Lock()}


def charge():
    """Token-bucket over a sliding window. Returns what is left, or None if
    the caller is over the limit."""
    now = time.monotonic()
    STATE["spent"] = [t for t in STATE["spent"] if now - t < STATE["window"]]
    if len(STATE["spent"]) >= STATE["limit"]:
        return None
    STATE["spent"].append(now)
    return STATE["limit"] - len(STATE["spent"])


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_a):
        pass

    def send_json(self, code, body, remaining=None, retry=None):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        if remaining is not None:
            self.send_header("X-RateLimit-Limit", str(STATE["limit"]))
            self.send_header("X-RateLimit-Remaining", str(remaining))
        if retry is not None:
            self.send_header("Retry-After", str(retry))
        self.end_headers()
        self.wfile.write(raw)

    def route(self, path, q, body=None):
        s = STATE["sys"]
        parts = [p for p in path.split("/") if p]
        if path == "/probe":
            return s.probe()
        if path == "/_cost":
            return s.cost()
        if parts[:1] == ["cases"]:
            if len(parts) == 1:
                return s.cases_since(int(q.get("since", ["0"])[0]),
                                     int(q.get("limit", ["200"])[0]))
            if parts[1] == "all":
                return s.cases_page(int(q.get("page", ["0"])[0]),
                                    int(q.get("size", ["100"])[0]))
            if body is not None:
                return s.write_case(int(parts[1]), body.get("subject"), body.get("state"))
            return s.case(int(parts[1]))
        if parts[:1] == ["contracts"]:
            if len(parts) > 1 and parts[1] == "all":
                return s.contracts_page(int(q.get("page", ["0"])[0]),
                                        int(q.get("size", ["200"])[0]))
            ids = [int(x) for x in q.get("case_id", [""])[0].split(",") if x]
            if not ids:
                return []
            return s.contracts_for(ids)
        return None

    def handle_one(self, body=None):
        u = urlparse(self.path)
        with STATE["lock"]:
            left = charge()
            if left is None:
                return self.send_json(429, {"error": "rate limit"}, 0, retry=5)
            try:
                out = self.route(u.path, parse_qs(u.query), body)
            except (ValueError, KeyError, IndexError) as e:
                return self.send_json(400, {"error": str(e)}, left)
        if out is None:
            return self.send_json(404, {"error": "no such endpoint"}, left)
        self.send_json(200, out, left)

    def do_GET(self):
        self.handle_one()

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(n) if n else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            return self.send_json(400, {"error": "body must be JSON"}, STATE["limit"])
        self.handle_one(body)


def ticker(secs):
    while True:
        time.sleep(secs)
        with STATE["lock"]:
            STATE["sys"].tick()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=877)
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--initial", type=int, default=400)
    ap.add_argument("--lying", type=int, default=40)
    ap.add_argument("--update-pct", type=int, default=1)
    ap.add_argument("--delete-pct", type=int, default=1)
    ap.add_argument("--tick-secs", type=float, default=2.0)
    ap.add_argument("--limit", type=int, default=60, help="calls per window")
    ap.add_argument("--limit-window", type=float, default=60.0)
    a = ap.parse_args()

    STATE["sys"] = System(seed=a.seed, initial=a.initial, lying_pct=a.lying,
                          update_pct=a.update_pct, delete_pct=a.delete_pct)
    STATE["limit"], STATE["window"] = a.limit, a.limit_window
    threading.Thread(target=ticker, args=(a.tick_secs,), daemon=True).start()
    print(f"ingest-lab on http://127.0.0.1:{a.port}  "
          f"({a.limit} calls / {a.limit_window:.0f}s, world ticks every "
          f"{a.tick_secs}s, {a.lying}% of updates lie about updated_at)")
    ThreadingHTTPServer(("127.0.0.1", a.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
