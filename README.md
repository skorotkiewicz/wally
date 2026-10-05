# wally

A standalone Bitcoin and Dogecoin CLI wallet with direct peer-to-peer networking.
No Bitcoin Core, Dogecoin Core, or API service required. Mainnet only.

**Experimental SPV wallet, not a full validating node. Use small amounts.**

## Build and use

Building requires Rust and a C compiler.

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

Replace address placeholders with real recipient addresses. `init` asks for a
password of at least 12 characters. `send` shows the amount and fee, then asks for
confirmation and your password.

`init`, `ls`, and `receive` work offline. Other wallet operations need internet.
First synchronization can take a long time and use substantial bandwidth. Only
confirmed funds can be spent.

Fees are fixed, not live estimates: defaults are 0.0001 BTC or 0.01 DOGE per
1,000 bytes. Change the rate with `--fee-per-kb`, in coin units.

## Storage and backup

Wallets are encrypted and stored as `~/.wally/<name>.json`. Use
`--data-dir <directory>` to choose another folder.

Back up wallet JSON files before funding and their `<name>.state` files after
payments, with wally stopped. State files preserve pending payment records.
Keep passwords separately and the data directory private. Losing a wallet file
or its password means losing access to the coins. There is no password recovery.

## Pending payments

If broadcasting fails or times out, check the printed transaction ID before
sending again. Retry the exact saved payment with:

```sh
wally rebroadcast bitcoin
```

Peer handoff does not guarantee confirmation. Never create another payment just
to retry a pending one.
