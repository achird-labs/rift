#!/usr/bin/env python3
"""Serve the Mountebank docs snapshot docs-snapshot.sh saved, for mbTest's docs tester (#1547).

usage: snapserve.py <snapshot-dir> <port>

GET /docs/api/mocks answers <snapshot-dir>/docs/api/mocks.html as text/html; anything else is 404.
Stdlib only; binds 127.0.0.1.
"""
import http.server
import os
import sys


def main() -> None:
    root = os.path.realpath(sys.argv[1])
    port = int(sys.argv[2])

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:  # noqa: N802 (http.server's naming)
            page = self.path.split("?", 1)[0].lstrip("/")
            target = os.path.realpath(os.path.join(root, page + ".html"))
            if not target.startswith(root + os.sep) or not os.path.isfile(target):
                self.send_error(404)
                return
            with open(target, "rb") as f:
                body = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args) -> None:
            pass

    http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()


if __name__ == "__main__":
    main()
