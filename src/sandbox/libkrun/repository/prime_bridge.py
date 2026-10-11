"""Fixed Prime one-shot transport. Guest input is a prompt, never an RPC command."""
import json
import os
import select
import signal
import socket
import subprocess
import time

RUNTIME = '/opt/pillbox-prime-text-runtime'


def main():
    with open(RUNTIME + '/config.json', encoding='utf-8') as source:
        config = json.load(source)
    deadline = time.monotonic() + config['duration_ms'] / 1000
    sock = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    sock.settimeout(max(0.001, deadline - time.monotonic()))
    sock.connect((2, 1067))
    env = {
        'HOME': '/home/pillbox', 'PATH': '/usr/local/bin:/usr/bin:/bin',
        'PRIME_AGENT_CODING_AGENT_DIR': '/home/pillbox/.prime/agent',
        'DO_NOT_TRACK': '1', 'PI_OFFLINE': '1',
        # The VM supervisor owns this qualified native worker; its global
        # daemon must never participate in a sealed invocation.
        'PRIME_AGENT_INTERNAL_LEGACY_OWNED_WORKER_FRONTEND': '1',
        'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',
        'SSL_CERT_FILE': RUNTIME + '/ca.crt',
    }
    version = subprocess.run([config['argv'][0], '--version'], env=env,
                             capture_output=True, timeout=max(0.001, deadline - time.monotonic()),
                             check=True).stdout
    if len(version) > 128 or version.decode('utf-8').strip() != config['harness_version']:
        raise RuntimeError('Prime version differs from image profile')
    sock.sendall(json.dumps({'type': 'pillbox_prime_init',
                            'harness_version': version.decode().strip()}).encode() + b'\n')
    request = bytearray()
    while not request.endswith(b'\n'):
        sock.settimeout(max(0.001, deadline - time.monotonic()))
        chunk = sock.recv(1)
        if not chunk or len(request) > config['frame_bytes']:
            raise RuntimeError('Invalid Prime input frame')
        request.extend(chunk)
    request = json.loads(request)
    if set(request) != {'prompt'} or not isinstance(request['prompt'], str):
        raise RuntimeError('Invalid Prime input')
    # Native slash-command normalization must never interpret the input as a
    # session action. CLI attachment expansion is also fenced by this prefix.
    prompt = 'Respond in text to the following input:\n\n' + request['prompt']
    process = subprocess.Popen(config['argv'], env=env,
                               cwd='/workspace', stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               start_new_session=True)
    used = 0
    partial = bytearray()
    pipes = {process.stdout, process.stderr}
    pending = memoryview(prompt.encode())
    os.set_blocking(process.stdin.fileno(), False)
    try:
        for pipe in pipes:
            os.set_blocking(pipe.fileno(), False)
        while pipes or pending:
            if time.monotonic() >= deadline:
                raise RuntimeError('Prime deadline exceeded')
            writers = [process.stdin] if pending else []
            ready, writable, _ = select.select(list(pipes), writers, [], 0.05)
            if writable:
                written = os.write(process.stdin.fileno(), pending)
                pending = pending[written:]
                if not pending:
                    process.stdin.close()
            for pipe in ready:
                chunk = os.read(pipe.fileno(), 8192)
                if not chunk:
                    pipes.remove(pipe)
                    continue
                used += len(chunk)
                if used > config['evidence_bytes']:
                    raise RuntimeError('Prime evidence exceeds byte limit')
                if pipe is process.stderr:
                    os.write(2, chunk)
                    continue
                partial.extend(chunk)
                while b'\n' in partial:
                    line, _, tail = partial.partition(b'\n')
                    if not line or len(line) > config['frame_bytes']:
                        raise RuntimeError('Invalid Prime output frame')
                    sock.settimeout(max(0.001, deadline - time.monotonic()))
                    sock.sendall(line + b'\n')
                    partial = bytearray(tail)
                if len(partial) > config['frame_bytes']:
                    raise RuntimeError('Prime output frame exceeds byte limit')
        if partial:
            raise RuntimeError('Unterminated Prime output frame')
        status = process.wait(timeout=max(0.001, deadline - time.monotonic()))
        sock.sendall(json.dumps({'type': 'pillbox_prime_exit', 'exit_code': status}).encode() + b'\n')
        # Deliver EOF while keeping the VMM alive until the host has consumed
        # the entire capture. Then the host confirms owned teardown.
        sock.shutdown(socket.SHUT_WR)
        sock.settimeout(max(0.001, deadline - time.monotonic()))
        sock.recv(1)
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait(timeout=2)
        sock.close()


if __name__ == '__main__':
    main()
