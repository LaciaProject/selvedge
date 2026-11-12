#!/usr/bin/env python3
"""Load for the consistency check: concurrent appends and reads on a few
Durable Object logs (log.js) while a fault runs, recorded as a history.

Each operation is one line of JSON in --out:
  {"phase", "key", "op": "append"|"read", "token", "start", "end",
   "ok": true, "log": [...]}            the object answered with its log
  {..., "ok": false, "error": "..."}    no answer: the append may or may not
                                         have happened (indeterminate)
start and end are CLOCK_MONOTONIC nanoseconds, comparable across the runs of
this script on one host, so check.py can order operations in real time.

Runs until --seconds pass or the file named by --stop appears.
"""
import argparse
import http.client
import json
import os
import random
import threading
import time


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--addr", required=True, help="gateway host:port")
    p.add_argument("--host", required=True, help="the Worker's hostname")
    p.add_argument("--phase", required=True)
    p.add_argument("--keys", type=int, default=6)
    p.add_argument("--clients", type=int, default=8)
    p.add_argument("--seconds", type=float, default=600)
    p.add_argument("--stop", default="")
    p.add_argument("--pause", type=float, default=0.05, help="seconds between a client's operations")
    p.add_argument("--timeout", type=float, default=15)
    p.add_argument("--out", required=True)
    a = p.parse_args()

    host, port = a.addr.rsplit(":", 1)
    keys = [f"{a.phase}-{i}" for i in range(a.keys)]
    deadline = time.monotonic() + a.seconds
    lock = threading.Lock()
    out = open(a.out, "a")

    def done():
        return time.monotonic() >= deadline or (a.stop and os.path.exists(a.stop))

    def client(n):
        conn = None
        seq = 0
        while not done():
            key = random.choice(keys)
            # Mostly appends; the reads check that nothing acknowledged goes
            # missing between them.
            append = random.random() < 0.8
            token = f"{a.phase}.{n}.{seq}" if append else None
            seq += 1
            path = f"/?key={key}" + (f"&append={token}" if append else "")
            rec = {"phase": a.phase, "key": key, "op": "append" if append else "read", "token": token}
            rec["start"] = time.monotonic_ns()
            try:
                if conn is None:
                    conn = http.client.HTTPConnection(host, int(port), timeout=a.timeout)
                conn.request("GET", path, headers={"Host": a.host})
                resp = conn.getresponse()
                body = resp.read().decode()
                rec["end"] = time.monotonic_ns()
                if resp.status == 200:
                    rec["ok"] = True
                    rec["log"] = body.split("\n") if body else []
                else:
                    rec["ok"] = False
                    rec["error"] = f"{resp.status} {body[:200]}"
            except Exception as e:  # noqa: BLE001 - every failure is an outcome
                rec["end"] = time.monotonic_ns()
                rec["ok"] = False
                rec["error"] = f"{type(e).__name__}: {e}"[:200]
                if conn is not None:
                    conn.close()
                conn = None
            with lock:
                out.write(json.dumps(rec) + "\n")
            time.sleep(a.pause)

    threads = [threading.Thread(target=client, args=(n,)) for n in range(a.clients)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    out.close()


if __name__ == "__main__":
    main()
