#!/usr/bin/env python3
"""Sends one SigV4-signed S3 request and prints the status code.

Usage: s3req.py <endpoint> <access key> <secret> <method> <path> [query]
"""
import datetime, hashlib, hmac, sys, urllib.parse, urllib.request

endpoint, key, secret, method, path = sys.argv[1:6]
query = sys.argv[6] if len(sys.argv) > 6 else ""
host = urllib.parse.urlparse(endpoint).netloc
now = datetime.datetime.now(datetime.timezone.utc)
amz_date, date = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
pairs = sorted(urllib.parse.parse_qsl(query, keep_blank_values=True))
canonical_query = "&".join(f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}" for k, v in pairs)
headers = {"host": host, "x-amz-content-sha256": "UNSIGNED-PAYLOAD", "x-amz-date": amz_date}
signed = ";".join(headers)
canonical = "\n".join([method, urllib.parse.quote(path, safe="/-_.~"), canonical_query,
                       "".join(f"{k}:{v}\n" for k, v in headers.items()), signed, "UNSIGNED-PAYLOAD"])
scope = f"{date}/us-east-1/s3/aws4_request"
to_sign = f"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
k = ("AWS4" + secret).encode()
for part in (date, "us-east-1", "s3", "aws4_request"):
    k = hmac.new(k, part.encode(), hashlib.sha256).digest()
sig = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()
req = urllib.request.Request(f"{endpoint}{path}" + (f"?{canonical_query}" if query else ""), method=method)
req.add_header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
req.add_header("x-amz-date", amz_date)
req.add_header("authorization", f"AWS4-HMAC-SHA256 Credential={key}/{scope}, SignedHeaders={signed}, Signature={sig}")
try:
    print(urllib.request.urlopen(req).status)
except urllib.error.HTTPError as e:
    print(e.code)
