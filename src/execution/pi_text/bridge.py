"""Owned one-shot Pi SDK relay. stdout JSONL, stderr and input are bounded."""
import json
import os
import select
import signal
import socket
import subprocess
import time

RUNTIME = '/opt/pillbox-execution'


def stop(proc):
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    proc.wait(timeout=2)


def relay(sock, proc, limits, deadline, request):
    # Request is host-generated, never model output or discovered config.
    pending_input = bytearray(json.dumps(request, ensure_ascii=False, separators=(',', ':')).encode()) if request else bytearray()
    partial = bytearray()
    pending_output = bytearray()
    used = 0
    input_open = proc.stdin is not None
    output_open = True
    error_open = True
    sock.setblocking(False)
    for pipe in (proc.stdin, proc.stdout, proc.stderr):
        if pipe is not None:
            os.set_blocking(pipe.fileno(), False)
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('Pi deadline exceeded')
        if input_open and not pending_input:
            proc.stdin.close()
            input_open = False
        if not output_open and not error_open and not pending_output:
            code = proc.wait(timeout=min(remaining, 2))
            terminal = (json.dumps({'type': 'pillbox_pi.exit', 'code': code}) + '\n').encode()
            if used + len(terminal) > limits['max_output_bytes']:
                raise RuntimeError('Pi evidence limit')
            sock.settimeout(min(remaining, 2))
            sock.sendall(terminal)
            sock.shutdown(socket.SHUT_WR)
            return
        reads = [sock]
        capacity = limits['max_frame_bytes'] + 1 - len(partial) - len(pending_output)
        if output_open and capacity > 0:
            reads.append(proc.stdout)
        if error_open:
            reads.append(proc.stderr)
        writes = [sock] if pending_output else []
        if input_open:
            writes.append(proc.stdin)
        readable, writable, _ = select.select(reads, writes, [], min(remaining, 0.1))
        for source in readable:
            if source is sock:
                if not sock.recv(1):
                    raise RuntimeError('Pi host disconnected')
                raise RuntimeError('Unexpected Pi host input')
            budget = limits['max_output_bytes'] - used
            size = min(65536, budget + 1, capacity) if source is proc.stdout else min(65536, budget + 1)
            try:
                data = os.read(source.fileno(), size)
            except BlockingIOError:
                continue
            used += len(data)
            if used > limits['max_output_bytes']:
                raise RuntimeError('Pi evidence limit')
            if source is proc.stderr:
                if not data:
                    error_open = False
                else:
                    # The VMM supervisor bounds this diagnostic stream too.
                    os.write(2, data)
                continue
            if not data:
                if partial:
                    raise RuntimeError('Unterminated Pi frame')
                output_open = False
                continue
            partial.extend(data)
            while True:
                end = partial.find(b'\n')
                if end < 0:
                    if len(partial) > limits['max_frame_bytes']:
                        raise RuntimeError('Pi frame limit')
                    break
                if end > limits['max_frame_bytes']:
                    raise RuntimeError('Pi frame limit')
                pending_output.extend(partial[:end + 1])
                del partial[:end + 1]
        for target in writable:
            queue = pending_output if target is sock else pending_input
            try:
                count = sock.send(queue) if target is sock else os.write(target.fileno(), queue)
                if count <= 0:
                    raise RuntimeError('Pi relay write made no progress')
                del queue[:count]
            except BlockingIOError:
                pass


def main():
    with open(RUNTIME + '/limits.json', 'rb') as handle:
        limits = json.load(handle)
    with open(RUNTIME + '/mode.json', 'rb') as handle:
        mode = json.load(handle)
    deadline = time.monotonic() + limits['duration_ms'] / 1000
    env = {'HOME': '/home/pillbox', 'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin',
           'LANG': 'C.UTF-8', 'PI_OFFLINE': '1', 'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
           'SSL_CERT_FILE': RUNTIME + '/ca.crt'}
    if mode == 'turn':
        for argv in (['/usr/sbin/ip', 'link', 'set', 'eth0', 'up'],
                     ['/usr/sbin/ip', 'addr', 'add', '10.0.2.15/24', 'dev', 'eth0'],
                     ['/usr/sbin/ip', 'route', 'add', 'default', 'via', '10.0.2.2']):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError('Pi deadline exceeded')
            subprocess.run(argv, env=env, check=True, stdin=subprocess.DEVNULL,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=min(remaining, 5))
        with open('/etc/resolv.conf', 'w') as handle:
            handle.write('nameserver 10.0.2.2\n')
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    proc = None
    try:
        sock.settimeout(max(0.001, min(deadline - time.monotonic(), 10)))
        sock.connect((2, 1067))
        request = None
        if mode == 'turn':
            frame = bytearray()
            while True:
                sock.settimeout(max(0.001, min(deadline - time.monotonic(), 0.1)))
                if time.monotonic() >= deadline:
                    raise RuntimeError('Pi deadline exceeded')
                try:
                    byte = sock.recv(1)
                except socket.timeout:
                    continue
                if not byte:
                    raise RuntimeError('Pi request absent')
                if byte == b'\n':
                    break
                frame.extend(byte)
                if len(frame) > limits['max_frame_bytes']:
                    raise RuntimeError('Pi input frame limit')
            request = json.loads(frame)
        proc = subprocess.Popen(['/usr/bin/env', 'node', RUNTIME + '/driver.mjs', mode],
                                cwd='/workspace', env=env, stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0,
                                close_fds=True, start_new_session=True)
        relay(sock, proc, limits, deadline, request)
    finally:
        sock.close()
        if proc is not None:
            stop(proc)


if __name__ == '__main__':
    main()
