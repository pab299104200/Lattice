#!/usr/bin/env python3
"""Exercise cold/warm health startup through a real isolated daemon and CLI.

Usage: python3 daemon/tests/health_startup_smoke.py /absolute/path/to/lattice
Creates only a temporary Git fixture and terminates only its own daemon process.
"""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, timeout=30, **kwargs)


def health(binary, repo, env):
    response = json.loads(run([binary, 'status', '--workspace', str(repo),
                               '--scope', 'health', '--json'], env=env).stdout)
    if 'content' in response:
        response = json.loads(response['content'][0]['text'])
    return response


def check_startup(binary, repo, runtime, label):
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        address = probe.getsockname()
    env = dict(os.environ, LATTICE_DAEMON_ADDR=f'{address[0]}:{address[1]}',
               XDG_RUNTIME_DIR=str(runtime))
    with (runtime / f'{label}.log').open('w') as log:
        daemon = subprocess.Popen([binary, '--daemon'], env=env, stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 90
            while True:
                if daemon.poll() is not None:
                    raise AssertionError(f'{label} daemon exited; see {runtime}')
                try:
                    with socket.create_connection(address, timeout=0.2):
                        break
                except OSError:
                    if time.monotonic() >= deadline:
                        raise AssertionError(f'{label} listener did not start')
                    time.sleep(0.1)
            last = {}
            while time.monotonic() < deadline:
                last = health(binary, repo, env)
                families = {item['family']: item for item in last.get('families', [])}
                if all(families.get(name, {}).get('availability') == 'available'
                       and families[name]['files_covered'] > 0
                       for name in ('graph', 'git', 'complexity')):
                    assert last['git_intelligence']['file_history_availability'] == 'available'
                    assert last['coverage_basis']['denominator'] == 'all_indexed_files'
                    return {'startup': label, 'availability': last['availability'],
                            'families': last['families']}
                time.sleep(0.3)
            raise AssertionError(f'{label} facts not ready: {last}')
        finally:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=10)


def main():
    binary = str(Path(sys.argv[1]).resolve(strict=True))
    with tempfile.TemporaryDirectory(prefix='lattice-health-startup-') as directory:
        root = Path(directory)
        repo, runtime = root / 'repo', root / 'runtime'
        repo.mkdir()
        runtime.mkdir(mode=0o700)
        (repo / '.gitignore').write_text('.lattice/\n')
        (repo / 'app.py').write_text('def select(value):\n    if value:\n        return 1\n    return 0\n')
        (repo / 'test_app.py').write_text('from app import select\n\ndef test_select():\n    assert select(True) == 1\n')
        (repo / 'README.md').write_text('# Health startup fixture\n')
        run(['git', 'init', '-q', str(repo)])
        run(['git', '-C', str(repo), 'add', '.'])
        run(['git', '-C', str(repo), '-c', 'user.name=Fixture', '-c',
             'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false',
             'commit', '-qm', 'fix: establish startup fixture'])
        results = [check_startup(binary, repo, runtime, label) for label in ('cold', 'warm')]
        print(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
