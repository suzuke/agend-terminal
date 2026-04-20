#!/usr/bin/env python3
"""
Connects to an agend-terminal agent's TUI socket, dumps protocol handshake
+ initial vterm snapshot + live PTY frames. Used to diagnose whether the
PTY reader is actually receiving output from the child process.

Usage:  python tui_dump.py <agent_name> [duration_seconds=10]

Protocol (from src/framing.rs + src/auth_cookie.rs, Stage 8 onward):
  - Client sends 32 raw cookie bytes (from {run_dir}/api.cookie)
  - Server sends 1 byte PROTOCOL_VERSION
  - Then frames: [u8 tag][u32 BE length][bytes]
    tag 0 = PTY data, tag 1 = resize (4 bytes cols+rows)
  - First frame after handshake is the vterm dump (prior buffered output)
"""
import os
import socket
import struct
import sys
import time
from pathlib import Path

HOME = Path(os.environ.get("USERPROFILE") or os.path.expanduser("~"))
RUN_DIR = HOME / ".agend" / "run"


def read_exact(sock, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return bytes(buf) if buf else None
        buf.extend(chunk)
    return bytes(buf)


def find_agent_port(agent):
    # Run dir contains one subdir per daemon PID
    for pid_dir in RUN_DIR.glob("*"):
        port_file = pid_dir / f"{agent}.port"
        if port_file.exists():
            return int(port_file.read_text().strip()), pid_dir
    return None, None


def read_cookie(run_dir):
    cookie = (run_dir / "api.cookie").read_bytes()
    if len(cookie) != 32:
        raise SystemExit(f"unexpected cookie length {len(cookie)}")
    return cookie


def describe(data):
    s = data.decode("utf-8", errors="replace")
    printable = s.replace("\r", "\\r").replace("\n", "\\n").replace("\x1b", "\\x1b")
    return printable[:200] + ("...(truncated)" if len(printable) > 200 else "")


def main():
    agent = sys.argv[1] if len(sys.argv) > 1 else "sh"
    duration = float(sys.argv[2]) if len(sys.argv) > 2 else 10.0

    port, run_dir = find_agent_port(agent)
    if port is None:
        print(f"agent {agent!r} port file not found under {RUN_DIR}", file=sys.stderr)
        sys.exit(2)
    print(f"[tui_dump] connecting to agent={agent!r} port={port}")

    sock = socket.create_connection(("127.0.0.1", port), timeout=5.0)
    sock.settimeout(duration)

    # Stage 8 auth: send 32-byte cookie before anything else
    cookie = read_cookie(run_dir)
    sock.sendall(cookie)

    # Read protocol version
    version = read_exact(sock, 1)
    if version is None:
        print("[tui_dump] connection closed before version byte", file=sys.stderr)
        sys.exit(3)
    print(f"[tui_dump] protocol version = {version[0]}")

    deadline = time.monotonic() + duration
    total_data = 0
    frame_idx = 0

    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            print(f"[tui_dump] time budget exhausted ({duration}s)")
            break
        try:
            sock.settimeout(max(remaining, 0.1))
            tag_buf = read_exact(sock, 1)
            if tag_buf is None:
                print("[tui_dump] EOF from server")
                break
            len_buf = read_exact(sock, 4)
            if len_buf is None:
                print("[tui_dump] EOF while reading length")
                break
            (length,) = struct.unpack(">I", len_buf)
            data = read_exact(sock, length) if length > 0 else b""
            if length > 0 and data is None:
                print(f"[tui_dump] EOF while reading {length}-byte payload")
                break

            tag = tag_buf[0]
            tag_name = {0: "DATA", 1: "RESIZE"}.get(tag, f"?({tag})")
            label = "DUMP" if frame_idx == 0 and tag == 0 else tag_name
            print(f"[tui_dump] frame#{frame_idx} {label} len={length}")
            if length > 0 and tag == 0:
                print(f"              bytes: {describe(data)}")
                total_data += length
            frame_idx += 1
        except socket.timeout:
            print(f"[tui_dump] read timeout (no frame for a while)")
            break

    print(f"[tui_dump] done. frames={frame_idx} total_data_bytes={total_data}")
    sock.close()


if __name__ == "__main__":
    main()
