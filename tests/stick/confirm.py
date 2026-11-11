#!/usr/bin/env python3
"""Confirm only a CLI-owned disposable loop; never confirm a physical disk."""
import os
import re
import select
import subprocess
import sys
import time

child = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT)
output = b""
confirmed = False
erase_confirmed = False
deadline = time.monotonic() + 600
try:
    while child.poll() is None:
        if time.monotonic() > deadline:
            raise RuntimeError("create timed out")
        readable, _, _ = select.select([child.stdout], [], [], 1)
        if not readable:
            continue
        chunk = os.read(child.stdout.fileno(), 65536)
        if not chunk:
            break
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        output += chunk
        if b"Type the disk name to continue" in output and not confirmed:
            matches = re.findall(rb"(/dev/loop[0-9]+)", output)
            if not matches:
                raise RuntimeError("refusing to confirm anything except an owned loop")
            name = matches[-1].rsplit(b"/", 1)[-1]
            child.stdin.write(name + b"\n")
            child.stdin.flush()
            confirmed = True
        if (confirmed and not erase_confirmed and
                b"Disk is not empty; also type ERASE followed by the disk name" in output):
            child.stdin.write(b"ERASE " + name + b"\n")
            child.stdin.flush()
            erase_confirmed = True
    remaining = child.stdout.read()
    sys.stdout.buffer.write(remaining)
    sys.exit(child.wait())
finally:
    if child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()
