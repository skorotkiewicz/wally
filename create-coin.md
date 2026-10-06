For a fresh build on Arch, keep compatible Boost in your home folder:

```bash
boost="$HOME/.local/opt/boost-1.89"
mkdir -p "$boost"

curl -fL https://archive.archlinux.org/packages/b/boost/boost-1.89.0-1-x86_64.pkg.tar.zst \
 -o "$boost/boost.pkg.tar.zst"

printf '%s  %s\n' \
 b55ad12fccbc0d0e9bf24be0c38b65f5ccca62cee29169ea4f0b9575b36f84ee \
 "$boost/boost.pkg.tar.zst" | sha256sum -c - &&
tar -xf "$boost/boost.pkg.tar.zst" -C "$boost"

export CMAKE_PREFIX_PATH="$boost/usr"
./create-coin.sh
 ```

After building:

 ```bash
cd wly-coin
./node -daemon
./wallet init --coin wly
 ```

Boost is needed only to build, not to run. Follow the generated README for the second node and mining.

Only regenerate if you want a new coin: another node must use the same generated project and genesis.
