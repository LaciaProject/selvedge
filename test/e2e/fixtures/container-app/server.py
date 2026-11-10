# The e2e container: answers on :8080, makes outbound requests on demand and
# exits when told to.
import http.server, os, ssl, sys, threading, urllib.parse, urllib.request

CA = "/etc/cloudflare/certs/cloudflare-containers-ca.crt"


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def answer(self, status, text):
        body = text.encode()
        self.send_response(status)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(url.query)
        if url.path == "/":
            self.answer(200, f"hello {os.environ.get('MESSAGE', 'nobody')} v__VERSION__")
        elif url.path == "/out":
            try:
                context = ssl.create_default_context()
                if os.path.exists(CA):
                    context.load_verify_locations(CA)
                with urllib.request.urlopen(query["u"][0], timeout=10, context=context) as r:
                    self.answer(200, f"{r.status} {r.read(200).decode(errors='replace')}")
            except Exception as e:
                self.answer(200, f"failed {type(e).__name__}")
        elif url.path == "/ca":
            self.answer(200, "present" if os.path.exists(CA) and "BEGIN CERTIFICATE" in open(CA).read() else "absent")
        elif url.path == "/args":
            self.answer(200, " ".join(sys.argv[1:]))
        elif url.path == "/exit":
            self.answer(200, "bye")
            threading.Timer(0.2, lambda: os._exit(int(query.get("code", ["0"])[0]))).start()
        else:
            self.answer(404, "not found")


http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
