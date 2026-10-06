#!/usr/bin/env python3
"""Run after building a generated coin: python3 tests/create_coin.py /path/to/coin.
Uses only local native nodes and temporary wallets. Keeps artifacts for inspection.
"""
from decimal import Decimal
import errno
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import socket
import subprocess
import sys
import tempfile
import time

project = Path(sys.argv[1]).resolve()
repo = Path(__file__).resolve().parents[1]
coin = json.loads((project / 'coin.json').read_text())
slug = coin['slug']
work = Path(tempfile.mkdtemp(prefix='wally-own-coin-test-'))
print(f'Test data: {work}', flush=True)

def wait(predicate):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except (subprocess.CalledProcessError, json.JSONDecodeError):
            pass
        time.sleep(0.1)
    raise AssertionError('Local node timed out')

def free_port(ip):
    with socket.socket() as sock:
        sock.bind((ip, 0))
        return sock.getsockname()[1]

ports = [free_port('127.0.0.1'), free_port('127.0.0.2')]
rpc_ports = [free_port('127.0.0.1'), free_port('127.0.0.1')]
assert len(set(ports + rpc_ports)) == 4, 'Retry test: local port allocation collided'

def rpc(index, *args):
    output = subprocess.check_output([
        str(project / 'bin' / f'{slug}-cli'), f'-datadir={work / str(index)}',
        f'-rpcport={rpc_ports[index]}', *map(str, args)], stderr=subprocess.DEVNULL, text=True)
    try:
        return json.loads(output, parse_float=Decimal)
    except json.JSONDecodeError:
        return output.strip()

wallet = [str(project / 'bin' / f'{slug}-wallet'), '--data-dir', str(work / 'wallets')]
for ip, port in zip(('127.0.0.1', '127.0.0.2'), ports):
    wallet += ['--peer', f'{ip}:{port}']

def interactive(args, prompts):
    pid, fd = pty.fork()
    if pid == 0:
        os.execv(wallet[0], wallet + args)
    transcript = b''
    # Wallet relay can wait up to 60 seconds for each of its two peers.
    deadline = time.monotonic() + 180
    try:
        for prompt, reply in prompts:
            while prompt not in transcript:
                assert time.monotonic() < deadline, transcript.decode(errors='replace')
                if select.select([fd], [], [], 0.1)[0]:
                    transcript += os.read(fd, 4096)
            transcript = transcript.split(prompt, 1)[1]
            os.write(fd, reply + b'\n')
        while True:
            assert time.monotonic() < deadline, transcript.decode(errors='replace')
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    data = os.read(fd, 4096)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                transcript += data
        _, status = os.waitpid(pid, 0)
        assert os.waitstatus_to_exitcode(status) == 0, transcript.decode(errors='replace')
    except BaseException:
        try:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
        raise
    finally:
        os.close(fd)

# Reject invalid names, reserved ports, overflowing issuance and existing output.
for name, reward, interval, port, destination in [
    ('../bad', '50', '210000', '19335', work / 'invalid-name'),
    ('Valid', '50', '210000', '8333', work / 'invalid-port'),
    ('Valid', '1000000', '10000000', '19335', work / 'overflow'),
    ('Valid', '50', '210000', '19335', work),
]:
    result = subprocess.run(['bash', str(repo / 'create-coin.sh')], text=True,
                            input=f'{name}\nXYZ\n60\n{reward}\n{interval}\n{port}\n19336\n\n{destination}\ny\nn\n',
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    assert result.returncode != 0, result.stdout
    if destination != work:
        assert not destination.exists()
raw = bytes.fromhex(coin['genesis'])
assert hashlib.sha256(hashlib.sha256(raw[:80]).digest()).digest()[::-1].hex() == coin['genesis_hash']
assert coin['magic'] not in ('f9beb4d9', 'c0c0c0c0', 'fabfb5da')
assert coin['max_money_satoshis'] == coin['halving'] * sum((coin['reward'] * 100000000) >> i for i in range(64))

nodes = []
try:
    for index, ip in enumerate(('127.0.0.1', '127.0.0.2')):
        (work / str(index)).mkdir()
        log = open(work / f'node-{index}.log', 'w')
        command = [str(project / 'bin' / f'{slug}-node'), f'-datadir={work / str(index)}',
                   f'-bind={ip}:{ports[index]}', f'-port={ports[index]}',
                   f'-rpcport={rpc_ports[index]}', '-server=1', '-listen=1', '-listenonion=0',
                   '-dnsseed=0', '-fixedseeds=0', '-discover=0', '-persistmempool=0']
        if index:
            command.append(f'-connect=127.0.0.1:{ports[0]}')
        nodes.append(subprocess.Popen(command, stdout=log, stderr=log))
        log.close()
        wait(lambda: rpc(index, 'getblockhash', 0) == coin['genesis_hash'])
    password = b'local-test-password-only'
    for name in ('sender', 'receiver'):
        interactive(['init', '--coin', slug, '--name', name], [
            (b'New wallet password (12+ characters): ', password),
            (b'Repeat password: ', password)])
    sender = subprocess.check_output(wallet + ['receive', 'sender'], text=True).strip()
    receiver = subprocess.check_output(wallet + ['receive', 'receiver'], text=True).strip()
    assert rpc(0, 'validateaddress', sender)['isvalid']
    assert rpc(0, 'validateaddress', receiver)['isvalid']
    rpc(0, 'generatetoaddress', 101, sender, 10000000)
    wait(lambda: rpc(1, 'getblockcount') == 101)
    interactive(['send', 'sender', '--to', receiver, '--amount', '1'], [
        (b"Type 'send' to confirm: ", b'send'), (b'Wallet password: ', password)])
    pending = rpc(0, 'getrawmempool')
    assert len(pending) == 1, pending
    state = json.loads((work / 'wallets/sender.state').read_text())
    assert len(state['outgoing']) == 1
    txid = hashlib.sha256(hashlib.sha256(bytes.fromhex(state['outgoing'][0])).digest()).digest()[::-1].hex()
    assert pending == [txid], 'Native node did not accept the saved signed payment'
    fee = int(rpc(0, 'getmempoolentry', txid)['fees']['base'] * 100000000)
    block = rpc(0, 'generatetoaddress', 1, receiver, 10000000)[0]
    assert txid in rpc(0, 'getblock', block)['tx']
    wait(lambda: rpc(1, 'getblockcount') == 102)
    balance = subprocess.check_output(wallet + ['balance', 'receiver'], text=True)
    received = 100000000 + fee + ((coin['reward'] * 100000000) >> (102 // coin['halving']))
    assert balance.startswith(f'{received // 100000000}.{received % 100000000:08d} {slug} confirmed'), balance
    subprocess.run(wallet + ['sync', 'sender'], check=True)
    restored = subprocess.check_output(wallet + ['balance', 'receiver'], text=True)
    assert restored == balance, 'Cache reload changed the confirmed balance'
    for flag in ('-regtest', '-testnet', '-testnet4', '-signet'):
        wrong_chain = subprocess.run([str(project / 'bin' / f'{slug}-node'),
                                      f'-datadir={work / "0"}', flag], capture_output=True)
        assert wrong_chain.returncode != 0, f'Fork accidentally supports {flag}'
        assert b'This fork only supports its own chain.' in wrong_chain.stderr + wrong_chain.stdout
    print('PASS: input validation, genesis, two native nodes, mining, signed payment, confirmation and cache reload.')
finally:
    for index, node in enumerate(nodes):
        try:
            rpc(index, 'stop')
        except subprocess.CalledProcessError:
            node.terminate()
        try:
            node.wait(timeout=15)
        except subprocess.TimeoutExpired:
            node.kill()
            node.wait()
