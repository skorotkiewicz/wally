#!/usr/bin/env bash
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
commit=d0f6d9953a15d7c7111d46dcb76ab2bb18e5dee3
source="$root/core-src"
if [[ ! -e "$source" ]]; then
    git clone --no-hardlinks --depth 1 --branch v30.0 --single-branch \
        "${BITCOIN_SOURCE:-https://github.com/bitcoin/bitcoin.git}" "$source"
fi
[[ -d "$source/.git" && $(git -C "$source" rev-parse HEAD) == "$commit" ]] || {
    echo 'Expected the pinned Bitcoin Core v30.0 checkout in core-src; refusing to modify it.' >&2
    exit 1
}
if ! git -C "$source" apply --reverse --check "$root/bitcoin-core.patch" 2>/dev/null; then
    git -C "$source" apply --check "$root/bitcoin-core.patch"
    git -C "$source" apply "$root/bitcoin-core.patch"
fi
cmake -S "$source" -B "$root/core-build" -DCMAKE_BUILD_TYPE=Release \
    -DENABLE_WALLET=OFF -DBUILD_TESTS=OFF -DBUILD_GUI=OFF -DENABLE_IPC=OFF \
    -DBUILD_BITCOIN_BIN=OFF -DWITH_CCACHE=OFF
cmake --build "$root/core-build" --target bitcoind bitcoin-cli -j "${JOBS:-2}"
export CARGO_TARGET_DIR="$root/wallet-src/target"
cargo fmt --manifest-path "$root/wallet-src/Cargo.toml"
cargo test --locked --manifest-path "$root/wallet-src/Cargo.toml"
cargo build --release --locked --manifest-path "$root/wallet-src/Cargo.toml"
mkdir -p "$root/bin"
cp "$root/core-build/bin/bitcoind" "$root/bin/wly-node"
cp "$root/core-build/bin/bitcoin-cli" "$root/bin/wly-cli"
cp "$root/wallet-src/target/release/wly-wallet" "$root/bin/wly-wallet"
