#!/usr/bin/env python3
"""Send raw bytes to an agend-terminal agent via its TUI socket.

Usage:  python tui_send.py <agent_name> <hex_bytes>
Example: python tui_send.py sh 03   # send a Ctrl+C (ETX) byte
"""
import os, socket, struct, sys, time
from pathlib import Path

HOME = Path(os.environ.get("USERPROFILE") or os.path.expanduser("~"))
RUN_DIR = HOME / ".agend" / "run"
TAG_DATA = 0


def read_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def find_port(agent):
    for pid_dir in RUN_DIR.glob("*"):
        p = pid_dir / f"{agent}.port"
        if p.exists():
            return int(p.read_text().strip())
    return None


def main():
    agent = sys.argv[1]
    hex_bytes = sys.argv[2].replace(" ", "")
    payload = bytes.fromhex(hex_bytes)
    port = find_port(agent)
    sock = socket.create_connection(("127.0.0.1", port), timeout=5.0)
    version = read_exact(sock, 1)
    print(f"[tui_send] connected port={port} version={version[0]}")
    # Skip initial dump frame
    tag = read_exact(sock, 1)
    ln = struct.unpack(">I", read_exact(sock, 4))[0]
    if ln:
        read_exact(sock, ln)
    print(f"[tui_send] skipped dump ({ln} bytes)")
    # Send framed payload
    frame = bytes([TAG_DATA]) + struct.pack(">I", len(payload)) + payload
    sock.sendall(frame)
    print(f"[tui_send] sent {len(payload)} bytes: {payload.hex()}")
    # Read response for 2s
    sock.settimeout(2.0)
    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline:
        try:
            tag = read_exact(sock, 1)
            if tag is None:
                break
            ln = struct.unpack(">I", read_exact(sock, 4))[0]
            data = read_exact(sock, ln) if ln else b""
            print(f"[tui_send] reply tag={tag[0]} {ln}B: {data[:120]!r}")
        except socket.timeout:
            break
    sock.close()


if __name__ == "__main__":
    main()
