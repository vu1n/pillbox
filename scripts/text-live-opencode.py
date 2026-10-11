#!/usr/bin/env python3
"""Run the OpenCode text/2 guest driver outside a VM against a real provider.

The cloud stand-in for a libkrun text turn when no HVF/KVM host can boot one. It
takes the guest driver verbatim from `src/sandbox/libkrun/repository/opencode.rs`
and swaps only what the VM provides: the vsock relay becomes a Unix socket, the
guest network setup is skipped, and the host's egress proxy and CA trust stand in
for the VMM's allowlist. The credential store is copied into a private HOME as the
VM builder copies it into the rootfs.

    scripts/text-live-opencode.py STORE_DIR TURN_JSON CAPTURE_JSONL

STORE_DIR holds `opencode.db` (an OpenCode 2 home's `.local/share/opencode`).
TURN_JSON is the turn document `opencode::turn_document` builds. The relayed
frames are written to CAPTURE_JSONL, one per line, as the host reads them.
"""

import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
SOURCE = ROOT / 'src/sandbox/libkrun/repository/opencode.rs'
PASSTHROUGH = ('HTTPS_PROXY', 'https_proxy', 'NO_PROXY', 'no_proxy', 'SSL_CERT_FILE')


def patched_driver(runtime, home, workspace, relay):
    driver = re.search(r'const DRIVER: &str = r#"(.*?)"#;', SOURCE.read_text(), re.S).group(1)
    host_env = {name: os.environ[name] for name in PASSTHROUGH if name in os.environ}
    ca = os.environ.get('NODE_EXTRA_CA_CERTS') or os.environ.get('SSL_CERT_FILE')
    swaps = [
        ("RUNTIME = '/opt/pillbox-execution'", f'RUNTIME = {str(runtime)!r}'),
        ("'HOME': '/home/pillbox',", f"'HOME': {str(home)!r}, **{host_env!r},"),
        ("'NODE_EXTRA_CA_CERTS': RUNTIME + '/ca.crt',", f"'NODE_EXTRA_CA_CERTS': {ca!r},"),
        ("    for argv in (\n", "    for argv in () and (\n"),
        ("    with open('/etc/resolv.conf', 'w') as handle:\n"
         "        handle.write('nameserver 10.0.2.2\\n')\n", ""),
        ("socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)",
         "socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)"),
        ("sock.connect((2, 1067))", f"sock.connect({str(relay)!r})"),
        ("cwd='/workspace'", f"cwd={str(workspace)!r}"),
    ]
    for old, new in swaps:
        if old not in driver:
            sys.exit(f'guest driver changed; cannot patch {old!r}')
        driver = driver.replace(old, new, 1)
    return driver


def main():
    store, turn, capture = map(pathlib.Path, sys.argv[1:4])
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        runtime, home, workspace = scratch / 'runtime', scratch / 'home', scratch / 'workspace'
        data = home / '.local/share/opencode'
        for path in (runtime, data, workspace):
            path.mkdir(parents=True)
        for name in ('opencode.db', 'opencode.db-wal', 'opencode.db-shm'):
            if (store / name).exists():
                shutil.copyfile(store / name, data / name)
        shutil.copyfile(turn, runtime / 'turn.json')
        (runtime / 'limits.json').write_text(
            json.dumps({'duration_ms': 180_000, 'max_frame_bytes': 1_048_576}))
        relay = scratch / 'relay.sock'
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(relay))
        listener.listen(1)
        driver = runtime / 'opencode_turn.py'
        driver.write_text(patched_driver(runtime, home, workspace, relay))
        guest = subprocess.Popen([sys.executable, '-I', '-S', str(driver)])
        listener.settimeout(60)
        conn, _ = listener.accept()
        with conn, open(capture, 'wb') as out:
            while chunk := conn.recv(65536):
                out.write(chunk)
        sys.exit(guest.wait(timeout=30))


if __name__ == '__main__':
    main()
