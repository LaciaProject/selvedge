#!/usr/bin/env python3
"""Checks a history from workload.py for linearizability, per object.

An object's log is append-only and every answer is the whole log, so a
history is linearizable exactly when one final log explains every answer:

  1. the final log (read after the faults, twice, the same both times) holds
     no token twice and no token nobody appended;
  2. every answer is a prefix of the final log: nothing acknowledged was lost
     or reordered, and no two owners ever wrote diverging histories;
  3. an acknowledged append is in its own answer;
  4. an operation that started after an append was acknowledged sees it;
  5. an answer holds no append that started after the answer arrived.

Appends that got no answer are indeterminate: they may be in the log or not.
Prints a summary per phase, including the longest stretch in which one object
gave no successful answer, and exits 1 on any violation.
"""
import argparse
import bisect
import collections
import http.client
import json
import sys
import time


def read_log(addr, host, key, timeout=15):
    h, p = addr.rsplit(":", 1)
    deadline = time.monotonic() + 120
    while True:
        try:
            conn = http.client.HTTPConnection(h, int(p), timeout=timeout)
            conn.request("GET", f"/?key={key}", headers={"Host": host})
            resp = conn.getresponse()
            body = resp.read().decode()
            conn.close()
            if resp.status == 200:
                return body.split("\n") if body else []
            err = f"{resp.status} {body[:200]}"
        except Exception as e:  # noqa: BLE001
            err = str(e)
        if time.monotonic() > deadline:
            raise SystemExit(f"FAIL final read of {key}: {err}")
        time.sleep(1)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("history")
    p.add_argument("--addr", required=True)
    p.add_argument("--host", required=True)
    a = p.parse_args()

    ops = [json.loads(line) for line in open(a.history)]
    by_key = collections.defaultdict(list)
    for op in ops:
        by_key[op["key"]].append(op)

    # Final state, read twice: it must have settled.
    final = {k: read_log(a.addr, a.host, k) for k in sorted(by_key)}
    time.sleep(5)
    for k in sorted(by_key):
        again = read_log(a.addr, a.host, k)
        if again != final[k]:
            print(f"FAIL {k}: the final log changed between two reads ({len(final[k])} -> {len(again)})")
            sys.exit(1)

    violations = []

    def bad(key, msg):
        violations.append(f"{key}: {msg}")

    for key, kops in by_key.items():
        log = final[key]
        index = {}
        for i, t in enumerate(log):
            if t in index:
                bad(key, f"token {t} is in the final log twice (at {index[t]} and {i})")
            index.setdefault(t, i)
        started = {op["token"]: op["start"] for op in kops if op["op"] == "append"}
        for t in log:
            if t not in started:
                bad(key, f"token {t} in the final log was never appended")
        # The latest start of any append in the first i+1 entries.
        latest_start = []
        m = 0
        for t in log:
            m = max(m, started.get(t, 0))
            latest_start.append(m)

        done = [op for op in kops if op["ok"]]
        for op in done:
            ans = op["log"]
            if ans != log[: len(ans)]:
                # Find where it diverges, for the report.
                i = next((i for i, (x, y) in enumerate(zip(ans, log)) if x != y), min(len(ans), len(log)))
                bad(key, f"{op['op']} {op['token'] or ''} answered a log that is not a prefix of the final one "
                         f"(length {len(ans)}, final {len(log)}, first difference at {i}: "
                         f"{ans[i] if i < len(ans) else '-'} vs {log[i] if i < len(log) else '-'})")
                continue
            if op["op"] == "append" and op["token"] not in ans:
                bad(key, f"append {op['token']} is missing from its own answer")
            if ans and latest_start[len(ans) - 1] > op["end"]:
                bad(key, f"{op['op']} {op['token'] or ''} saw an append that started after it ended")

        # Real time: each operation must see every append acknowledged before
        # it started.
        acked = sorted((op["end"], index.get(op["token"], -1)) for op in done if op["op"] == "append")
        ends = [e for e, _ in acked]
        need = []
        m = -1
        for _, i in acked:
            m = max(m, i)
            need.append(m)
        for op in done:
            j = bisect.bisect_left(ends, op["start"])
            if j and len(op["log"]) <= need[j - 1] and op["log"] == log[: len(op["log"])]:
                bad(key, f"{op['op']} {op['token'] or ''} missed an append acknowledged before it started "
                         f"(saw {len(op['log'])} entries, needed {need[j - 1] + 1})")

    # Per phase: counts, and the longest stretch an object went without a
    # successful answer.
    phases = collections.OrderedDict()
    for op in sorted(ops, key=lambda o: o["start"]):
        phases.setdefault(op["phase"], []).append(op)
    in_final = {t for log in final.values() for t in log}
    print(f"{'phase':<18}{'ops':>7}{'ok':>7}{'failed':>8}{'failed appends kept':>21}{'longest outage':>16}")
    for name, pops in phases.items():
        ok = [o for o in pops if o["ok"]]
        failed = [o for o in pops if not o["ok"]]
        kept = sum(1 for o in failed if o["op"] == "append" and o["token"] in in_final)
        fa = sum(1 for o in failed if o["op"] == "append")
        first, last = pops[0]["start"], max(o["end"] for o in pops)
        gap = 0
        for key in {o["key"] for o in pops}:
            points = [first] + sorted(o["end"] for o in ok if o["key"] == key) + [last]
            gap = max(gap, max(b - a for a, b in zip(points, points[1:])))
        gap /= 1e9
        print(f"{name:<18}{len(pops):>7}{len(ok):>7}{len(failed):>8}{f'{kept}/{fa}':>21}{gap:>15.1f}s")
        errors = collections.Counter(o["error"].split(":")[0][:60] for o in failed)
        for e, n in errors.most_common(4):
            print(f"{'':<18}  {n} x {e}")
    print(f"{len(by_key)} objects, {sum(len(v) for v in final.values())} entries in their final logs")

    if violations:
        print(f"FAIL {len(violations)} consistency violations:")
        for v in violations[:30]:
            print("  " + v)
        sys.exit(1)
    print("ok   the history is linearizable: no acknowledged write lost, one order per object")


if __name__ == "__main__":
    main()
