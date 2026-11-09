# GET / answers; GET /out?u=<url> fetches <url> from inside the container and
# reports what came back, to test egress.
import http.server, urllib.parse, urllib.request


class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        if url.path == "/out":
            target = urllib.parse.parse_qs(url.query)["u"][0]
            try:
                with urllib.request.urlopen(target, timeout=10) as r:
                    body = f"{r.status} {r.read(200).decode(errors='replace')}"
            except Exception as e:  # report, do not fail the request
                body = f"error {type(e).__name__}: {e}"
        else:
            body = "hello from the container"
        data = body.encode()
        self.send_response(200)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


http.server.ThreadingHTTPServer(("0.0.0.0", 8080), H).serve_forever()
