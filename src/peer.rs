use crate::wallet::Coin;
use anyhow::{Context, Result, bail, ensure};
use bitcoin::{
    BlockHash, Transaction, consensus,
    hashes::{Hash, sha256d},
    p2p::{
        ServiceFlags,
        address::Address,
        message_blockdata::{GetHeadersMessage, Inventory},
        message_network::VersionMessage,
    },
};
use rand::{RngCore, seq::SliceRandom};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub fn magic(coin: Coin) -> [u8; 4] {
    match coin {
        Coin::Bitcoin => [0xf9, 0xbe, 0xb4, 0xd9],
        Coin::Dogecoin => [0xc0; 4],
    }
}

pub struct Peer {
    stream: TcpStream,
    coin: Coin,
    pub height: u32,
}

fn read_until(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("Peer response timed out")?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(bytes)?;
        ensure!(count != 0, "Peer disconnected");
        bytes = &mut bytes[count..];
    }
    Ok(())
}

fn frame(coin: Coin, command: &str, payload: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        !command.is_empty()
            && command.len() <= 12
            && command
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
        "Invalid message command"
    );
    ensure!(payload.len() <= MAX_MESSAGE, "Oversized peer message");
    let mut header = [0; 24];
    header[..4].copy_from_slice(&magic(coin));
    header[4..4 + command.len()].copy_from_slice(command.as_bytes());
    header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[20..].copy_from_slice(&sha256d::Hash::hash(payload).as_byte_array()[..4]);
    let mut result = header.to_vec();
    result.extend_from_slice(payload);
    Ok(result)
}

impl Peer {
    pub fn connect(address: SocketAddr, coin: Coin) -> Result<Self> {
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        stream.set_write_timeout(Some(Duration::from_secs(15)))?;
        let mut peer = Self {
            stream,
            coin,
            height: 0,
        };
        let none = ServiceFlags::NONE;
        let mut version = VersionMessage::new(
            none,
            now()? as i64,
            Address::new(&address, none),
            Address::new(&"0.0.0.0:0".parse()?, none),
            rand::rngs::OsRng.next_u64(),
            "/wally:0.1.0/".to_owned(),
            0,
        );
        version.version = 70015;
        peer.send("version", &consensus::serialize(&version))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got_version = false;
        let mut got_verack = false;
        while !got_version || !got_verack {
            let (command, payload) = peer.receive(deadline)?;
            match command.as_str() {
                "version" => {
                    ensure!(!got_version, "Duplicate peer version");
                    let version: VersionMessage = consensus::deserialize(&payload)?;
                    ensure!(
                        version.version >= 70001 && version.start_height >= 0,
                        "Unsupported peer version/height"
                    );
                    ensure!(
                        version.services.has(ServiceFlags::NETWORK),
                        "Peer does not serve historical blocks"
                    );
                    if coin == Coin::Bitcoin {
                        ensure!(
                            version.services.has(ServiceFlags::WITNESS),
                            "Bitcoin peer does not serve witness blocks"
                        );
                    }
                    peer.height = version.start_height as u32;
                    peer.send("verack", &[])?;
                    got_version = true;
                }
                "verack" => {
                    ensure!(payload.is_empty(), "Invalid verack");
                    got_verack = true;
                }
                _ => {}
            }
        }
        Ok(peer)
    }

    fn send(&mut self, command: &str, payload: &[u8]) -> Result<()> {
        self.stream
            .write_all(&frame(self.coin, command, payload)?)?;
        Ok(())
    }

    fn receive(&mut self, deadline: Instant) -> Result<(String, Vec<u8>)> {
        let mut header = [0; 24];
        read_until(&mut self.stream, &mut header, deadline)?;
        ensure!(header[..4] == magic(self.coin), "Wrong network magic");
        let command_len = header[4..16].iter().position(|b| *b == 0).unwrap_or(12);
        ensure!(
            header[4 + command_len..16].iter().all(|b| *b == 0),
            "Invalid command padding"
        );
        let command = std::str::from_utf8(&header[4..4 + command_len])?.to_owned();
        ensure!(
            !command.is_empty()
                && command
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "Invalid peer command"
        );
        let len = u32::from_le_bytes(header[16..20].try_into()?) as usize;
        ensure!(len <= MAX_MESSAGE, "Oversized peer payload");
        let mut payload = vec![0; len];
        read_until(&mut self.stream, &mut payload, deadline)?;
        ensure!(
            header[20..24] == sha256d::Hash::hash(&payload).as_byte_array()[..4],
            "Peer checksum mismatch"
        );
        if command == "ping" {
            ensure!(payload.len() == 8, "Invalid ping");
            self.send("pong", &payload)?;
        }
        Ok((command, payload))
    }

    fn wait_for(&mut self, expected: &str) -> Result<Vec<u8>> {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        for _ in 0..1000 {
            let (command, payload) = self.receive(deadline)?;
            if command == expected {
                return Ok(payload);
            }
            ensure!(
                command != "notfound" && command != "reject",
                "Peer rejected or cannot serve the request"
            );
        }
        bail!("Too many unrelated peer messages")
    }

    pub fn headers(&mut self, locator: Vec<BlockHash>) -> Result<Vec<u8>> {
        let mut request = GetHeadersMessage::new(locator, BlockHash::all_zeros());
        request.version = 70015;
        self.send("getheaders", &consensus::serialize(&request))?;
        self.wait_for("headers")
    }

    pub fn block(&mut self, hash: BlockHash) -> Result<Vec<u8>> {
        let inventory = if self.coin == Coin::Bitcoin {
            Inventory::WitnessBlock(hash)
        } else {
            Inventory::Block(hash)
        };
        self.send("getdata", &consensus::serialize(&vec![inventory]))?;
        self.wait_for("block")
    }

    pub fn broadcast(&mut self, transaction: &Transaction) -> Result<()> {
        let txid = transaction.compute_txid();
        self.send(
            "inv",
            &consensus::serialize(&vec![Inventory::Transaction(txid)]),
        )?;
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        let nonce = rand::rngs::OsRng.next_u64().to_le_bytes();
        let mut sent = false;
        for _ in 0..1000 {
            let (command, payload) = self.receive(deadline)?;
            if command == "getdata" {
                let inventory: Vec<Inventory> = consensus::deserialize(&payload)?;
                if inventory.iter().any(|item| {
                    matches!(item,
                    Inventory::Transaction(id) | Inventory::WitnessTransaction(id) if *id == txid)
                }) {
                    self.send("tx", &consensus::serialize(transaction))?;
                    self.send("ping", &nonce)?;
                    sent = true;
                }
            } else if command == "pong" && sent && payload == nonce {
                // A pong is a transport round-trip, not a transaction acceptance receipt.
                return Ok(());
            } else if command == "reject" {
                bail!("Peer rejected the transaction");
            }
        }
        bail!("Peer did not request/receive the advertised transaction")
    }
}

pub fn discover(coin: Coin, explicit: &[String]) -> Result<Vec<Peer>> {
    let (port, seeds): (u16, &[&str]) = match coin {
        Coin::Bitcoin => (
            8333,
            &[
                "seed.bitcoin.sipa.be",
                "dnsseed.bluematt.me",
                "seed.bitcoinstats.com",
            ],
        ),
        Coin::Dogecoin => (
            22556,
            &[
                "seed.multidoge.org",
                "seed2.multidoge.org",
                "seed.dogecoin.com",
            ],
        ),
    };
    let mut addresses = Vec::new();
    if explicit.is_empty() {
        for seed in seeds {
            if let Ok(found) = (*seed, port).to_socket_addrs() {
                addresses.extend(found);
            }
        }
    } else {
        for host in explicit {
            addresses.extend(
                host.to_socket_addrs()
                    .with_context(|| format!("Cannot resolve peer {host}"))?,
            );
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses.shuffle(&mut rand::thread_rng());
    let mut peers: Vec<Peer> = Vec::new();
    let mut ips = std::collections::HashSet::new();
    for address in addresses.into_iter().take(64) {
        if ips.contains(&address.ip()) {
            continue;
        }
        if let Ok(peer) = Peer::connect(address, coin) {
            eprintln!("Connected to {address} (height {})", peer.height);
            ips.insert(address.ip());
            peers.push(peer);
            if peers.len() == 2 {
                return Ok(peers);
            }
        }
    }
    bail!(
        "Could not connect to two distinct {} peers. Check networking or supply --peer HOST:PORT twice",
        coin.name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn local_peer_handshake_requests_and_relay() -> Result<()> {
        for coin in [Coin::Bitcoin, Coin::Dogecoin] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
            let expected = genesis.clone();
            let server = std::thread::spawn(move || -> Result<()> {
                let (stream, address) = listener.accept()?;
                stream.set_write_timeout(Some(Duration::from_secs(5)))?;
                let mut peer = Peer {
                    stream,
                    coin,
                    height: 0,
                };
                let deadline = Instant::now() + Duration::from_secs(10);
                let (command, payload) = peer.receive(deadline)?;
                assert_eq!(command, "version");
                let mut version: VersionMessage = consensus::deserialize(&payload)?;
                version.services = ServiceFlags::NETWORK | ServiceFlags::WITNESS;
                version.receiver = Address::new(&address, version.services);
                peer.send("version", &consensus::serialize(&version))?;
                peer.send("verack", &[])?;
                assert_eq!(peer.receive(deadline)?.0, "verack");
                let (command, payload) = peer.receive(deadline)?;
                assert_eq!(command, "getheaders");
                let request: GetHeadersMessage = consensus::deserialize(&payload)?;
                assert_eq!(request.locator_hashes, vec![expected.block_hash()]);
                peer.send("headers", &[0])?;
                let (command, payload) = peer.receive(deadline)?;
                assert_eq!(command, "getdata");
                let request: Vec<Inventory> = consensus::deserialize(&payload)?;
                assert_eq!(
                    request[0].network_hash(),
                    Some(expected.block_hash().to_byte_array())
                );
                peer.send("block", &consensus::serialize(&expected))?;
                let (command, payload) = peer.receive(deadline)?;
                assert_eq!(command, "inv");
                peer.send("getdata", &payload)?;
                let (command, payload) = peer.receive(deadline)?;
                assert_eq!(command, "tx");
                assert_eq!(
                    consensus::deserialize::<Transaction>(&payload)?,
                    expected.txdata[0]
                );
                assert_eq!(peer.receive(deadline)?.0, "ping"); // receive automatically sends pong
                Ok(())
            });
            let mut peer = Peer::connect(address, coin)?;
            assert_eq!(peer.headers(vec![genesis.block_hash()])?, vec![0]);
            assert_eq!(
                peer.block(genesis.block_hash())?,
                consensus::serialize(&genesis)
            );
            peer.broadcast(&genesis.txdata[0])?;
            server.join().expect("local peer panicked")?;
        }
        Ok(())
    }

    #[test]
    fn malformed_messages_are_rejected_before_allocation() -> Result<()> {
        for damage in 0..4 {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let stream = TcpStream::connect(listener.local_addr()?)?;
            let (mut sender, _) = listener.accept()?;
            let mut payload = frame(Coin::Bitcoin, "verack", &[])?;
            match damage {
                0 => payload[0] ^= 1,
                1 => payload[20] ^= 1,
                2 => payload[16..20].copy_from_slice(&(MAX_MESSAGE as u32 + 1).to_le_bytes()),
                _ => {
                    payload[4] = b'!';
                }
            }
            sender.write_all(&payload)?;
            let mut peer = Peer {
                stream,
                coin: Coin::Bitcoin,
                height: 0,
            };
            assert!(
                peer.receive(Instant::now() + Duration::from_secs(2))
                    .is_err()
            );
        }
        assert!(frame(Coin::Bitcoin, "too_long_a_command", &[]).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::chain;
    use bitcoin::VarInt;

    #[test]
    #[ignore = "read-only public mainnet connectivity check"]
    fn public_mainnet_headers_and_blocks() -> Result<()> {
        for coin in [Coin::Bitcoin, Coin::Dogecoin] {
            let mut peers = discover(coin, &[])?;
            let hash = match coin {
                Coin::Bitcoin => {
                    bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin)
                        .block_hash()
                }
                Coin::Dogecoin => {
                    "e7d4577405223918491477db725a393bcfc349d8ee63b0a4fde23cbfbfd81dea".parse()?
                }
            };
            for peer in &mut peers {
                let raw = peer.block(hash)?;
                let block = chain::read_block(&raw, coin, hash)?;
                let response = peer.headers(vec![hash])?;
                let mut bytes = response.as_slice();
                let count: VarInt = chain::decode(&mut bytes)?;
                ensure!(
                    count.0 > 0 && count.0 <= 2000,
                    "Unexpected mainnet header count"
                );
                let next = chain::read_header(&mut bytes, coin, true)?;
                ensure!(
                    next.prev_blockhash == hash,
                    "Mainnet headers do not link to anchor"
                );
                if coin == Coin::Dogecoin {
                    let previous = peer.block(block.header.prev_blockhash)?;
                    let previous = chain::read_block(&previous, coin, block.header.prev_blockhash)?;
                    ensure!(
                        next.bits
                            == chain::next_bits(&[previous.header, block.header], 5_049_999, coin)?,
                        "Live DigiShield target mismatch"
                    );
                }
                eprintln!(
                    "Read-only {} checkpoint and first child header verified",
                    coin.name()
                );
            }
        }
        Ok(())
    }
}
