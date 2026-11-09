#!/usr/bin/env python3
"""Prints a Markdown table comparing two bench.sh result files.

Usage: test/bench/compare.py <base.jsonl> <head.jsonl>
"""
import json, sys


def load(path):
    try:
        with open(path) as f:
            return {r["label"]: r for r in map(json.loads, filter(str.strip, f))}
    except FileNotFoundError:
        return {}


base, head = load(sys.argv[1]), load(sys.argv[2])
print("| Scenario | Base req/s | Head req/s | Change | Base p50 ms | Head p50 ms | Base p99 ms | Head p99 ms | Head errors |")
print("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
for label, h in head.items():
    b = base.get(label)
    change = f"{(h['rps'] / b['rps'] - 1) * 100:+.0f}%" if b and b["rps"] else "n/a"
    cell = lambda r, k: f"{r[k]:g}" if r else "n/a"
    print(f"| {label} | {cell(b, 'rps')} | {h['rps']:g} | {change} | {cell(b, 'p50_ms')} | {h['p50_ms']:g} "
          f"| {cell(b, 'p99_ms')} | {h['p99_ms']:g} | {h['errors'] + h['non_2xx']} |")
