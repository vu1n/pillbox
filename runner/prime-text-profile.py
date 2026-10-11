"""Record the installed Prime binary's offline catalog, without a model turn."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

MAX_PROFILE_BYTES = 1024 * 1024


def profile(binary):
    with tempfile.TemporaryDirectory(prefix='pillbox-prime-catalog-') as home:
        env = {
            'HOME': home, 'PATH': '/usr/local/bin:/usr/bin:/bin',
            'PRIME_AGENT_CODING_AGENT_DIR': home + '/agent',
            'PRIME_API_KEY': 'pillbox-catalog-probe', 'PI_OFFLINE': '1',
            'DO_NOT_TRACK': '1',
            # Qualified 0.9.8 worker mode bypasses the persistent daemon. The
            # assembly supervisor owns this process; no prompt is submitted.
            'PRIME_AGENT_INTERNAL_LEGACY_OWNED_WORKER_FRONTEND': '1',
        }
        version = subprocess.run([binary, '--version'], env=env, capture_output=True,
                                 check=True, timeout=30).stdout.decode().strip()
        if version != '0.9.8':
            raise ValueError('Prime native cohort requires text-driver qualification')
        argv = [binary, '--mode', 'rpc', '--offline', '--no-session', '--no-tools',
                '--no-extensions', '--no-skills', '--no-context-files', '--no-prompt-templates']
        result = subprocess.run(argv, input=b'{"id":"catalog","type":"get_available_models"}\n',
                                env=env, capture_output=True, check=True, timeout=30)
        if len(result.stdout) > MAX_PROFILE_BYTES or result.stderr:
            raise ValueError('Invalid Prime catalog probe output')
        lines = [json.loads(line) for line in result.stdout.splitlines() if line]
        replies = [line for line in lines if line.get('id') == 'catalog'
                   and line.get('command') == 'get_available_models' and line.get('success') is True]
        if len(replies) != 1:
            raise ValueError('Prime catalog response absent or ambiguous')
        models = replies[0]['data']['models']
        if not models or any(model.get('provider') != 'prime-inference'
                             or model.get('baseUrl') != 'https://api.pinference.ai/api/v1'
                             for model in models):
            raise ValueError('Prime catalog contains an unsupported provider or egress host')
        return {'schema_version': 1, 'harness_version': version, 'models': models}


def main():
    binary, destination = sys.argv[1:]
    data = (json.dumps(profile(binary), separators=(',', ':')) + '\n').encode()
    if len(data) > MAX_PROFILE_BYTES:
        raise ValueError('Prime image profile exceeds bound')
    target = Path(destination)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open('xb') as output:
        output.write(data)
    os.chmod(target, 0o644)


if __name__ == '__main__':
    main()
