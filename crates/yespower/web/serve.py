#!/usr/bin/env python3
"""Serve the benchmark page on all interfaces (open it from phones on the LAN).

    python3 web/serve.py [port]
"""
import http.server
import socket
import sys
from pathlib import Path

port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {**http.server.SimpleHTTPRequestHandler.extensions_map,
                      ".wasm": "application/wasm", ".js": "text/javascript"}

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(Path(__file__).parent), **kwargs)

    def end_headers(self):
        self.send_header("Cache-Control", "no-store")
        # Cross-origin isolation, so workers may share one WASM memory (pkg-shared-*).
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()


try:
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.connect(("192.0.2.1", 9))  # no packets sent; picks the LAN interface
    lan = s.getsockname()[0]
except OSError:
    lan = "127.0.0.1"
print(f"http://127.0.0.1:{port}/   (LAN: http://{lan}:{port}/)")
http.server.ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
