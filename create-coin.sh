#!/usr/bin/env bash
# Creates a development-only Bitcoin Core fork and a matching native Wally wallet.
set -euo pipefail
umask 077
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
for tool in python3 git tar; do
    command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }
done
add_viewer() {
python3 - "$1" <<'VIEWER'
import json
from pathlib import Path
import sys

project = Path(sys.argv[1]).resolve()
coin = json.loads((project / 'coin.json').read_text())
if not (project / 'rpc').is_file() or not isinstance(coin.get('rpc_port'), int):
    raise SystemExit('Expected a generated coin project with coin.json and rpc.')
for filename in ('viewer', 'viewer.py'):
    if (project / filename).exists() or (project / filename).is_symlink():
        raise SystemExit(f'Refusing to overwrite {project / filename}')
(project / 'viewer.py').write_text(r'''from decimal import Decimal
from html import escape
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlencode, urlsplit
import argparse
import json
import re
import subprocess

ROOT = Path(__file__).resolve().parent
COIN = json.loads((ROOT / 'coin.json').read_text())


def decode_reply(command, text):
    if command == 'getblockhash':
        value = text.strip()
        if not re.fullmatch(r'[0-9a-f]{64}', value):
            raise ValueError('Invalid block hash from native CLI.')
        return value
    return json.loads(text, parse_float=Decimal)


def rpc(command, *args):
    if command not in {'getblockchaininfo', 'getblockhash', 'getblock', 'validateaddress', 'scantxoutset'}:
        raise ValueError('Viewer RPC is read-only.')
    if command == 'scantxoutset' and (len(args) != 2 or args[0] != 'start'):
        raise ValueError('Viewer only starts address UTXO scans.')
    timeout = 310 if command == 'scantxoutset' else 10
    result = subprocess.run([str(ROOT / 'rpc'), '-rpcconnect=127.0.0.1',
                             f'-rpcport={COIN["rpc_port"]}', f'-rpcclienttimeout={timeout - 1}',
                             command, *map(str, args)],
                            check=True, capture_output=True, text=True, timeout=timeout)
    return decode_reply(command, result.stdout)


def block_id(value):
    if re.fullmatch(r'[0-9]{1,10}', value) and int(value) <= 2147483647:
        return int(value)
    if re.fullmatch(r'[0-9a-fA-F]{64}', value):
        return value.lower()
    raise ValueError('Enter a block height or a 64-character block hash.')


def address_id(value):
    if not re.fullmatch(r'[A-Za-z0-9]{14,90}', value):
        raise ValueError('Enter an address for this coin, not a descriptor or RPC option.')
    return value


def block_link(value, text):
    return '<a href="/block?' + escape(urlencode({'id': value})) + '">' + escape(str(text)) + '</a>'


def page(title, body):
    return ("""<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>""" + escape(str(COIN['name'])) + ' explorer</title>' + """
<style>
body{max-width:1100px;margin:2rem auto;padding:0 1rem;background:#faf9f6;color:#292722;
font:16px/1.5 system-ui,sans-serif}a{color:#84531b}h1{line-height:1.2}
form{display:flex;gap:.5rem;flex-wrap:wrap}input{flex:1;min-width:12rem;padding:.6rem}
button{padding:.6rem 1rem}table{border-collapse:collapse;width:100%;margin:1rem 0}
th,td{text-align:left;padding:.6rem;border-bottom:1px solid #ddd;overflow-wrap:anywhere}
code,pre{font-family:ui-monospace,monospace;overflow-wrap:anywhere}
pre{white-space:pre-wrap;font-size:.85rem}details{padding:.7rem 0;border-bottom:1px solid #ddd}
summary{cursor:pointer;overflow-wrap:anywhere}.note{color:#6c6251}
</style><body><nav><a href="/">""" + escape(str(COIN['name'])) + ' (' +
            escape(str(COIN['symbol'])) + ')</a></nav><p class="note">Local, read-only development-chain viewer.</p>' +
            '<form action="/block"><input name="id" aria-label="Block height or block hash" '
            'placeholder="Block height or block hash" required><button>Find block</button></form>' +
            '<form action="/address"><input name="id" aria-label="Coin address" '
            'placeholder="Coin address" required><button>Find address</button></form>' +
            '<h1>' + escape(title) + '</h1>' + body + '</body></html>').encode('utf-8')


def block_body(block):
    body = '<p>Hash: <code>' + escape(str(block['hash'])) + '</code></p>'
    body += '<p>Transactions: ' + escape(str(len(block['tx']))) + '</p>'
    for field, label in (('previousblockhash', 'Previous block'), ('nextblockhash', 'Next block')):
        if block.get(field):
            body += '<p>' + label + ': ' + block_link(block[field], block[field]) + '</p>'
    # ponytail: full transaction JSON per block; add pagination if large blocks become slow.
    for tx in block['tx']:
        body += '<details><summary>' + escape(str(tx['txid'])) + '</summary><pre>'
        body += escape(json.dumps(tx, indent=2, default=lambda amount: format(amount, 'f'))) + '</pre></details>'
    return body


def address_body(scan):
    body = '<p>Confirmed unspent: <strong>' + format(scan['total_amount'], '.8f') + ' ' + escape(str(COIN['symbol'])) + '</strong></p>'
    body += '<p>Snapshot at block ' + block_link(scan['bestblock'], scan['height']) + '.</p>'
    body += '<p class="note">Ignores mempool changes; may include immature mining rewards. Not spent transactions or full history.</p>'
    if not scan['unspents']:
        return body + '<p>No unspent outputs.</p>'
    body += '<table><thead><tr><th>Transaction:output</th><th>Amount</th><th>Block</th><th>Confirmations</th></tr></thead><tbody>'
    for output in scan['unspents']:
        body += '<tr><td><code>' + escape(str(output['txid'])) + ':' + escape(str(output['vout'])) + '</code></td>'
        body += '<td>' + format(output['amount'], '.8f') + '</td><td>' + block_link(output['blockhash'], output['height'])
        body += '</td><td>' + escape(str(output['confirmations'])) + '</td></tr>'
    return body + '</tbody></table>'


class Handler(BaseHTTPRequestHandler):
    def setup(self):
        self.request.settimeout(10)
        super().setup()

    def reply(self, status, title, body):
        data = page(title, body)
        self.send_response(status)
        self.send_header('Content-Type', 'text/html; charset=utf-8')
        self.send_header('Content-Length', str(len(data)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.send_header('Content-Security-Policy', "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        try:
            if len(self.path) > 256:
                raise ValueError('Request too long.')
            url = urlsplit(self.path)
            if url.path not in ('/', '/block', '/address'):
                self.reply(404, 'Not found', '<p>No such page.</p>')
                return
            params = parse_qs(url.query, max_num_fields=1)
            value = params.get('id', [''])[0].strip()
            if url.path == '/block':
                value = block_id(value)
            elif url.path == '/address':
                value = address_id(value)
            if rpc('getblockhash', 0) != COIN['genesis_hash']:
                self.reply(503, 'Wrong node', '<p>The node genesis does not match this coin.</p>')
                return
            if url.path == '/address':
                if not rpc('validateaddress', value)['isvalid']:
                    raise ValueError('Invalid address or address for a different coin.')
                # ponytail: synchronous full UTXO scan; cache/index addresses if frequent scans become slow.
                scan = rpc('scantxoutset', 'start', json.dumps([f'addr({value})']))
                if not scan['success']:
                    self.reply(503, 'Scan incomplete', '<p>No balance shown because the UTXO scan did not finish. Retry later.</p>')
                    return
                self.reply(200, 'Address ' + value, address_body(scan))
                return
            if url.path == '/block':
                block_hash = rpc('getblockhash', value) if isinstance(value, int) else value
                block = rpc('getblock', block_hash, 2)
                self.reply(200, f'Block {block["height"]}', block_body(block))
                return
            info = rpc('getblockchaininfo')
            body = '<p>Height: ' + escape(str(info['blocks'])) + ' | Headers: ' + escape(str(info['headers'])) + '</p>'
            body += '<table><thead><tr><th>Height</th><th>Hash</th><th>Transactions</th></tr></thead><tbody>'
            for height in range(info['blocks'], max(-1, info['blocks'] - 10), -1):
                block = rpc('getblock', rpc('getblockhash', height), 1)
                body += '<tr><td>' + block_link(height, height) + '</td><td><code>'
                body += block_link(block['hash'], block['hash']) + '</code></td><td>' + escape(str(block['nTx'])) + '</td></tr>'
            self.reply(200, 'Recent blocks', body + '</tbody></table>')
        except ValueError as error:
            self.reply(400, 'Invalid request', '<p>' + escape(str(error)) + '</p>')
        except (OSError, subprocess.SubprocessError):
            self.reply(503, 'Node or scan unavailable', '<p>Build and start the native node with <code>./node -daemon</code>, then check the block height/hash or retry after any running UTXO scan finishes.</p>')


def check():
    assert decode_reply('getblockhash', 'a' * 64 + '\n') == 'a' * 64
    assert decode_reply('getblock', '{"value":0.00000001}')['value'] == Decimal('0.00000001')
    assert block_id('0') == 0
    assert block_id('A' * 64) == 'a' * 64
    for value in ('-1', '../wallet.json', 'getwalletinfo', '2147483648', '0' * 65):
        try:
            block_id(value)
        except ValueError:
            pass
        else:
            raise AssertionError(value)
    try:
        rpc('sendrawtransaction', '00')
    except ValueError:
        pass
    else:
        raise AssertionError('Write RPC allowed')
    assert address_id('gsz39BPM7See7TthqkGBtxuDnf7FKJT6oG') == 'gsz39BPM7See7TthqkGBtxuDnf7FKJT6oG'
    for value in ('-rpcconnect=remote', 'addr(test)', '<script>', 'a' * 91):
        try:
            address_id(value)
        except ValueError:
            pass
        else:
            raise AssertionError(value)
    try:
        rpc('scantxoutset', 'abort')
    except ValueError:
        pass
    else:
        raise AssertionError('Abort scan allowed')
    scan = {'total_amount': Decimal('0.00000001'), 'height': 1, 'bestblock': 'a' * 64,
            'unspents': [{'txid': '<unsafe>', 'vout': 0, 'amount': Decimal('0.00000001'),
                          'blockhash': 'a' * 64, 'height': 1, 'confirmations': 1}]}
    assert '0.00000001' in address_body(scan) and '&lt;unsafe&gt;' in address_body(scan)
    assert 'No unspent outputs' in address_body(dict(scan, total_amount=Decimal(0), unspents=[]))
    body = block_body({'hash': 'a' * 64, 'tx': [{'txid': 'b' * 64, 'data': '<script>alert(1)</script>', 'value': Decimal('0.00000001')}]})
    rendered = page('<unsafe>', body)
    assert b'<script>' not in rendered and b'&lt;script&gt;' in rendered
    assert b'&lt;unsafe&gt;' in rendered and b'0.00000001' in rendered
    print('Viewer checks passed: block/address validation, read-only RPC, UTXO rendering, HTML escaping, exact amounts.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description='Read-only localhost blockchain viewer')
    parser.add_argument('port', nargs='?', type=int, default=8080)
    parser.add_argument('--check', action='store_true', help='Run the offline self-check')
    args = parser.parse_args()
    if args.check:
        check()
    else:
        if not 1024 <= args.port <= 65535:
            parser.error('Choose a port between 1024 and 65535.')
        try:
            # ponytail: localhost, single request at a time; batch local RPC if browsing becomes slow.
            with HTTPServer(('127.0.0.1', args.port), Handler) as server:
                print(f'Viewer: http://127.0.0.1:{args.port}', flush=True)
                server.serve_forever()
        except KeyboardInterrupt:
            pass
        except OSError as error:
            parser.exit(1, f'{error}\nTry another port: ./viewer 8081\n')
''')
wrapper = project / 'viewer'
wrapper.write_text('#!/usr/bin/env bash\nset -euo pipefail\nroot=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)\nexec python3 "$root/viewer.py" "$@"\n')
wrapper.chmod(0o700)
print(f'Viewer added: {project}/viewer (http://127.0.0.1:8080)')
VIEWER
}
if (( $# > 0 )); then
    [[ $# == 2 && "$1" == --viewer-only ]] || { echo 'Usage: ./create-coin.sh [--viewer-only COIN_DIRECTORY]' >&2; exit 1; }
    add_viewer "$2"
    exit
fi
ask() { local answer; read -r -p "$1 [$2]: " answer; printf '%s' "${answer:-$2}"; }
name=$(ask 'Coin name' Wally)
symbol=$(ask 'Symbol' WLY)
spacing=$(ask 'Seconds per block' 60)
reward=$(ask 'Initial reward, whole coins' 50)
halving=$(ask 'Halving interval, blocks' 210000)
port=$(ask 'P2P port' 19335)
rpc_port=$(ask 'Local RPC port' 19336)
peers=$(ask 'Initial peers, comma-separated IP:PORT (optional)' '')
output=$(ask 'New output directory' "$PWD/${symbol,,}-coin")
echo 'Development chain only: initial PoW is easy and the chain can be cheaply rewritten.'
echo 'No external APIs. Building needs Rust, CMake, C++20, Boost 1.73-1.89 and libevent headers.'
echo 'BITCOIN_SOURCE can point to a local Bitcoin Core repository for offline generation.'
read -r -p 'Create the separate coin project? [y/N]: ' confirm
[[ "$confirm" =~ ^[yY]([eE][sS])?$ ]] || { echo Cancelled.; exit 0; }
python3 - "$root" "$name" "$symbol" "$spacing" "$reward" "$halving" "$port" "$rpc_port" "$peers" "$output" <<'PY'
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import struct
import subprocess
import sys
import time

PIN = 'd0f6d9953a15d7c7111d46dcb76ab2bb18e5dee3'  # Bitcoin Core v30.0
root, name, symbol, spacing, reward, halving, port, rpc_port, peers, output = sys.argv[1:]
root, output = Path(root), Path(output).resolve()

def require(condition, message):
    if not condition:
        raise SystemExit(message)

require(re.fullmatch(r'[A-Za-z][A-Za-z0-9 -]{0,39}', name), 'Invalid coin name (1-40 ASCII characters).')
require(re.fullmatch(r'[A-Z][A-Z0-9]{1,7}', symbol), 'Symbol must be 2-8 uppercase letters/digits.')
slug = symbol.lower()
require(slug not in {'btc', 'doge', 'bitcoin', 'dogecoin'}, 'Choose a symbol different from Bitcoin/Dogecoin.')
for value in (spacing, reward, halving, port, rpc_port):
    require(re.fullmatch(r'[0-9]{1,10}', value), 'Numeric inputs must be positive whole numbers.')
spacing, reward, halving, port, rpc_port = map(int, (spacing, reward, halving, port, rpc_port))
require(10 <= spacing <= 3600, 'Block interval must be 10-3600 seconds.')
require(1 <= reward <= 1000000, 'Reward must be 1-1000000 whole coins.')
require(100 <= halving <= 10000000, 'Halving interval must be 100-10000000 blocks.')
reserved = {8332, 8333, 18332, 18333, 18443, 18444, 38332, 38333, 48332, 48333, 22555, 22556}
require(all(1024 <= p <= 65535 and p not in reserved for p in (port, rpc_port)), 'Choose unprivileged, non-Bitcoin/Dogecoin ports.')
require(port != rpc_port, 'P2P and RPC ports must differ.')
reward_sat = reward * 100000000
max_money = halving * sum(reward_sat >> shift for shift in range(64))
require(max_money <= 2**63 - 1, 'Issuance would overflow native consensus amounts.')
peers = [p.strip() for p in peers.split(',') if p.strip()]
for peer in peers:
    match = re.fullmatch(r'(\[[0-9a-fA-F:]+\]|[0-9.]+):([0-9]{1,5})', peer)
    require(match, 'Peers must be literal IP:PORT, with brackets around IPv6.')
    address = ipaddress.ip_address(match[1].strip('[]'))
    require(not address.is_unspecified and not address.is_multicast, 'Invalid peer IP.')
    require(1 <= int(match[2]) <= 65535, 'Invalid peer port.')
require(not output.exists(), f'Refusing to overwrite {output}')
source = os.environ.get('BITCOIN_SOURCE')
if source:
    result = subprocess.check_output(['git', '-C', source, 'rev-parse', '--verify', PIN + '^{commit}'], text=True).strip()
    require(result == PIN, 'Local repository must contain the pinned Bitcoin Core v30.0 commit.')
output.mkdir(parents=True, mode=0o700)
node, wallet = output / 'node-src', output / 'wallet-src'
if source:
    node.mkdir()
    archive = subprocess.Popen(['git', '-C', source, 'archive', PIN], stdout=subprocess.PIPE)
    try:
        subprocess.run(['tar', '-xf', '-', '-C', str(node)], stdin=archive.stdout, check=True)
    finally:
        archive.stdout.close()
        require(archive.wait() == 0, 'Could not archive the pinned source.')
else:
    subprocess.run(['git', 'clone', '--depth', '1', '--branch', 'v30.0', '--single-branch',
                    'https://github.com/bitcoin/bitcoin.git', str(node)], check=True)
    require(subprocess.check_output(['git', '-C', str(node), 'rev-parse', 'HEAD'], text=True).strip() == PIN,
            'Upstream release changed: refusing to patch a different commit.')
wallet.mkdir()
for filename in ('Cargo.toml', 'Cargo.lock'):
    shutil.copyfile(root / filename, wallet / filename)
shutil.copytree(root / 'src', wallet / 'src')
shutil.copytree(root / 'tests' / 'data', wallet / 'tests' / 'data')

# Bitcoin's genesis coinbase output is deliberately unspendable. No premine.
def sha(data):
    return hashlib.sha256(hashlib.sha256(data).digest()).digest()

def push(data):
    return (bytes([len(data)]) if len(data) <= 75 else b'\x4c' + bytes([len(data)])) + data

stamp = int(time.time())
message = f'{name} ({symbol}) genesis {stamp} {secrets.token_hex(8)}'
script = bytes.fromhex('04ffff001d0104') + push(message.encode('ascii'))
require(len(script) <= 100, 'Genesis coinbase message too long.')
pubkey = '04678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5f'
pay_script = bytes.fromhex('41' + pubkey + 'ac')
tx = (struct.pack('<I', 1) + b'\x01' + bytes(32) + b'\xff' * 4 + bytes([len(script)]) + script
      + b'\xff' * 4 + b'\x01' + struct.pack('<q', reward_sat) + bytes([len(pay_script)])
      + pay_script + bytes(4))
bits = 0x1f00ffff
limit = 0xffff << (8 * (0x1f - 3))
header = struct.pack('<I', 1) + bytes(32) + sha(tx) + struct.pack('<II', stamp, bits)
for nonce in range(2**32):
    raw_header = header + struct.pack('<I', nonce)
    if int.from_bytes(sha(raw_header), 'little') <= limit:
        break
else:
    raise SystemExit('Genesis mining exhausted its nonce space.')
genesis_hash = sha(raw_header)[::-1].hex()
merkle = sha(tx)[::-1].hex()
genesis = (raw_header + b'\x01' + tx).hex()
magic = bytes(secrets.randbelow(128) + 128 for _ in range(4))
while magic in (bytes.fromhex('f9beb4d9'), bytes.fromhex('c0c0c0c0'), bytes.fromhex('fabfb5da')):
    magic = bytes(secrets.randbelow(128) + 128 for _ in range(4))
prefixes = secrets.SystemRandom().sample([p for p in range(32, 110) if p != 65], 3)
pkh, sh, secret = prefixes
manifest = dict(name=name, symbol=symbol, slug=slug, spacing=spacing, reward=reward,
                halving=halving, port=port, rpc_port=rpc_port, peers=peers, genesis=genesis,
                genesis_hash=genesis_hash, bits=bits, magic=magic.hex(), p2pkh=pkh, p2sh=sh,
                max_money_satoshis=max_money, bitcoin_commit=PIN, development_only=True)
(output / 'coin.json').write_text(json.dumps(manifest, indent=2) + '\n')

def replace(text, old, new):
    require(text.count(old) == 1, f'Expected exactly one source match: {old[:80]}')
    return text.replace(old, new)

def patch(path, old, new):
    path.write_text(replace(path.read_text(), old, new))

def sub(text, pattern, replacement, expected=1):
    result, count = re.subn(pattern, lambda _: replacement, text, flags=re.S)
    require(count == expected, f'Unexpected pinned source layout: {pattern}')
    return result

path = node / 'src/kernel/chainparams.cpp'
text = path.read_text()
start, end = text.index('class CMainParams :'), text.index('class CTestNetParams :')
main = text[start:end]
main = sub(main, r'        consensus\.script_flag_exceptions\.emplace\(.*?;\n', '', 2)
assignments = {
    'consensus.nSubsidyHalvingInterval': str(halving),
    'consensus.BIP34Hash': 'uint256{}',
    'consensus.MinBIP9WarningHeight': '1',
    'consensus.powLimit': 'uint256{"0000' + 'f' * 60 + '"}',
    'consensus.nPowTargetTimespan': str(spacing * 2016),
    'consensus.nPowTargetSpacing': str(spacing),
    'consensus.vDeployments[Consensus::DEPLOYMENT_TAPROOT].nStartTime': 'Consensus::BIP9Deployment::ALWAYS_ACTIVE',
    'consensus.vDeployments[Consensus::DEPLOYMENT_TAPROOT].nTimeout': 'Consensus::BIP9Deployment::NO_TIMEOUT',
    'consensus.vDeployments[Consensus::DEPLOYMENT_TAPROOT].min_activation_height': '0',
    'consensus.nMinimumChainWork': 'uint256{}',
    'consensus.defaultAssumeValid': 'uint256{}',
    'nDefaultPort': str(port),
    'm_assumed_blockchain_size': '0',
    'm_assumed_chain_state_size': '0',
    'vFixedSeeds': '{}',
    'bech32_hrp': json.dumps(slug),
}
for field in ('BIP34Height', 'BIP65Height', 'BIP66Height', 'CSVHeight', 'SegwitHeight'):
    assignments['consensus.' + field] = '1'
for index, byte in enumerate(magic):
    assignments[f'pchMessageStart[{index}]'] = hex(byte)
for field, byte in (('PUBKEY_ADDRESS', pkh), ('SCRIPT_ADDRESS', sh), ('SECRET_KEY', secret)):
    assignments[f'base58Prefixes[{field}]'] = f'std::vector<unsigned char>(1,{byte})'
for field, value in assignments.items():
    main = sub(main, re.escape(field) + r'\s*=\s*[^;]+;', field + ' = ' + value + ';')
main = sub(main, r'genesis = CreateGenesisBlock\([^;]+;',
           f'genesis = CreateGenesisBlock({json.dumps(message)}, CScript() << "{pubkey}"_hex << OP_CHECKSIG, {stamp}, {nonce}, {hex(bits)}, 1, {reward_sat});')
main = sub(main, r'assert\(consensus.hashGenesisBlock == uint256\{"[0-9a-f]+"\}\);',
           f'assert(consensus.hashGenesisBlock == uint256{{"{genesis_hash}"}});')
main = sub(main, r'assert\(genesis.hashMerkleRoot == uint256\{"[0-9a-f]+"\}\);',
           f'assert(genesis.hashMerkleRoot == uint256{{"{merkle}"}});')
main = sub(main, r'        vSeeds\.emplace_back\([^\n]+\n', '', 9)
main = sub(main, r'm_assumeutxo_data = \{.*?\n        \};', 'm_assumeutxo_data = {};')
main = sub(main, r'chainTxData = ChainTxData\{.*?\n        \};', 'chainTxData = ChainTxData{0, 0, 0};')
path.write_text(text[:start] + main + text[end:])
patch(node / 'cmake/module/AddBoostIfNeeded.cmake',
      '  find_package(Boost 1.73.0 REQUIRED CONFIG)',
      '  find_package(Boost 1.73.0 REQUIRED CONFIG)\n'
      '  if(Boost_VERSION VERSION_GREATER_EQUAL 1.90)\n'
      '    message(FATAL_ERROR "This pinned fork needs Boost 1.73-1.89 headers.")\n'
      '  endif()')
patch(node / 'src/validation.cpp', 'CAmount nSubsidy = 50 * COIN;', f'CAmount nSubsidy = {reward_sat};')
patch(node / 'src/consensus/amount.h', 'MAX_MONEY = 21000000 * COIN;', f'MAX_MONEY = {max_money};')
patch(node / 'src/chainparamsbase.cpp', 'CBaseChainParams>("", 8332)', f'CBaseChainParams>("", {rpc_port})')
patch(node / 'src/chainparams.cpp',
      'void SelectParams(const ChainType chain)\n{',
      'void SelectParams(const ChainType chain)\n{\n'
      '    if (chain != ChainType::MAIN) throw std::runtime_error("This fork only supports its own chain.");')
patch(node / 'CMakeLists.txt', 'set(CLIENT_NAME "Bitcoin Core")', f'set(CLIENT_NAME "{name} Core")')

# Keep the existing Bitcoin code path, but bind it to this coin's own parameters.
(wallet / 'src/coin.rs').write_text(f'''use bitcoin::{{Block, CompactTarget, Target, consensus}};
pub const PORT: u16 = {port};
pub const MAGIC: [u8; 4] = {list(magic)};
pub const P2PKH: u8 = {pkh};
pub const P2SH: u8 = {sh};
pub const TIMESPAN: u64 = {spacing * 2016};
pub const PEERS: &[&str] = &{json.dumps(peers)};
pub fn genesis() -> Block {{
    consensus::deserialize(&hex::decode("{genesis}").expect("Generated genesis hex"))
        .expect("Generated genesis block")
}}
pub fn pow_limit() -> Target {{ CompactTarget::from_consensus({bits}).into() }}
pub fn params() -> consensus::Params {{
    let mut params = consensus::Params::MAINNET;
    params.max_attainable_target = pow_limit();
    params.pow_target_spacing = {spacing};
    params.pow_target_timespan = TIMESPAN;
    params
}}
''')
for path in (wallet / 'src').glob('*.rs'):
    text = path.read_text().replace('bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin)', 'crate::coin::genesis()')
    path.write_text(text)
for filename in ('Cargo.toml', 'Cargo.lock'):
    patch(wallet / filename, 'name = "wally"', f'name = "{slug}-wallet"')
path = wallet / 'src/wallet.rs'
text = path.read_text()
text = replace(text, '    Bitcoin,', f'    #[serde(rename = "{slug}")]\n    #[value(name = "{slug}")]\n    Bitcoin,')
text = replace(text, 'Self::Bitcoin => "bitcoin",', f'Self::Bitcoin => "{slug}",')
text = text.replace('Address, Amount, EcdsaSighashType, Network,', 'Amount, EcdsaSighashType,')
start, end = text.index('        if self == Self::Bitcoin {'), text.index('\n    pub fn address(')
text = text[:start] + '''        // ponytail: custom payments use Base58 only; add custom Bech32 parsing when needed.
        let bytes = bs58::decode(address).with_check(None).into_vec()?;
        ensure!(bytes.len() == 21, "Invalid address length");
        match (self, bytes[0]) {
            (Self::Bitcoin, crate::coin::P2PKH) | (Self::Dogecoin, 30) =>
                Ok(ScriptBuf::new_p2pkh(&PubkeyHash::from_slice(&bytes[1..])?)),
            (Self::Bitcoin, crate::coin::P2SH) | (Self::Dogecoin, 22) =>
                Ok(ScriptBuf::new_p2sh(&bitcoin::ScriptHash::from_slice(&bytes[1..])?)),
            _ => bail!("Expected an address for this coin"),
        }
    }
''' + text[end:]
text = replace(text, 'if self == Self::Bitcoin { 0 } else { 30 }', 'if self == Self::Bitcoin { crate::coin::P2PKH } else { 30 }')
path.write_text(text)
patch(wallet / 'src/main.rs', 'mod chain;', 'mod chain;\nmod coin;')
patch(wallet / 'src/main.rs', 'Standalone Bitcoin and Dogecoin wallets (mainnet)', f'{name} ({symbol}) development wallet and Dogecoin wallet')
patch(wallet / 'src/main.rs', '.join(".wally")', f'.join(".{slug}-wallet")')
patch(wallet / 'src/backend.rs',
      'assert!(headers.tip() >= chain::BITCOIN_CHECKPOINT_HEIGHT);',
      'assert_eq!(headers.get(0)?.block_hash().to_string(), chain::BITCOIN_CHECKPOINT_HASH);')
path = wallet / 'src/peer.rs'
text = replace(path.read_text(), 'Coin::Bitcoin => [0xf9, 0xbe, 0xb4, 0xd9],', 'Coin::Bitcoin => crate::coin::MAGIC,')
text = sub(text, r'Coin::Bitcoin => \(\s*8333,\s*&\[.*?\],\s*\),', 'Coin::Bitcoin => (crate::coin::PORT, &[]),')
text = replace(text, '    if explicit.is_empty() {', '''    if explicit.is_empty() {
        if coin == Coin::Bitcoin {
            for host in crate::coin::PEERS {
                addresses.extend(host.to_socket_addrs()?);
            }
        }''')
path.write_text(text)
path = wallet / 'src/chain.rs'
text = replace(path.read_text(), 'BITCOIN_CHECKPOINT_HEIGHT: u32 = 709_631', 'BITCOIN_CHECKPOINT_HEIGHT: u32 = 0')
text = replace(text, '000000000000000000013712fc242ee6dd28476d0e9c931c75f83e6974c6bccc', genesis_hash)
text = replace(text, 'return Target::MAX_ATTAINABLE_MAINNET;', 'return crate::coin::pow_limit();')
text = replace(text, '.clamp(302400, 4838400)', '.clamp((crate::coin::TIMESPAN / 4) as i64, (crate::coin::TIMESPAN * 4) as i64)')
text = replace(text, 'bitcoin::consensus::Params::MAINNET,', 'crate::coin::params(),')
text = replace(text, 'corrupt[76] ^= 1;', 'corrupt[72..76].fill(0);')
text = replace(text, 'epoch[0].time + 604800', 'epoch[0].time + (crate::coin::TIMESPAN / 2) as u32')
text = replace(text, '0x1c7fff80', '0x1e7fff80')
path.write_text(text)

def executable(filename, body):
    path = output / filename
    path.write_text('#!/usr/bin/env bash\nset -euo pipefail\nroot=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)\n' + body)
    path.chmod(0o700)

(output / 'bin').mkdir()
(output / 'data').mkdir()
(output / 'data/bitcoin.conf').write_text(
    f'server=1\nport={port}\nrpcport={rpc_port}\nbind=127.0.0.1:{port}\n'
    'dnsseed=0\nfixedseeds=0\ndiscover=0\nlistenonion=0\nupnp=0\nnatpmp=0\nprune=0\n'
    + ''.join(f'addnode={p}\n' for p in peers))
executable('build.sh', f'''cmake -S "$root/node-src" -B "$root/node-build" -DCMAKE_BUILD_TYPE=Release \\
    -DENABLE_WALLET=OFF -DBUILD_TESTS=OFF -DBUILD_GUI=OFF -DENABLE_IPC=OFF \\
    -DBUILD_BITCOIN_BIN=OFF -DWITH_CCACHE=OFF
cmake --build "$root/node-build" --target bitcoind bitcoin-cli -j "${{JOBS:-2}}"
export CARGO_TARGET_DIR="$root/wallet-src/target"
cargo fmt --manifest-path "$root/wallet-src/Cargo.toml"
cargo test --locked --manifest-path "$root/wallet-src/Cargo.toml"
cargo build --release --locked --manifest-path "$root/wallet-src/Cargo.toml"
cp "$root/node-build/bin/bitcoind" "$root/bin/{slug}-node"
cp "$root/node-build/bin/bitcoin-cli" "$root/bin/{slug}-cli"
cp "$root/wallet-src/target/release/{slug}-wallet" "$root/bin/{slug}-wallet"
''')
executable('node', f'exec "$root/bin/{slug}-node" -datadir="$root/data" "$@"\n')
executable('rpc', f'exec "$root/bin/{slug}-cli" -datadir="$root/data" "$@"\n')
executable('wallet', f'exec "$root/bin/{slug}-wallet" --data-dir "$root/wallets" "$@"\n')
executable('mine', '''[[ $# -ge 1 && $# -le 2 ]] || { echo 'Usage: ./mine ADDRESS [BLOCKS]' >&2; exit 1; }
exec "$root/rpc" generatetoaddress "${2:-1}" "$1" 10000000
''')
(output / 'README.md').write_text(f'''# {name} ({symbol})

Development-only SHA256d PoW chain. Easy initial difficulty, retarget every 2016
blocks. Not a secure public currency. No premine; genesis output is unspendable.
Reward {reward} {symbol}; halving every {halving} blocks; target {spacing} seconds.
Coinbase maturity is 100 blocks. Money-range ceiling: {max_money} satoshis.
Consensus comes from the pinned Bitcoin Core v30.0 fork; review patches and
maintain upstream security updates before considering a public launch.

Build: `./build.sh` (Rust 1.89+, CMake 3.22+, C++20, Boost 1.73-1.89 headers,
and libevent 2.1.8+ development headers).
The output retains Bitcoin Core's MIT license in node-src/COPYING.
Building does not start nodes, mine coins, or open ports. You start those explicitly.
Builds may download source/dependencies, never wallet-service API data.
Use BITCOIN_SOURCE during generation and cached Cargo dependencies to build offline.

```sh
./node -daemon
./wallet init --coin {slug}
./wallet receive {slug}
./mine <receiving-address> 101
./rpc getblockchaininfo
```

The wallet still requires two distinct peer IPs. Run another instance of the
SAME generated node, not the generator again (that would create another genesis):

```sh
mkdir data2
./bin/{slug}-node -datadir="$PWD/data2" -port={port} -rpcport={rpc_port + 1 if rpc_port < 65535 else rpc_port - 1} \\
  -bind=127.0.0.2:{port} -connect=127.0.0.1:{port} -listen=1 -dnsseed=0 -fixedseeds=0 -daemon
./wallet --peer 127.0.0.1:{port} --peer 127.0.0.2:{port} balance {slug}
```

RPC is localhost/cookie-authenticated. The default node binds P2P to localhost;
remote peers require explicitly binding a reachable interface and firewalling RPC.
Distribute this generated project's sources/coin.json to run the same network.
Custom wallet destinations use Base58 P2PKH/P2SH, not Bech32. Wallet signing,
encryption, progress bars, scan recovery and pending rebroadcast stay native.
Keep wallet JSON/passwords and pending state backed up. Original Wally is unchanged.

Start the read-only web viewer: `./viewer`, then open http://127.0.0.1:8080.
It shows recent blocks and transaction details, with block height/hash and address search.
Address search uses the node's UTXO scan: confirmed unspent balance, outputs, and block links,
not spent transactions or full history. Ignores mempool changes and may include immature
mining rewards. Scans can take time on large UTXO sets; incomplete scans show no balance.
Uses Python's standard library and the native CLI, never external APIs or wallet files.
Use `./viewer 8081` for another local port; `./viewer --check` runs its offline checks.
''')
print(f'Created {name} ({symbol}) in {output}\nGenesis: {genesis_hash}\nBuild with: {output}/build.sh')
PY
add_viewer "$output"
read -r -p 'Build native binaries now? [y/N]: ' build
if [[ "$build" =~ ^[yY]([eE][sS])?$ ]]; then
    "$output/build.sh"
fi
