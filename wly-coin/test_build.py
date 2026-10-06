#!/usr/bin/env python3
"""Check clone/patch/rebuild guards without compiling. Uses Git and Python stdlib."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

project = Path(__file__).resolve().parent
work = Path(tempfile.mkdtemp(prefix='wly-build-check-'))
for filename in ('build.sh', 'bitcoin-core.patch', 'coin.json'):
    shutil.copy2(project / filename, work / filename)
tools = work / 'tools'
tools.mkdir()
for name, body in {
    'cmake': '''if [ "$1" = --build ]; then
    mkdir -p "$2/bin"
    printf 'node stub\\n' > "$2/bin/bitcoind"
    printf 'cli stub\\n' > "$2/bin/bitcoin-cli"
fi
''',
    'cargo': '''if [ "$1" = build ]; then
    mkdir -p "$CARGO_TARGET_DIR/release"
    printf 'wallet stub\\n' > "$CARGO_TARGET_DIR/release/wly-wallet"
fi
''',
}.items():
    path = tools / name
    path.write_text('#!/bin/sh\nset -eu\n' + body)
    path.chmod(0o700)
env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'])

def build():
    return subprocess.run(['bash', str(work / 'build.sh')], env=env, capture_output=True, text=True)

def git(*args):
    return subprocess.check_output(['git', '-C', str(work / 'core-src'), *args], text=True).strip()

result = build()
assert result.returncode == 0, result.stdout + result.stderr
coin = json.loads((work / 'coin.json').read_text())
assert git('rev-parse', 'HEAD') == coin['bitcoin_commit']
assert coin['genesis_hash'] in (work / 'core-src/src/kernel/chainparams.cpp').read_text()
git('apply', '--reverse', '--check', str(work / 'bitcoin-core.patch'))
assert all((work / 'bin' / name).is_file() for name in ('wly-node', 'wly-cli', 'wly-wallet'))
source = work / 'core-src/src/kernel/chainparams.cpp'
before = hashlib.sha256(source.read_bytes()).digest()
result = build()
assert result.returncode == 0, result.stdout + result.stderr
assert hashlib.sha256(source.read_bytes()).digest() == before
source.write_text(source.read_text().replace(coin['genesis_hash'], '0' * 64))
modified = source.read_bytes()
assert build().returncode != 0, 'Conflicting patch silently accepted'
assert source.read_bytes() == modified, 'Conflicting source overwritten'
git('-c', 'user.name=Build test', '-c', 'user.email=build-test@example.invalid',
    'commit', '--allow-empty', '-m', 'Test wrong revision')
result = build()
assert result.returncode != 0 and 'pinned Bitcoin Core' in result.stderr
assert source.read_bytes() == modified
print('PASS: clone, pinned revision, WLY genesis, patch replay, bin creation, conflict and wrong-revision guards.')
print(f'Artifacts: {work}')
