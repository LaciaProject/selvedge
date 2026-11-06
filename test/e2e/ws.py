"""Sends one WebSocket text message through the gateway and prints the reply."""
import base64
import os
import socket
import sys

port, host, path, message = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4].encode()
s = socket.create_connection(("127.0.0.1", port), timeout=10)
key = base64.b64encode(os.urandom(16)).decode()
s.sendall(
    f"GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
)
head = b""
while b"\r\n\r\n" not in head:
    head += s.recv(1)
if not head.startswith(b"HTTP/1.1 101"):
    sys.exit(f"no upgrade: {head.splitlines()[0]!r}")
mask = os.urandom(4)
s.sendall(bytes([0x81, 0x80 | len(message)]) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(message)))
frame = s.recv(2)
print(s.recv(frame[1] & 0x7F).decode())
