#!/usr/bin/env python3
"""Send a real CTRL_C_EVENT to agend-terminal daemon in a new console,
using AGEND_CTRLC_SENTINEL file to observe whether the ctrlc handler
fired (since daemon stdout in a new console isn't captured).
"""
import ctypes, os, subprocess, sys, time
from pathlib import Path

BIN = r"C:\Users\suzuke\Downloads\agend-terminal\agend-terminal.exe"
SENTINEL = Path(r"C:\Users\suzuke\AppData\Local\Temp\agend-ctrlc-fired.txt")
if SENTINEL.exists():
    SENTINEL.unlink()

CREATE_NEW_CONSOLE = 0x00000010
CTRL_C_EVENT = 0
ATTACH_PARENT_PROCESS = -1

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
for n in ("AttachConsole", "FreeConsole", "GenerateConsoleCtrlEvent", "SetConsoleCtrlHandler"):
    fn = getattr(k32, n)
    fn.restype = ctypes.c_int

env = os.environ.copy()
env["AGEND_CTRLC_SENTINEL"] = str(SENTINEL)

print(f"[test] spawning daemon (new console), sentinel={SENTINEL}")
proc = subprocess.Popen(
    [BIN, "daemon", "sh:cmd"],
    creationflags=CREATE_NEW_CONSOLE,
    env=env,
)
print(f"[test] pid={proc.pid}")
time.sleep(4)

run_dir = Path(os.environ["USERPROFILE"]) / ".agend" / "run"
print(f"[test] run dir: {list(run_dir.glob('*'))}")
print(f"[test] sentinel before signal: exists={SENTINEL.exists()}")

print("[test] FreeConsole + AttachConsole(daemon)")
k32.FreeConsole()
if not k32.AttachConsole(proc.pid):
    print(f"[test] AttachConsole FAILED err={ctypes.get_last_error()}")
    sys.exit(2)
k32.SetConsoleCtrlHandler(None, 1)

t0 = time.monotonic()
print("[test] GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)")
ok = k32.GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)
print(f"[test] sent: ok={ok}")

# Detach and poll
k32.FreeConsole()
k32.AttachConsole(ATTACH_PARENT_PROCESS)

deadline = t0 + 12.0
saw_sentinel = False
while time.monotonic() < deadline:
    if not saw_sentinel and SENTINEL.exists():
        saw_sentinel = True
        dt = time.monotonic() - t0
        print(f"[test] SENTINEL fired after {dt:.2f}s: {SENTINEL.read_text().strip()}")
    if proc.poll() is not None:
        dt = time.monotonic() - t0
        print(f"[test] daemon exited after {dt:.2f}s, code={proc.returncode}")
        break
    time.sleep(0.2)
else:
    dt = time.monotonic() - t0
    print(f"[test] STILL RUNNING after {dt:.1f}s, sentinel={SENTINEL.exists()}")
    print("[test] force-killing")
    proc.terminate()
    time.sleep(0.5)
    if proc.poll() is None:
        proc.kill()

print(f"[test] run dir after: {list(run_dir.glob('*'))}")
print(f"[test] sentinel final: exists={SENTINEL.exists()}")
