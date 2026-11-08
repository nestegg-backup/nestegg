#!/usr/bin/env python3
"""Drive a VM's serial console (QEMU unix socket chardev).

Usage: console.py SOCKET LOGFILE SCRIPT
SCRIPT is a file with one step per line:
  expect <regex> [timeout seconds]   wait until the output matches
  send <text>                        type text followed by Enter
  sendfile <path>                    type the content of a file plus Enter
                                     (for secrets: never written to the log)
Everything received is appended to LOGFILE.
"""
import re
import socket
import sys
import time


def main() -> int:
    sock_path, log_path, script_path = sys.argv[1:4]
    deadline = time.time() + 60
    while True:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.connect(sock_path)
            break
        except OSError:
            if time.time() > deadline:
                print(f"cannot connect to {sock_path}", file=sys.stderr)
                return 2
            time.sleep(0.5)
    s.settimeout(1.0)
    buf = ""
    log = open(log_path, "a", encoding="utf-8", errors="replace")

    def pump() -> None:
        nonlocal buf
        try:
            data = s.recv(65536)
        except socket.timeout:
            return
        if not data:
            raise EOFError("console closed")
        text = data.decode("utf-8", errors="replace")
        log.write(text)
        log.flush()
        buf += text

    for raw in open(script_path, encoding="utf-8"):
        line = raw.rstrip("\n")
        if not line or line.startswith("#"):
            continue
        cmd, _, arg = line.partition(" ")
        if cmd == "expect":
            parts = arg.rsplit(" ", 1)
            timeout = 600
            if len(parts) == 2 and parts[1].isdigit():
                arg, timeout = parts[0], int(parts[1])
            pattern = re.compile(arg)
            end = time.time() + timeout
            while not pattern.search(buf):
                if time.time() > end:
                    print(f"timeout waiting for: {arg}", file=sys.stderr)
                    return 1
                try:
                    pump()
                except EOFError:
                    print(f"console closed while waiting for: {arg}", file=sys.stderr)
                    return 1
            # Consume up to the match so the next expect needs new output.
            m = pattern.search(buf)
            buf = buf[m.end():]
        elif cmd == "send":
            s.sendall((arg + "\r").encode())
        elif cmd == "sendfile":
            with open(arg, encoding="utf-8") as f:
                s.sendall((f.read().strip("\n") + "\r").encode())
        else:
            print(f"unknown step: {line}", file=sys.stderr)
            return 2
    # Keep logging until the VM closes the console.
    try:
        while True:
            pump()
    except EOFError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
