# A private controlling terminal for the official client's passphrase command.
# Secrets arrive only on fd 10; terminal output is never forwarded or logged.
import os
import pty
import select
import signal
import sys
import termios
import time

secret = os.read(10, 4096).rstrip(b'\n')
pid, terminal = pty.fork()
if pid == 0:
    os.close(10)
    os.execvp(sys.argv[1], [sys.argv[1], 'key', 'change-passphrase', sys.argv[2], '--kdf', sys.argv[3]])
try:
    pending = b''
    deadline = time.monotonic() + 60
    answers = 0
    status = None
    while time.monotonic() < deadline:
        if select.select([terminal], [], [], .1)[0]:
            try:
                data = os.read(terminal, 4096)
            except OSError:
                break
            if not data:
                break
            pending = (pending + data)[-8192:]
            if pending.rstrip().endswith(b':') and (b'password' in pending.lower() or b'passphrase' in pending.lower()):
                answers += 1
                if answers > 3:
                    raise RuntimeError('unexpected repeated prompt')
                settings = termios.tcgetattr(terminal)
                settings[3] &= ~termios.ECHO
                termios.tcsetattr(terminal, termios.TCSANOW, settings)
                os.write(terminal, secret + b'\n')
                pending = b''
        got, result = os.waitpid(pid, os.WNOHANG)
        if got:
            status = result
            break
    # PTY EOF can precede waitpid reporting the child as exited. Reap with
    # the same deadline rather than treating that scheduler race as failure.
    while status is None and time.monotonic() < deadline:
        got, result = os.waitpid(pid, os.WNOHANG)
        if got:
            status = result
        else:
            time.sleep(.01)
    if status is None or os.waitstatus_to_exitcode(status) != 0:
        raise RuntimeError('passphrase operation failed or timed out')
except BaseException:
    try:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    except ProcessLookupError:
        pass
    except ChildProcessError:
        pass
    sys.exit(1)
finally:
    os.close(terminal)
