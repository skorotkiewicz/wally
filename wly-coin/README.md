# Wally (WLY)

Development-only SHA256d PoW chain. Easy initial difficulty, retarget every 2016
blocks. Not a secure public currency. No premine; genesis output is unspendable.
Reward 50 WLY; halving every 210000 blocks; target 60 seconds.
Coinbase maturity is 100 blocks. Money-range ceiling: 2099999997690000 satoshis.
Consensus comes from the pinned Bitcoin Core v30.0 fork; review patches and
maintain upstream security updates before considering a public launch.

Build: `./build.sh` (Rust 1.89+, CMake 3.22+, C++20, Boost 1.73-1.89 headers,
and libevent 2.1.8+ development headers).
`build.sh` clones Bitcoin Core v30.0 at commit
`d0f6d9953a15d7c7111d46dcb76ab2bb18e5dee3` into ignored `core-src/`,
then applies `bitcoin-core.patch` without reapplying an already applied patch.
It builds into ignored `core-build/` and creates `bin/` if needed.
Your existing `node-src/`, `node-build/`, chain data and wallets stay untouched.
The patch retains this WLY genesis and network parameters. Do not regenerate the coin.
The cloned source retains Bitcoin Core's MIT license in core-src/COPYING.
Run `python3 test_build.py` to check the clone/patch flow with stubbed build tools.
Building does not start nodes, mine coins, or open ports. You start those explicitly.
Builds may download source/dependencies, never wallet-service API data.
Set BITCOIN_SOURCE to a local Bitcoin Core repository with the v30.0 tag and
pinned commit, and use cached Cargo dependencies to build offline.

```sh
./node -daemon
./wallet init --coin wly
./wallet receive wly
./mine <receiving-address> 101
./rpc getblockchaininfo
```

The wallet still requires two distinct peer IPs. Run another instance of the
SAME generated node, not the generator again (that would create another genesis):

```sh
mkdir data2
./bin/wly-node -datadir="$PWD/data2" -port=19335 -rpcport=19337 \
  -bind=127.0.0.2:19335 -connect=127.0.0.1:19335 -listen=1 -dnsseed=0 -fixedseeds=0 -daemon
./wallet --peer 127.0.0.1:19335 --peer 127.0.0.2:19335 balance wly
```

RPC is localhost/cookie-authenticated. The default node binds P2P to localhost;
remote peers require explicitly binding a reachable interface and firewalling RPC.
Distribute this generated project's sources/coin.json to run the same network.
Custom wallet destinations use Base58 P2PKH/P2SH, not Bech32. Wallet signing,
encryption, progress bars, scan recovery and pending rebroadcast stay native.
Keep wallet JSON/passwords and pending state backed up. Original Wally is unchanged.
