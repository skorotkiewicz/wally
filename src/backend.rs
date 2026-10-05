use crate::{
    chain,
    peer::{self, Peer},
    wallet::{Coin, Utxo, Wallet},
};
use anyhow::{Context, Result, bail, ensure};
use bitcoin::{
    BlockHash, MerkleBlock, OutPoint, ScriptBuf, Transaction, VarInt, Work, block::Header,
    consensus,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
};

// Dogecoin Core v1.14.9 mainnet checkpoint (January 2024).
const DOGE_HEIGHT: u32 = 5_050_000;
const DOGE_HASH: &str = "e7d4577405223918491477db725a393bcfc349d8ee63b0a4fde23cbfbfd81dea";
const HEADER_MAGIC: &[u8] = b"WALLYH01";

fn anchor(coin: Coin) -> Result<(u32, usize, BlockHash)> {
    Ok(match coin {
        Coin::Bitcoin => (
            0,
            0,
            bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).block_hash(),
        ),
        Coin::Dogecoin => (DOGE_HEIGHT - 10, 10, DOGE_HASH.parse()?),
    })
}

pub fn write_atomic(
    path: &Path,
    write: impl FnOnce(&mut BufWriter<fs::File>) -> Result<()>,
) -> Result<()> {
    let mut temporary_name = path.as_os_str().to_os_string();
    temporary_name.push(".tmp");
    let temporary = PathBuf::from(temporary_name);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
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
    total_work: Work,
}

impl Headers {
    fn tip(&self) -> u32 {
        self.base + self.entries.len() as u32 - 1
    }

    fn get(&self, height: u32) -> Result<&Header> {
        self.entries
            .get(
                height
                    .checked_sub(self.base)
                    .context("Height before checkpoint")? as usize,
            )
            .context("Unknown block height")
    }

    fn work_of(entries: &[Header]) -> Work {
        entries
            .iter()
            .fold(Work::from_be_bytes([0; 32]), |sum, header| {
                sum + header.work()
            })
    }

    fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, |writer| {
            writer.write_all(HEADER_MAGIC)?;
            for header in &self.entries {
                writer.write_all(&consensus::serialize(header))?;
            }
            Ok(())
        })
    }

    fn load(path: &Path, coin: Coin, peers: &mut [Peer]) -> Result<Self> {
        let (base, anchor, hash) = anchor(coin)?;
        let mut entries = Vec::new();
        if path.try_exists()? {
            // ponytail: trusted local cache skips repeated PoW; retain AuxPoW proofs and
            // reverify on reload if filesystem integrity cannot be trusted. Network PoW
            // is always checked before saving a header.
            // Difficulty, linkage and the fixed checkpoint are still checked on every reload.
            let file = fs::File::open(path)?;
            ensure!(
                file.metadata()?.len() <= 500_000_000,
                "Header cache exceeds the supported size"
            );
            let mut raw = Vec::new();
            file.take(500_000_001).read_to_end(&mut raw)?;
            ensure!(
                raw.starts_with(HEADER_MAGIC)
                    && (raw.len() - HEADER_MAGIC.len()).is_multiple_of(80),
                "Invalid header cache"
            );
            for raw in raw[HEADER_MAGIC.len()..].as_chunks::<80>().0 {
                entries.push(consensus::deserialize(raw)?);
            }
        } else if coin == Coin::Bitcoin {
            entries.push(
                bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            );
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
        ensure!(
            entries
                .get(anchor)
                .is_some_and(|h: &Header| h.block_hash() == hash),
            "Cache checkpoint mismatch"
        );
        for pair in entries[..=anchor].windows(2) {
            ensure!(
                pair[1].prev_blockhash == pair[0].block_hash(),
                "Checkpoint ancestry mismatch"
            );
        }
        let mut validated = entries[..=anchor].to_vec();
        let now = peer::now()?;
        for header in entries.into_iter().skip(anchor + 1) {
            chain::append_header(&mut validated, base, coin, header, now)?;
        }
        Ok(Self {
            base,
            anchor,
            total_work: Self::work_of(&validated[anchor..]),
            entries: validated,
        })
    }

    fn locator(&self) -> Vec<BlockHash> {
        let mut result = Vec::new();
        let mut index = self.entries.len() - 1;
        let mut step = 1;
        loop {
            result.push(self.entries[index].block_hash());
            if index <= self.anchor {
                break;
            }
            if result.len() > 10 {
                step *= 2;
            }
            index = index.saturating_sub(step).max(self.anchor);
        }
        result
    }

    fn sync_peer(&self, path: &Path, peer: &mut Peer, coin: Coin) -> Result<Self> {
        let mut candidate = Self {
            base: self.base,
            anchor: self.anchor,
            entries: self.entries.clone(),
            total_work: self.total_work,
        };
        let mut batches = 0_u32;
        loop {
            let old_tip = candidate
                .entries
                .last()
                .context("Empty header chain")?
                .block_hash();
            let response = peer.headers(candidate.locator())?;
            let mut bytes = response.as_slice();
            let count: VarInt = chain::decode(&mut bytes)?;
            ensure!(count.0 <= 2000, "Oversized header batch");
            if count.0 == 0 {
                ensure!(
                    bytes.is_empty() && candidate.tip() >= peer.height,
                    "Peer withheld headers or sent malformed headers"
                );
                if candidate.total_work > self.total_work {
                    candidate.save(path)?;
                }
                return Ok(candidate);
            }
            for index in 0..count.0 {
                let header = chain::read_header(&mut bytes, coin, true)?;
                let transaction_count: VarInt = chain::decode(&mut bytes)?;
                ensure!(
                    transaction_count.0 == 0,
                    "Nonzero transaction count in headers"
                );
                if index == 0 {
                    let parent = candidate
                        .entries
                        .iter()
                        .rposition(|h| h.block_hash() == header.prev_blockhash)
                        .context("Fork does not connect to the trusted checkpoint")?;
                    ensure!(parent >= self.anchor, "Fork predates checkpoint");
                    if parent + 1 != candidate.entries.len() {
                        candidate.entries.truncate(parent + 1);
                        candidate.total_work =
                            Self::work_of(&candidate.entries[candidate.anchor..]);
                    }
                }
                chain::append_header(
                    &mut candidate.entries,
                    candidate.base,
                    coin,
                    header,
                    peer::now()?,
                )?;
                candidate.total_work = candidate.total_work + header.work();
            }
            ensure!(bytes.is_empty(), "Trailing header bytes");
            ensure!(
                candidate
                    .entries
                    .last()
                    .context("Empty header chain")?
                    .block_hash()
                    != old_tip,
                "Peer repeated the same header batch"
            );
            ensure!(
                candidate.entries.len() <= 6_000_000,
                "Header chain exceeds the supported cache size"
            );
            batches += 1;
            if batches.is_multiple_of(25) && candidate.total_work > self.total_work {
                candidate.save(path)?;
            }
            eprintln!(
                "Verified {} headers through height {}",
                coin.name(),
                candidate.tip()
            );
        }
    }

    fn sync(
        &mut self,
        path: &Path,
        coin: Coin,
        peers: &mut Vec<Peer>,
        explicit: &[String],
    ) -> Result<()> {
        let mut accepted = Vec::new();
        let mut ips = HashSet::new();
        let mut errors = Vec::new();
        let mut candidates = std::mem::take(peers);
        for attempt in 0..3 {
            if attempt > 0 {
                match peer::discover(coin, explicit) {
                    Ok(found) => candidates = found,
                    Err(error) => {
                        errors.push(error.to_string());
                        continue;
                    }
                }
            }
            for mut peer in candidates.drain(..) {
                let ip = peer.address()?.ip();
                if ips.contains(&ip) {
                    continue;
                }
                match self.sync_peer(path, &mut peer, coin) {
                    Ok(candidate) => {
                        if candidate.total_work > self.total_work {
                            *self = candidate;
                        }
                        ips.insert(ip);
                        accepted.push(peer);
                        if accepted.len() == 2 {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("Replacing failed peer: {error:#}");
                        errors.push(error.to_string());
                        // Resume verified batches rather than repeating expensive Scrypt work.
                        if path.try_exists()? {
                            let cached = Self::load(path, coin, &mut [])?;
                            if cached.total_work > self.total_work {
                                *self = cached;
                            }
                        }
                    }
                }
            }
            if accepted.len() == 2 {
                break;
            }
        }
        ensure!(
            accepted.len() == 2,
            "Two-peer header synchronization failed: {}",
            errors.join("; ")
        );
        if coin == Coin::Bitcoin {
            ensure!(
                self.get(chain::BITCOIN_CHECKPOINT_HEIGHT)?
                    .block_hash()
                    .to_string()
                    == chain::BITCOIN_CHECKPOINT_HASH,
                "Chain has not reached the known Bitcoin checkpoint"
            );
        }
        ensure!(
            u64::from(self.entries.last().context("Empty header chain")?.time) + 6 * 3600
                >= peer::now()?,
            "Chain tip is stale; refusing to report a current balance or send"
        );
        *peers = accepted;
        self.save(path)
    }
}

fn fetch_block(peers: &mut [Peer], coin: Coin, hash: BlockHash) -> Result<Vec<u8>> {
    for peer in peers {
        if let Ok(raw) = peer.block(hash)
            && chain::read_block(&raw, coin, hash).is_ok()
        {
            return Ok(raw);
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
    outgoing: Vec<String>,
}

struct Output {
    value: u64,
    height: u32,
    coinbase: bool,
}

type Ledger = BTreeMap<OutPoint, Output>;

fn apply(ledger: &mut Ledger, own: &ScriptBuf, tx: &Transaction, height: u32) -> Result<bool> {
    let mut relevant = false;
    for input in &tx.input {
        relevant |= ledger.remove(&input.previous_output).is_some();
    }
    let txid = tx.compute_txid();
    for (index, output) in tx.output.iter().enumerate() {
        if output.script_pubkey == *own {
            let previous = ledger.insert(
                OutPoint {
                    txid,
                    vout: u32::try_from(index)?,
                },
                Output {
                    value: output.value.to_sat(),
                    height,
                    coinbase: tx.is_coinbase(),
                },
            );
            ensure!(previous.is_none(), "Duplicate wallet output");
            relevant = true;
        }
    }
    Ok(relevant)
}

impl State {
    fn pending_transactions(&self) -> Result<Vec<Transaction>> {
        let mut confirmed = HashSet::new();
        let mut spent = HashSet::new();
        for entry in &self.history {
            for raw in &entry.transactions {
                let tx: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
                confirmed.insert(tx.compute_txid());
                spent.extend(tx.input.iter().map(|input| input.previous_output));
            }
        }
        let mut pending = Vec::new();
        for raw in &self.outgoing {
            let tx: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
            if !confirmed.contains(&tx.compute_txid())
                && !tx
                    .input
                    .iter()
                    .any(|input| spent.contains(&input.previous_output))
            {
                pending.push(tx);
            }
        }
        Ok(pending)
    }

    fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, |writer| {
            serde_json::to_writer(writer, self)?;
            Ok(())
        })
    }

    fn ledger(&self, headers: &Headers, own: &ScriptBuf) -> Result<Ledger> {
        let mut ledger = Ledger::new();
        let mut last_height = 0;
        for entry in &self.history {
            ensure!(
                entry.height > last_height && entry.height <= self.height,
                "Out-of-order wallet history"
            );
            let proof: MerkleBlock = consensus::deserialize(&hex::decode(&entry.proof)?)?;
            ensure!(
                proof.header == *headers.get(entry.height)?,
                "Cached wallet proof is not in the best chain"
            );
            let mut matches = Vec::new();
            proof.extract_matches(&mut matches, &mut Vec::new())?;
            ensure!(
                matches.len() == entry.transactions.len(),
                "Wallet proof/transaction count mismatch"
            );
            for (txid, raw) in matches.iter().zip(&entry.transactions) {
                let tx: Transaction = consensus::deserialize(&hex::decode(raw)?)?;
                ensure!(
                    *txid == tx.compute_txid(),
                    "Cached transaction does not match its merkle proof"
                );
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
    state: State,
    state_path: PathBuf,
    ledger: Ledger,
}

impl Session {
    pub fn sync(dir: &Path, name: &str, wallet: &Wallet, explicit: &[String]) -> Result<Self> {
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{}.lock", wallet.coin.name())))?;
        lock.try_lock()
            .context("Another wally command is using this coin's chain cache")?;
        let mut peers = peer::discover(wallet.coin, explicit)?;
        let header_path = dir.join(format!("{}.headers", wallet.coin.name()));
        let mut headers = Headers::load(&header_path, wallet.coin, &mut peers)?;
        headers.sync(&header_path, wallet.coin, &mut peers, explicit)?;
        let own = wallet.coin.script(&wallet.address)?;
        let state_path = dir.join(format!("{name}.state"));
        let start = headers
            .entries
            .iter()
            .position(|h| u64::from(h.time) >= wallet.birthday)
            .unwrap_or(headers.entries.len())
            .max(headers.anchor + 1);
        let initial_height = headers.base + start as u32 - 1;
        let fresh = || -> Result<State> {
            Ok(State {
                version: 1,
                address: wallet.address.clone(),
                height: initial_height,
                hash: headers.get(initial_height)?.block_hash().to_string(),
                history: Vec::new(),
                outgoing: Vec::new(),
            })
        };
        let mut state: State = if state_path.try_exists()? {
            serde_json::from_reader(fs::File::open(&state_path)?.take(128 * 1024 * 1024))?
        } else {
            fresh()?
        };
        ensure!(
            state.version == 1 && state.address == wallet.address,
            "Wrong wallet scan cache"
        );
        // ponytail: reorgs rescan from the wallet birthday; add per-block undo records if
        // the bandwidth cost of rescanning becomes a problem. Pending payments survive.
        if headers
            .get(state.height)
            .map(|h| h.block_hash().to_string())
            .ok()
            .as_ref()
            != Some(&state.hash)
        {
            let outgoing = std::mem::take(&mut state.outgoing);
            state = fresh()?;
            state.outgoing = outgoing;
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
                let proof =
                    MerkleBlock::from_block_with_predicate(&block, |txid| matches.contains(txid));
                state.history.push(Entry {
                    height,
                    proof: hex::encode(consensus::serialize(&proof)),
                    transactions,
                });
            }
            state.height = height;
            state.hash = hash.to_string();
            if height % 100 == 0 {
                state.save(&state_path)?;
                eprintln!("Scanned wallet through height {height}");
            }
        }
        state.save(&state_path)?;
        Ok(Self {
            _lock: lock,
            peers,
            coin: wallet.coin,
            headers,
            state,
            state_path,
            ledger,
        })
    }

    pub fn balance(&self) -> Result<u64> {
        self.ledger.values().try_fold(0_u64, |sum, output| {
            sum.checked_add(output.value).context("Balance overflow")
        })
    }

    pub fn utxos(&self) -> Result<Vec<Utxo>> {
        ensure!(
            self.state.pending_transactions()?.is_empty(),
            "A payment is still pending. Use rebroadcast instead of creating another payment"
        );
        let maturity = if self.coin == Coin::Bitcoin { 100 } else { 240 };
        Ok(self
            .ledger
            .iter()
            .filter(|(_, output)| {
                !output.coinbase || self.headers.tip() + 1 - output.height >= maturity
            })
            .map(|(outpoint, output)| Utxo {
                outpoint: *outpoint,
                value: output.value,
            })
            .collect())
    }

    pub fn has_pending(&self) -> Result<bool> {
        Ok(!self.state.pending_transactions()?.is_empty())
    }

    pub fn broadcast(&mut self, tx: &Transaction) -> Result<()> {
        ensure!(!self.has_pending()?, "Another payment is pending");
        // Keep signed payments even after confirmation, so a reorg can resurrect the
        // same payment rather than allowing an accidental second payment.
        self.state
            .outgoing
            .push(hex::encode(consensus::serialize(tx)));
        self.state.save(&self.state_path)?;
        self.rebroadcast()
    }

    pub fn rebroadcast(&mut self) -> Result<()> {
        let pending = self.state.pending_transactions()?;
        ensure!(!pending.is_empty(), "No pending transaction to rebroadcast");
        for transaction in pending {
            let mut relayed = 0;
            for peer in &mut self.peers {
                if peer.broadcast(&transaction).is_ok() {
                    relayed += 1;
                }
            }
            ensure!(
                relayed > 0,
                "Broadcast status unknown for {}. The exact transaction is saved; use rebroadcast, not another send",
                transaction.compute_txid()
            );
            println!(
                "Transaction {} handed to {relayed} peer(s); acceptance and confirmation are not guaranteed.",
                transaction.compute_txid()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{Amount, TxIn, TxOut, Txid, absolute, hashes::Hash, transaction};

    fn entry(block: &bitcoin::Block, height: u32, tx: &Transaction) -> Entry {
        let proof = MerkleBlock::from_block_with_predicate(block, |id| *id == tx.compute_txid());
        Entry {
            height,
            proof: hex::encode(consensus::serialize(&proof)),
            transactions: vec![hex::encode(consensus::serialize(tx))],
        }
    }

    #[test]
    fn cached_proofs_spends_and_reorg_pending_recovery() -> Result<()> {
        let own = ScriptBuf::new_p2pkh(&bitcoin::PubkeyHash::from_byte_array([1; 20]));
        let funding = Transaction {
            version: transaction::Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::from_byte_array([1; 32]),
                    vout: 0,
                },
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(200_000),
                script_pubkey: own.clone(),
            }],
        };
        let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
        let mut block = bitcoin::Block {
            header: genesis.header,
            txdata: vec![genesis.txdata[0].clone(), funding.clone()],
        };
        block.header.prev_blockhash = genesis.block_hash();
        block.header.merkle_root = block.compute_merkle_root().context("Missing merkle root")?;
        let mut headers = Headers {
            base: 0,
            anchor: 0,
            entries: vec![genesis.header, block.header],
            total_work: Work::from_be_bytes([0; 32]),
        };
        let mut state = State {
            version: 1,
            address: "test".into(),
            height: 1,
            hash: block.block_hash().to_string(),
            history: vec![entry(&block, 1, &funding)],
            outgoing: Vec::new(),
        };
        let mut ledger = state.ledger(&headers, &own)?;
        assert_eq!(ledger.values().map(|o| o.value).sum::<u64>(), 200_000);
        let spend = Transaction {
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: funding.compute_txid(),
                    vout: 0,
                },
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: Amount::from_sat(199_000),
                script_pubkey: ScriptBuf::new(),
            }],
            ..funding.clone()
        };
        state
            .outgoing
            .push(hex::encode(consensus::serialize(&spend)));
        assert_eq!(state.pending_transactions()?.len(), 1);
        assert!(apply(&mut ledger, &own, &spend, 2)?);
        assert!(ledger.is_empty());
        let mut confirmed = bitcoin::Block {
            header: block.header,
            txdata: vec![genesis.txdata[0].clone(), spend.clone()],
        };
        confirmed.header.prev_blockhash = block.block_hash();
        confirmed.header.merkle_root = confirmed
            .compute_merkle_root()
            .context("Missing merkle root")?;
        headers.entries.push(confirmed.header);
        state.height = 2;
        state.history.push(entry(&confirmed, 2, &spend));
        assert!(state.pending_transactions()?.is_empty());
        assert!(state.ledger(&headers, &own)?.is_empty());
        // Remove the confirming block: the exact old payment becomes pending again.
        state.history.pop();
        state.height = 1;
        assert_eq!(state.pending_transactions()?, vec![spend]);
        let mut tampered = funding;
        tampered.output[0].value = Amount::from_sat(999_999);
        state.history[0].transactions[0] = hex::encode(consensus::serialize(&tampered));
        assert!(state.ledger(&headers, &own).is_err());
        assert_eq!(anchor(Coin::Dogecoin)?.2.to_string(), DOGE_HASH);
        Ok(())
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    #[ignore = "downloads and verifies all Bitcoin mainnet headers; no keys or payments"]
    fn complete_bitcoin_header_sync() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("wally-header-check-{}", std::process::id()));
        fs::create_dir_all(&directory)?;
        let path = directory.join("bitcoin.headers");
        let mut peers = peer::discover(Coin::Bitcoin, &[])?;
        let mut headers = Headers::load(&path, Coin::Bitcoin, &mut peers)?;
        headers.sync(&path, Coin::Bitcoin, &mut peers, &[])?;
        assert!(headers.tip() >= chain::BITCOIN_CHECKPOINT_HEIGHT);
        assert_eq!(peers.len(), 2);
        let restored = Headers::load(&path, Coin::Bitcoin, &mut [])?;
        assert_eq!(restored.entries, headers.entries);
        eprintln!(
            "Complete Bitcoin header sync and cache reload verified through {}",
            headers.tip()
        );
        Ok(())
    }
}
