#!/usr/bin/env python3
"""Interactive agent workload: one HTTP request = one turn.

guestd (wrap mode) execs this as PID 1's child and infers turn boundaries from the
@@TURN_START@@ / @@TURN_END@@ markers it prints to stdout. A POST /ask runs aider
once (a turn, marked busy); between requests the guest is idle — that idle window
is when a migration is allowed to land. So you drive the turns by hand (curl) and a
migration behind the scenes only ever happens between your prompts, never mid-turn.

Env (delivered by the host over the Secrets vsock message — never baked in):
  AGENT_API_KEY   the free Groq key            AGENT_MODEL   model id (optional)
"""
import json
import os
import subprocess
from http.server import BaseHTTPRequestHandler, HTTPServer

os.environ.setdefault("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
os.environ.setdefault("HOME", "/root")
os.environ["GROQ_API_KEY"] = os.environ.get("AGENT_API_KEY", "")

REPO = os.environ.get("AGENT_REPO", "/root/task")
MODEL = os.environ.get("AGENT_MODEL", "groq/llama-3.3-70b-versatile")
PORT = int(os.environ.get("PORT", "8000"))
TURNS = 0


def run_aider(prompt):
    cmd = [
        "aider", "--model", MODEL,
        "--yes", "--no-auto-commits", "--no-stream", "--no-pretty",
        "--no-check-update", "--no-show-model-warnings", "--no-gitignore",
        "--message", prompt, "calc.py", "test_calc.py",
    ]
    p = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    return (p.stdout or "") + (p.stderr or "")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def _send(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # Liveness / status — never a turn, so it does not block a migration.
        self._send({"ok": True, "turns": TURNS})

    def do_POST(self):
        global TURNS
        n = int(self.headers.get("Content-Length", 0) or 0)
        prompt = self.rfile.read(n).decode("utf-8", "replace").strip()
        if not prompt:
            self._send({"error": "empty prompt"})
            return
        TURNS += 1
        # Marker -> guestd flips the shared turn state to "busy" for this request.
        print("@@TURN_START@@", flush=True)
        print(f"[agent] turn {TURNS}: {prompt}", flush=True)
        out = run_aider(prompt)
        print(out, flush=True)
        print("@@TURN_END@@", flush=True)
        self._send({"turn": TURNS, "reply": out[-4000:]})


if __name__ == "__main__":
    print(f"[agent] serving on :{PORT} (POST /ask = one turn)", flush=True)
    HTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
