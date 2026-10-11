"""Fixed guest stdout transport. No model output becomes a host operation."""
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


def relay(sock, proc, limits, deadline):
    partial = bytearray()
    output = bytearray()
    used = 0
    opened = {proc.stdout.fileno(), proc.stderr.fileno()}
    sock.setblocking(False)
    for fd in opened:
        os.set_blocking(fd, False)
    while opened or output:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RuntimeError('text invocation deadline exceeded')
        reads = list(opened) if not output else []
        readable, writable, _ = select.select(reads, [sock] if output else [], [], min(remaining, .05))
        for fd in readable:
            try:
                data = os.read(fd, min(8192, limits['max_frame_bytes'] + 1 - len(partial)))
            except BlockingIOError:
                continue
            used += len(data)
            if used > limits['max_output_bytes']:
                raise RuntimeError('native evidence byte limit exceeded')
            if not data:
                opened.remove(fd)
                if fd == proc.stdout.fileno() and partial:
                    raise RuntimeError('unterminated native frame')
                continue
            if fd == proc.stderr.fileno():
                # Diagnostics are bounded by the same guest budget. Host console
                # capture supplies a second bound; never merge stderr into JSON.
                os.write(2, data)
                continue
            partial.extend(data)
            while b'\n' in partial:
                end = partial.index(b'\n')
                if end > limits['max_frame_bytes']:
                    raise RuntimeError('native frame byte limit exceeded')
                # The bridge owns this prefix. Native bytes cannot forge E.
                output.extend(b'N' + partial[:end + 1])
                del partial[:end + 1]
            if len(partial) > limits['max_frame_bytes']:
                raise RuntimeError('native frame byte limit exceeded')
        if writable:
            try:
                sent = sock.send(output)
                if sent <= 0:
                    raise RuntimeError('transport made no progress')
                del output[:sent]
            except BlockingIOError:
                pass
    code = proc.wait(timeout=max(.001, min(2, deadline - time.monotonic())))
    sock.setblocking(True)
    sock.settimeout(max(.001, deadline - time.monotonic()))
    sock.sendall(('E' + str(code) + '\n').encode())


def main():
    with open(RUNTIME + '/launch.json', 'rb') as handle:
        limits = json.load(handle)
    deadline = time.monotonic() + limits['duration_ms'] / 1000
    env = {
        'HOME': '/home/pillbox', 'CLAUDE_CONFIG_DIR': '/home/pillbox/.claude',
        'PATH': '/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin', 'LANG': 'C.UTF-8',
        'SSL_CERT_FILE': RUNTIME + '/ca.crt', 'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
        'DISABLE_AUTOUPDATER': '1', 'DISABLE_TELEMETRY': '1',
        'DISABLE_ERROR_REPORTING': '1', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1',
    }
    for argv in (
        ['/usr/sbin/ip', 'link', 'set', 'eth0', 'up'],
        ['/usr/sbin/ip', 'addr', 'add', '10.0.2.15/24', 'dev', 'eth0'],
        ['/usr/sbin/ip', 'route', 'add', 'default', 'via', '10.0.2.2'],
    ):
        subprocess.run(argv, env=env, check=True, stdin=subprocess.DEVNULL,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=max(.001, min(5, deadline - time.monotonic())))
    with open('/etc/resolv.conf', 'w') as handle:
        handle.write('nameserver 10.0.2.2\n')
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    proc = None
    try:
        sock.settimeout(max(.001, min(10, deadline - time.monotonic())))
        sock.connect((2, 1067))
        with open(RUNTIME + '/input.txt', 'rb') as prompt:
            proc = subprocess.Popen(limits['argv'], cwd='/workspace', env=env, stdin=prompt,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    bufsize=0, close_fds=True, start_new_session=True)
        relay(sock, proc, limits, deadline)
    finally:
        sock.close()
        if proc is not None:
            stop(proc)


if __name__ == '__main__':
    main()
