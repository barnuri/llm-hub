#!/usr/bin/env python3
"""A local upstream that answers every POST with a short chat-completions stream, and GET with
one model. llm-hub's overhead is measured against it, so the network and the model add nothing.
"""

import http.server
import json
import sys

CHUNKS = [
    b'data: {"choices":[{"delta":{"role":"assistant","content":"hi"}}]}\n\n',
    b"data: [DONE]\n\n",
]


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        body = json.dumps({"data": [{"id": "m", "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length", 0)))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        for chunk in CHUNKS:
            self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            self.wfile.flush()
        self.wfile.write(b"0\r\n\r\n")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18901
    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
