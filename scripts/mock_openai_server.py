#!/usr/bin/env python3
"""A minimal mock OpenAI-compatible server for exercising turnip end to end.

Serves GET /v1/models and POST /v1/chat/completions (SSE). The first
response of a conversation asks the model to run `bash(echo ...)`;
the second, seeing the tool result, answers in text.
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


def sse_chunk(delta=None, finish=None, usage=None):
    chunk = {"id": "chatcmpl-mock", "object": "chat.completion.chunk", "model": "mock-1"}
    if delta is not None or finish is not None:
        chunk["choices"] = [{"index": 0, "delta": delta or {}, "finish_reason": finish}]
    else:
        chunk["choices"] = []
    if usage is not None:
        chunk["usage"] = usage
    return f"data: {json.dumps(chunk)}\n\n"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _send_json(self, code, payload):
        body = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._send_json(200, {"data": [{"id": "mock-1", "description": "Mock model"}]})
        else:
            self._send_json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        if not self.path.startswith("/v1/chat/completions"):
            self._send_json(404, {"error": {"message": "not found"}})
            return
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        saw_tool_result = any(m.get("role") == "tool" for m in body.get("messages", []))

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def emit(line):
            self.wfile.write(line.encode())
            self.wfile.flush()

        usage = {
            "prompt_tokens": 111,
            "completion_tokens": 7,
            "total_tokens": 118,
            "prompt_tokens_details": {"cached_tokens": 20},
        }
        if not saw_tool_result:
            # Ask for a bash tool call, streamed in pieces.
            emit(sse_chunk({"tool_calls": [{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": "bash", "arguments": ""},
            }]}))
            emit(sse_chunk({"tool_calls": [{
                "index": 0,
                "function": {"arguments": "{\"command\": \"echo hello-from-tool\"}"},
            }]}))
            emit(sse_chunk(delta={}, finish="tool_calls"))
            emit(sse_chunk(usage=usage))
            emit("data: [DONE]\n\n")
        else:
            for text in ["The tool said: ", "hello-from-tool", ". Done."]:
                emit(sse_chunk({"content": text}))
            emit(sse_chunk(delta={}, finish="stop"))
            emit(sse_chunk(usage=usage))
            emit("data: [DONE]\n\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    server = HTTPServer(("127.0.0.1", port), Handler)
    print(f"mock server on http://127.0.0.1:{port}/v1", file=sys.stderr)
    server.serve_forever()
