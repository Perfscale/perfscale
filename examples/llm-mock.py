#!/usr/bin/env python3
"""Mock LLM server for building std/llm@v1 tests without a GPU or an API key.

Speaks both wire formats perfscale drives, streaming and non-streaming:
  POST /v1/chat/completions   OpenAI-compatible (Ollama, vLLM, LM Studio, ...)
  POST /v1/messages           Anthropic messages API

Latency is shaped like a real model: TTFT_MS before the first token, then
TOK_S tokens per second. Token usage is reported, so llm_tokens_per_sec and
the llm_*_tokens counters populate. FAIL_PCT injects 503s to exercise
thresholds and error hints.

  python3 examples/llm-mock.py                    # :8088, ttft 150ms, 40 tok/s
  TTFT_MS=400 TOK_S=15 FAIL_PCT=5 python3 examples/llm-mock.py --port 9000

Standard library only.
"""

import argparse
import json
import os
import random
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TTFT_MS = float(os.environ.get("TTFT_MS", "150"))
TOK_S = float(os.environ.get("TOK_S", "40"))
FAIL_PCT = float(os.environ.get("FAIL_PCT", "0"))
WORDS = ("load testing measures how a system behaves when many users "
         "send requests at the same time and every token counts").split()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):  # keep the terminal quiet under load
        pass

    def _json(self, code, body, headers=()):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def _event(self, payload, event=None):
        line = (f"event: {event}\n" if event else "") + f"data: {payload}\n\n"
        data = line.encode()
        self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
        self.wfile.flush()

    def do_GET(self):
        if self.path.rstrip("/") in ("/v1/models", "/health"):
            return self._json(200, {"object": "list", "data": [{"id": "mock"}]})
        self._json(404, {"error": f"no route {self.path}"})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        path = self.path.split("?")[0].rstrip("/")
        if path not in ("/v1/chat/completions", "/v1/messages"):
            return self._json(404, {"error": f"no route {self.path} — use /v1/chat/completions or /v1/messages"})
        if random.uniform(0, 100) < FAIL_PCT:
            return self._json(503, {"error": "mock overloaded"}, [("retry-after", "1")])

        anthropic = path == "/v1/messages"
        prompt = " ".join(str(m.get("content", "")) for m in body.get("messages", []))
        prompt_tokens = max(1, len(prompt.split()))
        n = max(1, min(int(body.get("max_tokens", 64)), 64))
        tokens = [WORDS[i % len(WORDS)] + " " for i in range(n)]
        model = body.get("model", "mock")

        time.sleep(TTFT_MS / 1000)
        if not body.get("stream", False):
            time.sleep(n / TOK_S)
            text = "".join(tokens)
            if anthropic:
                return self._json(200, {"type": "message", "content": [{"type": "text", "text": text}],
                                        "usage": {"input_tokens": prompt_tokens, "output_tokens": n}})
            return self._json(200, {"model": model, "choices": [{"message": {"role": "assistant", "content": text}}],
                                    "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": n}})

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        if anthropic:
            self._event(json.dumps({"type": "message_start",
                                    "message": {"usage": {"input_tokens": prompt_tokens, "output_tokens": 0}}}),
                        "message_start")
        for i, tok in enumerate(tokens):
            if i:
                time.sleep(1 / TOK_S)
            if anthropic:
                self._event(json.dumps({"type": "content_block_delta", "index": 0,
                                        "delta": {"type": "text_delta", "text": tok}}), "content_block_delta")
            else:
                self._event(json.dumps({"choices": [{"delta": {"content": tok}}]}))
        if anthropic:
            self._event(json.dumps({"type": "message_delta", "usage": {"output_tokens": n}}), "message_delta")
            self._event(json.dumps({"type": "message_stop"}), "message_stop")
        else:
            self._event(json.dumps({"choices": [],
                                    "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": n}}))
            self._event("[DONE]")
        self.wfile.write(b"0\r\n\r\n")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=int(os.environ.get("PORT", "8088")))
    port = ap.parse_args().port
    print(f"mock LLM on http://127.0.0.1:{port} (ttft {TTFT_MS:.0f}ms, {TOK_S:.0f} tok/s, fail {FAIL_PCT:.0f}%)",
          flush=True)
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
