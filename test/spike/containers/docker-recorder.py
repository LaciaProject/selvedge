#!/usr/bin/env python3
"""Records the Docker Engine API calls made through it.

Usage: docker-recorder.py <listen socket> <docker socket> <log file>

A byte-for-byte relay between a Unix socket and Docker's. In the client's
direction it spots request lines and logs one JSON line per request: method,
path (query values dropped), and, for JSON bodies, the top-level keys and the
HostConfig keys. That is the contract a Docker API shim must honour.
"""
import json, os, re, socket, sys, threading
from urllib.parse import urlsplit, parse_qsl, urlencode

LISTEN, UPSTREAM, LOG = sys.argv[1:4]
REQUEST = re.compile(rb"(GET|POST|PUT|DELETE|HEAD) (\S+) HTTP/1\.[01]\r\n")
lock = threading.Lock()


def shape(path):
    """The path with ids and names replaced, and query values dropped."""
    u = urlsplit(path.decode(errors="replace"))
    p = re.sub(r"^/v[0-9.]+", "", u.path)
    p = re.sub(r"/(containers|exec|images|volumes)/[^/]+", r"/\1/{id}", p)
    q = urlencode([(k, "") for k, _ in parse_qsl(u.query)])
    return p + (f"?{q}" if q else "")


def body_keys(buf, start):
    head_end = buf.find(b"\r\n\r\n", start)
    if head_end < 0:
        return None
    body = buf[head_end + 4 : head_end + 4 + 65536]
    try:
        doc = json.loads(body[: body.rfind(b"}") + 1])
    except Exception:
        return None
    if not isinstance(doc, dict):
        return None
    keys = {"keys": sorted(doc)}
    if isinstance(doc.get("HostConfig"), dict):
        keys["HostConfig"] = sorted(doc["HostConfig"])
    return keys


def log(buf, m):
    entry = {"method": m.group(1).decode(), "path": shape(m.group(2))}
    keys = body_keys(buf, m.end())
    if keys:
        entry.update(keys)
    with lock, open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


def pump(src, dst, record):
    """Relays src to dst. When recording, a request is logged once the next
    one starts or the connection ends, so its body has arrived."""
    buf = b""
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
            if record:
                buf += data
                found = list(REQUEST.finditer(buf))
                for m in found[:-1]:
                    log(buf, m)
                if found:
                    buf = buf[found[-1].start() :]
                buf = buf[:1 << 20]
    except OSError:
        pass
    finally:
        if record:
            for m in REQUEST.finditer(buf):
                log(buf, m)
        for s in (src, dst):
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def serve(client):
    upstream = socket.socket(socket.AF_UNIX)
    upstream.connect(UPSTREAM)
    threading.Thread(target=pump, args=(client, upstream, True), daemon=True).start()
    threading.Thread(target=pump, args=(upstream, client, False), daemon=True).start()


if os.path.exists(LISTEN):
    os.unlink(LISTEN)
srv = socket.socket(socket.AF_UNIX)
srv.bind(LISTEN)
srv.listen(64)
while True:
    conn, _ = srv.accept()
    serve(conn)
