from decimal import Decimal
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
