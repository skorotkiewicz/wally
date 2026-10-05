use anyhow::{Context, Result, bail, ensure};
use bitcoin::{BlockHash, MerkleBlock, OutPoint, ScriptBuf, Transaction, Txid, VarInt, Work,
    block::Header, consensus, hashes::Hash};
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, HashSet}, fs, io::{BufWriter, Read, Write}, path::{Path, PathBuf}};
use crate::{chain, peer::{self, Peer}, wallet::{Coin, Utxo, Wallet}};

// Dogecoin Core v1.14.9 mainnet checkpoint (January 2024).
const DOGE_HEIGHT: u32 = 5_050_000;
const DOGE_HASH: &str = "e7d4577405223918491477db725a393bcfc349d8ee63b0a4fde23cbfbfd81dea";
const HEADER_MAGIC: &[u8] = b"WALLYH01";

fn anchor(coin: Coin) -> Result<(u32, usize, BlockHash)> {
    Ok(match coin {
        Coin::Bitcoin => (0, 0, bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).block_hash()),
        Coin::Dogecoin => (DOGE_HEIGHT - 10, 10, DOGE_HASH.parse()?),
    })
}

pub fn write_atomic(path: &Path, write: impl FnOnce(&mut BufWriter<fs::File>) -> Result<()>) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut writer = BufWriter::new(options.open(&temporary)?);
    write(&mut writer)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    drop(writer);
    fs::rename(&temporary, path)?;
    #[cfg(unix)]
    fs::File::open(path.parent().context("Missing storage directory")?)?.sync_all()?;
    Ok(())
}

struct Headers {
    base: u32,
    anchor: usize,
    entries: Vec<Header>,
}

impl Headers {
    fn tip(&self) -> u32 { self.base + self.entries.len() as u32 - 1 }

    fn get(&self, height: u32) -> Result<&Header> {
        self.entries.get(height.checked_sub(self.base).context("Height before checkpoint")? as usize)
            .context("Unknown block height")
    }

    fn work(&self) -> Work {
        self.entries[self.anchor..].iter().fold(Work::from_be_bytes([0; 32]), |sum, header| sum + header.work())
    }

    fn load(path: &Path, coin: Coin, peers: &mut [Peer]) -> Result<Self> {
        let (base, anchor, hash) = anchor(coin)?;
        let mut entries = Vec::new();
        if path.try_exists()? {
            // Local header cache is trusted storage: PoW was checked before each header was saved.
            // Difficulty, linkage and the fixed checkpoint are still checked on every reload.
            let file = fs::File::open(path)?;
            ensure!(file.metadata()?.len() <= 500_000_000, "Header cache exceeds the supported size");
            let mut raw = Vec::new();
            file.take(500_000_001).read_to_end(&mut raw)?;
            ensure!(raw.starts_with(HEADER_MAGIC) && (raw.len() - HEADER_MAGIC.len()) % 80 == 0, "Invalid header cache");
            for raw in raw[HEADER_MAGIC.len()..].chunks_exact(80) {
                entries.push(consensus::deserialize(raw)?);
            }
        } else if coin == Coin::Bitcoin {
            entries.push(bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header);
        } else {
            let mut requested = hash;
            for _ in 0..=anchor {
                let raw = fetch_block(peers, coin, requested)?;
                let block = chain::read_block(&raw, coin, requested)?;
                requested = block.header.prev_blockhash;
                entries.push(block.header);
            }
            entries.reverse();
        }
        ensure!(entries.get(anchor).is_some_and(|h: &Header| h.block_hash() == hash), "Cache checkpoint mismatch");
        for pair in entries[..=anchor].windows(2) {
            ensure!(pair[1].prev_blockhash == pair[0].block_hash(), "Checkpoint ancestry mismatch");
        }
        let mut validated = entries[..=anchor].to_vec();
        let now = peer::now()?;
        for header in entries.into_iter().skip(anchor + 1) {
            chain::append_header(&mut validated, base, coin, header, now)?;
        }
        Ok(Self { base, anchor, entries: validated })
    }

    fn locator(&self) -> Vec<BlockHash> {
        let mut result = Vec::new();
        let mut index = self.entries.len() - 1;
        let mut step = 1;
        loop {
            result.push(self.entries[index].block_hash());
            if index <= self.anchor { break; }
            if result.len() > 10 { step *= 2; }
            index = index.saturating_sub(step).max(self.anchor);
        }
        result
    }

    fn sync_peer(&self, peer: &mut Peer, coin: Coin) -> Result<Self> {
        let mut candidate = Self { base: self.base, anchor: self.anchor, entries: self.entries.clone() };
        let now = peer::now()?;
        loop {
            let response = peer.headers(candidate.locator())?;
            let mut bytes = response.as_slice();
            let count: VarInt = chain::decode(&mut bytes)?;
            ensure!(count.0 <= 2000, "Oversized header batch");
            if count.0 == 0 {
                ensure!(bytes.is_empty() && candidate.tip() >= peer.height, "Peer withheld headers or sent malformed headers");
                return Ok(candidate);
            }
            for index in 0..count.0 {
                let header = chain::read_header(&mut bytes, coin, true)?;
                let transaction_count: VarInt = chain::decode(&mut bytes)?;
                ensure!(transaction_count.0 == 0, "Nonzero transaction count in headers");
                if index == 0 {
                    let parent = candidate.entries.iter().rposition(|h| h.block_hash() == header.prev_blockhash)
                        .context("Fork does not connect to the trusted checkpoint")?;
                    ensure!(parent >= self.anchor, "Fork predates checkpoint");
                    candidate.entries.truncate(parent + 1);
                }
                chain::append_header(&mut candidate.entries, candidate.base, coin, header, now)?;
            }
            ensure!(bytes.is_empty(), "Trailing header bytes");
            eprintln!("Verified {} headers through height {}", coin.name(), candidate.tip());
        }
    }

    fn sync(&mut self, path: &Path, coin: Coin, peers: &mut [Peer]) -> Result<()> {
        let mut responses = 0;
        let mut errors = Vec::new();
        for peer in peers {
            match self.sync_peer(peer, coin) {
                Ok(candidate) => {
                    responses += 1;
                    if candidate.work() > self.work() { *self = candidate; }
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        ensure!(responses >= 2, "Two-peer header synchronization failed: {}", errors.join("; "));
        ensure!(u64::from(self.entries.last().context("Empty header chain")?.time) + 6 * 3600 >= peer::now()?,
            "Chain tip is stale; refusing to report a current balance or send");
        write_atomic(path, |writer| {
            writer.write_all(HEADER_MAGIC)?;
            for header in &self.entries { writer.write_all(&consensus::serialize(header))?; }
            Ok(())
        })
    }
}

fn fetch_block(peers: &mut [Peer], coin: Coin, hash: BlockHash) -> Result<Vec<u8>> {
    for peer in peers {
        if let Ok(raw) = peer.block(hash) {
            if chain::read_block(&raw, coin, hash).is_ok() { return Ok(raw); }
        }
    }
    bail!("No peer supplied a valid block {hash}")
}

#[derive(Serialize, Deserialize)]
struct Entry {
    height: u32,
    proof: String,
    transactions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u8,
    address: String,
    height: u32,
    hash: String,
    history: Vec<Entry>,
    pending: Option<String>,
}

struct Output {
    value: u64,
    height: u32,
    coinbase: bool,
}

type Ledger = BTreeMap<OutPoint, Output>;

fn apply(ledger: &mut Ledger, own: &ScriptBuf, tx: &Transaction, height: u32) -> Result<bool> {
    let mut relevant = false;
    for input in &tx.input { relevant |= ledger.remove(&input.previous_output).is_some(); }
    let txid = tx.compute_txid();
    for (index, output) in tx.output.iter().enumerate() {
        if output.script_pubkey == *own {
            let previous = ledger.insert(OutPoint { txid, vout: u32::try_from(index)? }, Output {
                value: output.value.to_sat(), height, coinbase: tx.is_coinbase(),
            });
            ensure!(previous.is_none(), "Duplicate wallet output");
            relevant = true;
        }
    }
    Ok(relevant)
}

impl State {
    fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, |writer| { serde_json::to_writer(writer, self)?; Ok(()) })
    }

    fn ledger(&self, headers: &Headers, own: &ScriptBuf) -> Result<Ledger> {
        let mut ledger = Ledger::new();
        let mut last_height = 0;
        for entry in &self.history {
            ensure!(entry.height > last_height && entry.height <= self.height, "Out-of-order wallet history");
            let proof: MerkleBlock = consensus::deserialize(&hex::decode(&entry.proof)?)?;
            ensure!(proof.header == *headers.get(entry.height)?, "Cached wallet proof is not in the best chain");
            let mut matches = Vec::new();
            proof.extract_matches(&mut matches, &mut Vec::new())?;
            ensure!(matches.len() == entry.transactions.len(), "Wallet proof/transaction count mismatch");
            for (txid, raw) in matches.iter().zip(&entry.transactions) {
                let tx: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
                ensure!(*txid == tx.compute_txid(), "Cached transaction does not match its merkle proof");
                apply(&mut ledger, own, &tx, entry.height)?;
            }
            last_height = entry.height;
        }
        Ok(ledger)
    }
}

pub struct Session {
    _lock: fs::File,
    peers: Vec<Peer>,
    coin: Coin,
    headers: Headers,
    own: ScriptBuf,
    state: State,
    state_path: PathBuf,
    ledger: Ledger,
}

impl Session {
    pub fn sync(dir: &Path, name: &str, wallet: &Wallet, explicit: &[String]) -> Result<Self> {
        let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .open(dir.join(format!("{}.lock", wallet.coin.name())))?;
        lock.try_lock().context("Another wally command is using this coin's chain cache")?;
        let mut peers = peer::discover(wallet.coin, explicit)?;
        let header_path = dir.join(format!("{}.headers", wallet.coin.name()));
        let mut headers = Headers::load(&header_path, wallet.coin, &mut peers)?;
        headers.sync(&header_path, wallet.coin, &mut peers)?;
        let own = wallet.coin.script(&wallet.address)?;
        let state_path = dir.join(format!("{name}.state"));
        let start = headers.entries.iter().position(|h| u64::from(h.time) >= wallet.birthday)
            .unwrap_or(headers.entries.len()).max(headers.anchor + 1);
        let initial_height = headers.base + start as u32 - 1;
        let fresh = || -> Result<State> { Ok(State {
            version: 1, address: wallet.address.clone(), height: initial_height,
            hash: headers.get(initial_height)?.block_hash().to_string(), history: Vec::new(), pending: None,
        }) };
        let mut state: State = if state_path.try_exists()? {
            serde_json::from_reader(fs::File::open(&state_path)?.take(128 * 1024 * 1024))?
        } else { fresh()? };
        ensure!(state.version == 1 && state.address == wallet.address, "Wrong wallet scan cache");
        // ponytail: reorgs rescan from the wallet birthday; add per-block undo records if
        // the bandwidth cost of rescanning becomes a problem. Pending payments survive.
        if headers.get(state.height).map(|h| h.block_hash().to_string()).ok().as_ref() != Some(&state.hash) {
            let pending = state.pending.take();
            state = fresh()?;
            state.pending = pending;
            eprintln!("Chain reorganization detected; rescanning wallet history");
        }
        let mut ledger = state.ledger(&headers, &own)?;
        for height in state.height + 1..=headers.tip() {
            let hash = headers.get(height)?.block_hash();
            let raw = fetch_block(&mut peers, wallet.coin, hash)?;
            let block = chain::read_block(&raw, wallet.coin, hash)?;
            let mut matches = HashSet::new();
            let mut transactions = Vec::new();
            for tx in &block.txdata {
                if apply(&mut ledger, &own, tx, height)? {
                    matches.insert(tx.compute_txid());
                    transactions.push(hex::encode(consensus::serialize(tx)));
                }
            }
            if !matches.is_empty() {
                let proof = MerkleBlock::from_block_with_predicate(&block, |txid| matches.contains(txid));
                state.history.push(Entry { height, proof: hex::encode(consensus::serialize(&proof)), transactions });
            }
            if let Some(raw) = &state.pending {
                let pending: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
                if block.txdata.iter().any(|tx| tx.compute_txid() == pending.compute_txid()) {
                    eprintln!("Pending payment {} confirmed", pending.compute_txid());
                    state.pending = None;
                } else if block.txdata.iter().any(|tx| tx.input.iter().any(|input|
                    pending.input.iter().any(|p| p.previous_output == input.previous_output))) {
                    eprintln!("Pending payment {} conflicts with a confirmed spend; it was not retried", pending.compute_txid());
                    state.pending = None;
                }
            }
            state.height = height;
            state.hash = hash.to_string();
            if height % 100 == 0 { state.save(&state_path)?; eprintln!("Scanned wallet through height {height}"); }
        }
        state.save(&state_path)?;
        Ok(Self { _lock: lock, peers, coin: wallet.coin, headers, own, state, state_path, ledger })
    }

    pub fn balance(&self) -> Result<u64> {
        self.ledger.values().try_fold(0_u64, |sum, output| sum.checked_add(output.value).context("Balance overflow"))
    }

    pub fn utxos(&self) -> Result<Vec<Utxo>> {
        ensure!(self.state.pending.is_none(), "A payment is still pending. Use rebroadcast instead of creating another payment");
        let maturity = if self.coin == Coin::Bitcoin { 100 } else { 240 };
        Ok(self.ledger.iter().filter(|(_, output)| !output.coinbase || self.headers.tip() + 1 - output.height >= maturity)
            .map(|(outpoint, output)| Utxo { outpoint: *outpoint, value: output.value }).collect())
    }

    pub fn has_pending(&self) -> bool { self.state.pending.is_some() }

    pub fn broadcast(&mut self, tx: &Transaction) -> Result<()> {
        ensure!(self.state.pending.is_none(), "Another payment is pending");
        // Durably record before touching the network. A crash or timeout never triggers a new payment.
        self.state.pending = Some(hex::encode(consensus::serialize(tx)));
        self.state.save(&self.state_path)?;
        self.rebroadcast()
    }

    pub fn rebroadcast(&mut self) -> Result<()> {
        let raw = self.state.pending.as_ref().context("No pending transaction to rebroadcast")?;
        let transaction: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
        let mut relayed = 0;
        for peer in &mut self.peers {
            if peer.broadcast(&transaction).is_ok() { relayed += 1; }
        }
        ensure!(relayed > 0, "Broadcast status unknown for {}. The exact transaction is saved; use rebroadcast, not another send", transaction.compute_txid());
        println!("Transaction {} handed to {relayed} peer(s); acceptance and confirmation are not guaranteed.", transaction.compute_txid());
        Ok(())
    }
}
