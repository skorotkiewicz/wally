# wally

A standalone Bitcoin and Dogecoin CLI wallet. No Bitcoin Core, Dogecoin Core,
or other runtime apps required. Mainnet only.

## Build and use

```sh
cargo install --path .
wally init --coin bitcoin
wally init --coin dogecoin
wally init --coin bitcoin --name savings
wally ls
wally receive bitcoin
wally balance bitcoin
wally send bitcoin --to <bitcoin-address> --amount 0.001
wally send dogecoin --to <dogecoin-address> --amount 5
```

`init` prompts for a password of at least 12 characters and confirmation.
Wallet names default to `bitcoin` or `dogecoin`. `receive` prints the address
for the sender to pay; it does not move coins itself. `recive` is also accepted.
`send` displays the amount and estimated fee, asks you to type `send`, then asks
for your password. Keys are decrypted and transactions signed locally. No
passwords or private keys are sent to the API.

`init`, `ls`, and `receive` work offline. `balance` and `send` need internet and
use BlockCypher's HTTPS API. The provider sees your address and transactions;
wally relies on it for confirmations, balances, fee estimates and broadcasting.
Raw previous transactions are hash-checked before signing to verify input
amounts and ownership. Free API limits may reject requests, particularly sends
with many inputs. An optional `WALLY_API_TOKEN` environment variable supplies a
BlockCypher token. If the service requires a token, set it before sending.

## Storage and backup

Wallets are stored as `~/.wally/<name>.json`. Override this with
`wally --data-dir <directory> ...`. Each wallet has an independent random
secp256k1 key, encrypted using Argon2id and ChaCha20-Poly1305. New Unix directories
and files use permissions `700` and `600`; on other platforms protect the folder
with your account's filesystem permissions. Existing wallets are never overwritten.

**Back up every wallet file and keep its password separately before funding it.**
There is no seed phrase, password recovery, or remote backup. Restoring means
copying the encrypted JSON file into your wallet directory. Losing the file or
password means losing access to the coins. Do not manually edit wallet files.

## Deliberate limits

- One reused legacy P2PKH receiving address per wallet; no HD address rotation.
  Bitcoin recipients may also use P2SH or SegWit; Dogecoin recipients must use
  mainnet P2PKH or P2SH addresses.
- Only confirmed outputs are spent. Sending is refused while transactions for
  the address are pending, or when the API indicates an incomplete UTXO list.
- Conservative size-based fees and dust limits: 546 satoshis for Bitcoin and
  0.01 DOGE for Dogecoin. Smaller change is included in the displayed fee.
- This is an unaudited starter wallet. Test with small amounts, not life savings.

The local transaction ID is printed **before** broadcasting. If broadcasting
fails or times out, its status may be unknown. Check that transaction ID on a
block explorer before trying another send. Never blindly retry a payment.

## Check

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

The offline self-checks exercise exact decimal amounts, coin/address validation,
encryption and tamper rejection, transaction fees/change, local ECDSA signatures,
serialization, wallet-name validation, and previous-output verification. They do
not broadcast transactions or use real funds. Live mainnet broadcasting is not
covered by these checks.
