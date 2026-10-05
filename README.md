# wally

A standalone Bitcoin and Dogecoin CLI wallet with direct peer-to-peer networking.
No Bitcoin Core, Dogecoin Core, or API service required. Mainnet only.

**Unaudited starter wallet: use small amounts, not life savings.**

## Build and use

Building requires Rust 1.89+ and a C compiler for secp256k1. The installed binary
needs no other runtime apps.

```sh
cargo install --path . --locked
wally init --coin bitcoin
wally init --coin dogecoin
wally init --coin bitcoin --name savings
wally ls
wally receive bitcoin
wally sync bitcoin
wally balance bitcoin
wally send bitcoin --to <bitcoin-address> --amount 0.001
wally send dogecoin --to <dogecoin-address> --amount 5
```

Replace the address placeholders with real recipient addresses. `init` prompts
for a password of at least 12 characters and confirmation. Wallet names default
to `bitcoin` or `dogecoin`; names may contain ASCII letters, digits, `-`, and `_`.
`receive` prints the address for the sender to pay; it does not move coins itself.
`recive` is also accepted. `ls` lists local wallets and addresses, not balances.

`send` synchronizes first, displays the amount and network fee, asks you to type
`send`, then asks for your password. Keys are decrypted and transactions signed
locally. Passwords and private keys are never sent to peers.

## Networking and synchronization

`init`, `ls`, and `receive` work offline. `sync`, `balance`, `send`, and
`rebroadcast` require internet. Nodes are discovered through DNS seeds; wally
requires two peers with different IP addresses. Default outbound TCP ports are
8333 for Bitcoin and 22556 for Dogecoin. No inbound listener is needed.

To bypass seed discovery, supply at least two reachable peers that serve
historical blocks:

```sh
wally --peer <node-a-ip>:8333 --peer <node-b-ip>:8333 sync bitcoin
```

Use port 22556 for Dogecoin peers. Peers see your IP, requested blocks, and
broadcast transactions. Keep your system clock accurate for header time checks.

wally verifies header linkage, difficulty, proof of work, and block transaction
merkle roots. Dogecoin checks include Scrypt and AuxPoW merged-mining proofs;
Bitcoin blocks also have their witness commitments checked. Bitcoin headers are
checked from genesis to the observed tip, enforcing a checkpoint at height 709631.
Dogecoin starts at the Dogecoin Core checkpoint at height 5050000 (January 2024);
payments before
that checkpoint cannot be recovered by this scanner.

After header synchronization, wally downloads full blocks starting at the
wallet's recorded birthday, which is creation time minus two days. This is not a
compact-filter client: initial synchronization can use substantial bandwidth and
CPU, especially for Dogecoin. Use the installed release binary, or
`cargo run --release -- ...`, rather than a debug build for normal use.

Headers and wallet scan progress are cached locally. Reorganizations trigger a
rescan from the wallet birthday while preserving signed outgoing payments.

## Fees

Fees use a fixed rate per 1000 serialized bytes, not a live fee estimate:

| Coin | Default rate | Minimum accepted rate |
| --- | --- | --- |
| Bitcoin | 0.0001 BTC | 0.00001 BTC |
| Dogecoin | 0.01 DOGE | 0.01 DOGE |

Override the rate in coin units with `--fee-per-kb`:

```sh
wally send bitcoin --to <bitcoin-address> --amount 0.001 --fee-per-kb 0.0002
```

Amounts accept at most eight decimal places. Transaction size is estimated
conservatively; change below the dust limit is added to the displayed fee.
The rate does not guarantee relay or a particular confirmation time.

## Storage and backup

Wallets are stored as `~/.wally/<name>.json`. Override this with
`wally --data-dir <directory> ...`. Each wallet has an independent random
secp256k1 key, encrypted using Argon2id and ChaCha20-Poly1305. The wallet file
format is version `1`, separate from the application's release version. New Unix
wallet directories and wallet/state/header files use permissions `700` and `600`;
protect existing directories and other platforms with filesystem permissions.
Existing wallet JSON files are never overwritten by `init`.

The same directory also contains:

- `<name>.state`: scan history, merkle proofs, and signed outgoing payments.
- `bitcoin.headers` / `dogecoin.headers`: shared chain caches.
- Coin lock files to prevent concurrent commands from changing the same cache.

**Back up wallet JSON files before funding, and keep passwords separately.**
Also back up `.state` files when present, especially after sending; take snapshots
with wally stopped. There is no seed phrase, password recovery, or remote backup.
Losing the JSON file or its password means losing access to the coins. Restore by
copying the saved wallet JSON and state files into the data directory; header
caches can be rebuilt. Do not manually edit wallet files.

The JSON file alone restores the key but not unconfirmed payment records. Losing
state while a payment is pending can lead to accidental duplicate payments if you
send again. Keep the data directory private: cached headers are trusted storage
and their proof of work is not repeated on reload.

## Deliberate limits

- One reused legacy P2PKH receiving address per wallet; no HD address rotation.
  Bitcoin recipients may also use P2SH or SegWit; Dogecoin recipients must use
  mainnet P2PKH or P2SH addresses.
- Only confirmed outputs are spent. Coinbase outputs require 100 confirmations
  for Bitcoin or 240 for Dogecoin. `balance` may include immature coinbase outputs
  and does not include unconfirmed incoming payments.
- A saved pending outgoing payment blocks new sends. wally does not track the
  entire mempool, including unconfirmed spends made by another app or device.
- Dust limits are 546 satoshis for Bitcoin and 0.01 DOGE for Dogecoin.
- This is an SPV-style wallet, not a full validating node. It trusts fixed
  checkpoints and the greatest-work chain observed from its peers; two different
  IPs do not eliminate eclipse attacks.

## Pending payments

The local transaction ID is printed **before** broadcasting, and the signed
transaction is saved before any network relay. Peer handoff does not guarantee
acceptance or confirmation. If relay fails or times out, check the transaction
ID before taking further action. Never blindly create another payment.

```sh
wally rebroadcast bitcoin
```

`rebroadcast` synchronizes and relays the exact saved pending transaction, without
creating a new payment or asking for the private key password. Use `balance` or
`sync` to update confirmation status. Signed outgoing records are retained after
confirmation so a reorganization can make the same payment pending again.

## Check

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Offline checks cover encryption and tampering, amounts, addresses, signing,
fees/change, cached merkle proofs, reorg payment recovery, difficulty/AuxPoW
vectors, and the wire protocol against local mock peers. No real funds are used.

Optional read-only mainnet checks are ignored by default:

```sh
cargo test public_ -- --ignored --nocapture
cargo test complete_bitcoin_header_sync -- --ignored --nocapture
```

These require internet. The full Bitcoin header check downloads chain data and
leaves a cache in a temporary directory. They do not broadcast mainnet payments;
full Dogecoin synchronization and live payment acceptance are not covered.
