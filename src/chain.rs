//! SPV header checks. Dogecoin rules follow Dogecoin Core v1.14.9
//! src/dogecoin.cpp and src/auxpow.cpp (MIT license). Not full block consensus.
use crate::wallet::Coin;
use anyhow::{Context, Result, ensure};
use bitcoin::{
    Block, BlockHash, CompactTarget, Target, Transaction, VarInt,
    block::Header,
    consensus::{self, Decodable},
    hashes::{Hash, sha256d},
};
use num_bigint::BigUint;

pub fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    ensure!(bytes.len() >= len, "Truncated peer payload");
    let (head, tail) = bytes.split_at(len);
    *bytes = tail;
    Ok(head)
}

pub fn decode<T: Decodable>(bytes: &mut &[u8]) -> Result<T> {
    let (value, len) = consensus::deserialize_partial(bytes)?;
    take(bytes, len)?;
    Ok(value)
}

fn branch(bytes: &mut &[u8]) -> Result<Vec<[u8; 32]>> {
    let count: VarInt = decode(bytes)?;
    ensure!(count.0 <= 30, "Oversized AuxPoW merkle branch");
    (0..count.0)
        .map(|_| Ok(take(bytes, 32)?.try_into()?))
        .collect()
}

fn merkle(mut hash: [u8; 32], branch: &[[u8; 32]], mut index: u32) -> Result<[u8; 32]> {
    ensure!(
        branch.len() <= 30 && (index as u64) < (1_u64 << branch.len()),
        "Invalid merkle index"
    );
    for sibling in branch {
        let mut pair = [0; 64];
        let (left, right) = if index & 1 == 0 {
            (&hash, sibling)
        } else {
            (sibling, &hash)
        };
        pair[..32].copy_from_slice(left);
        pair[32..].copy_from_slice(right);
        hash = sha256d::Hash::hash(&pair).to_byte_array();
        index >>= 1;
    }
    Ok(hash)
}

struct AuxPow {
    coinbase: Transaction,
    coinbase_branch: Vec<[u8; 32]>,
    coinbase_index: u32,
    chain_branch: Vec<[u8; 32]>,
    chain_index: u32,
    parent: Header,
}

impl AuxPow {
    fn read(bytes: &mut &[u8]) -> Result<Self> {
        let coinbase = decode(bytes)?;
        take(bytes, 32)?; // Legacy parent block hash; Core does not use this field.
        Ok(Self {
            coinbase,
            coinbase_branch: branch(bytes)?,
            coinbase_index: decode(bytes)?,
            chain_branch: branch(bytes)?,
            chain_index: decode(bytes)?,
            parent: decode(bytes)?,
        })
    }

    fn verify(&self, header: &Header) -> Result<()> {
        ensure!(self.coinbase_index == 0, "AuxPoW coinbase must be first");
        ensure!(
            (self.parent.version.to_consensus() as u32 >> 16) != 98,
            "AuxPoW parent has Dogecoin chain ID"
        );
        let parent_root = merkle(
            self.coinbase.compute_txid().to_byte_array(),
            &self.coinbase_branch,
            0,
        )?;
        ensure!(
            parent_root == self.parent.merkle_root.to_byte_array(),
            "Invalid AuxPoW coinbase proof"
        );
        let mut root = merkle(
            header.block_hash().to_byte_array(),
            &self.chain_branch,
            self.chain_index,
        )?;
        root.reverse();
        let script = self
            .coinbase
            .input
            .first()
            .context("Missing AuxPoW coinbase input")?
            .script_sig
            .as_bytes();
        let position = script
            .windows(32)
            .position(|window| window == root)
            .context("Missing merged-mining root")?;
        let tag = [0xfa, 0xbe, b'm', b'm'];
        let mut tags = script
            .windows(4)
            .enumerate()
            .filter(|(_, window)| *window == tag);
        if let Some((tag_position, _)) = tags.next() {
            ensure!(
                tags.next().is_none() && tag_position + 4 == position,
                "Invalid merged-mining marker"
            );
        } else {
            ensure!(position <= 20, "Merged-mining root too late in coinbase");
        }
        let suffix = script
            .get(position + 32..position + 40)
            .context("Missing AuxPoW size/nonce")?;
        let size = u32::from_le_bytes(suffix[..4].try_into()?);
        let nonce = u32::from_le_bytes(suffix[4..].try_into()?);
        ensure!(
            size == 1_u32 << self.chain_branch.len(),
            "Wrong AuxPoW tree size"
        );
        let expected = nonce
            .wrapping_mul(1103515245)
            .wrapping_add(12345)
            .wrapping_add(98)
            .wrapping_mul(1103515245)
            .wrapping_add(12345)
            % size;
        ensure!(expected == self.chain_index, "Wrong AuxPoW chain index");
        check_scrypt(&self.parent, header.target())
    }
}

fn check_scrypt(header: &Header, target: Target) -> Result<()> {
    let raw = consensus::serialize(header);
    let params = scrypt::Params::new(10, 1, 1, 32)
        .map_err(|error| anyhow::anyhow!("Invalid Scrypt parameters: {error}"))?;
    let mut hash = [0; 32];
    scrypt::scrypt(&raw, &raw, &params, &mut hash)
        .map_err(|error| anyhow::anyhow!("Scrypt hashing failed: {error}"))?;
    ensure!(
        target.is_met_by(BlockHash::from_byte_array(hash)),
        "Invalid Scrypt proof of work"
    );
    Ok(())
}

pub fn pow_limit(coin: Coin) -> Target {
    if coin == Coin::Bitcoin {
        return Target::MAX_ATTAINABLE_MAINNET;
    }
    let mut bytes = [0xff; 32];
    bytes[0] = 0;
    bytes[1] = 0;
    bytes[2] = 0x0f;
    Target::from_be_bytes(bytes)
}

pub fn read_header(bytes: &mut &[u8], coin: Coin, check_pow: bool) -> Result<Header> {
    let header: Header = decode(bytes)?;
    let auxiliary = if coin == Coin::Dogecoin && header.version.to_consensus() & 0x100 != 0 {
        Some(AuxPow::read(bytes)?)
    } else {
        None
    };
    if check_pow {
        ensure!(
            header.target() > Target::ZERO && header.target() <= pow_limit(coin),
            "Invalid proof-of-work target"
        );
        ensure!(
            header.bits == header.target().to_compact_lossy(),
            "Non-canonical difficulty encoding"
        );
        if coin == Coin::Bitcoin {
            header.validate_pow(header.target())?;
        } else {
            // This backend starts after modern Dogecoin rules activated at height 371337.
            ensure!(
                header.version.to_consensus() as u32 >> 16 == 98,
                "Wrong Dogecoin chain ID"
            );
            match auxiliary {
                Some(aux) => aux.verify(&header)?,
                None => check_scrypt(&header, header.target())?,
            }
        }
    }
    Ok(header)
}

pub fn read_block(raw: &[u8], coin: Coin, expected: BlockHash) -> Result<Block> {
    let mut bytes = raw;
    let header = read_header(&mut bytes, coin, false)?;
    ensure!(
        header.block_hash() == expected,
        "Peer returned a different block"
    );
    let txdata: Vec<Transaction> = decode(&mut bytes)?;
    ensure!(bytes.is_empty(), "Trailing block bytes");
    let block = Block { header, txdata };
    ensure!(block.check_merkle_root(), "Invalid transaction merkle root");
    ensure!(
        block.txdata.first().is_some_and(Transaction::is_coinbase),
        "Missing block coinbase"
    );
    if coin == Coin::Bitcoin {
        ensure!(
            block.check_witness_commitment(),
            "Invalid witness commitment"
        );
    }
    Ok(block)
}

pub fn next_bits(headers: &[Header], base: u32, coin: Coin) -> Result<CompactTarget> {
    let last = headers.last().context("Missing previous header")?;
    if coin == Coin::Bitcoin {
        let height = base + u32::try_from(headers.len())?;
        if !height.is_multiple_of(2016) {
            return Ok(last.bits);
        }
        let start = headers
            .get(
                headers
                    .len()
                    .checked_sub(2016)
                    .context("Missing retarget history")?,
            )
            .context("Missing retarget header")?;
        let timespan = (i64::from(last.time) - i64::from(start.time)).clamp(302400, 4838400);
        return Ok(CompactTarget::from_next_work_required(
            last.bits,
            timespan as u64,
            bitcoin::consensus::Params::MAINNET,
        ));
    }
    let previous = headers
        .get(
            headers
                .len()
                .checked_sub(2)
                .context("Missing DigiShield history")?,
        )
        .context("Missing previous Dogecoin header")?;
    let spacing = i64::from(last.time) - i64::from(previous.time);
    let modulated = (60 + (spacing - 60) / 8).clamp(45, 90) as u32;
    let target = BigUint::from_bytes_be(&last.target().to_be_bytes()) * modulated / 60_u32;
    let limit = BigUint::from_bytes_be(&pow_limit(coin).to_be_bytes());
    let bytes = target.min(limit).to_bytes_be();
    ensure!(bytes.len() <= 32, "Target overflow");
    let mut encoded = [0; 32];
    encoded[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(Target::from_be_bytes(encoded).to_compact_lossy())
}

pub fn append_header(
    headers: &mut Vec<Header>,
    base: u32,
    coin: Coin,
    header: Header,
    now: u64,
) -> Result<()> {
    let last = headers.last().context("Missing chain anchor")?;
    ensure!(
        header.prev_blockhash == last.block_hash(),
        "Headers do not link"
    );
    ensure!(
        header.bits == next_bits(headers, base, coin)?,
        "Incorrect network difficulty"
    );
    if headers.len() >= 11 {
        let mut times: Vec<_> = headers.iter().rev().take(11).map(|h| h.time).collect();
        times.sort_unstable();
        ensure!(header.time > times[5], "Header violates median time past");
    }
    ensure!(
        u64::from(header.time) <= now + 7200,
        "Header too far in the future; check your system clock"
    );
    headers.push(header);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitcoin_and_dogecoin_proofs_and_difficulty() -> Result<()> {
        let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
        let raw = consensus::serialize(&genesis);
        assert_eq!(
            read_block(&raw, Coin::Bitcoin, genesis.block_hash())?,
            genesis
        );
        read_header(&mut raw.as_slice(), Coin::Bitcoin, true)?;
        let mut corrupt = consensus::serialize(&genesis.header);
        corrupt[76] ^= 1;
        assert!(read_header(&mut corrupt.as_slice(), Coin::Bitcoin, true).is_err());
        assert!(read_block(&raw[..raw.len() - 1], Coin::Bitcoin, genesis.block_hash()).is_err());
        assert!(read_header(&mut raw.as_slice(), Coin::Dogecoin, true).is_err());

        // Public mainnet fixture from dogecoin 0.5.5 src/block.rs (MIT/Apache-2.0).
        let raw = hex::decode(include_str!("../tests/data/dogecoin-block.hex").trim())?;
        let mut bytes = raw.as_slice();
        let header = read_header(&mut bytes, Coin::Dogecoin, true)?;
        assert_eq!(
            header.block_hash().to_string(),
            "fb5f5b5b7d70e660c2c67bca8d3328afae32ae8bb4c8d6cbc42d96ff876b0859"
        );
        assert_eq!(
            read_block(&raw, Coin::Dogecoin, header.block_hash())?
                .txdata
                .len(),
            10
        );
        let mut bytes = &raw[80..];
        let mut auxiliary = AuxPow::read(&mut bytes)?;
        auxiliary.coinbase_branch[0][0] ^= 1;
        assert!(auxiliary.verify(&header).is_err());
        auxiliary.coinbase_branch[0][0] ^= 1;
        auxiliary.chain_index ^= 1;
        assert!(auxiliary.verify(&header).is_err());
        auxiliary.chain_index ^= 1;
        auxiliary.parent.nonce ^= 1;
        assert!(auxiliary.verify(&header).is_err());
        assert!(branch(&mut [31].as_slice()).is_err());
        assert!(merkle([0; 32], &[], 1).is_err());

        // Exact DigiShield vectors from Dogecoin Core v1.14.9 dogecoin_tests.cpp.
        for (previous_time, last_time, bits, expected) in [
            (1395094427, 1395094679, 0x1b499dfd, 0x1b671062),
            (1395100835, 1395101360, 0x1b3439cd, 0x1b4e56b3),
            (1395380517, 1395380447, 0x1b446f21, 0x1b335358),
            (1395094679, 1395094727, 0x1b671062, 0x1b6558a4),
        ] {
            let mut previous = header;
            previous.time = previous_time;
            let mut last = header;
            last.time = last_time;
            last.bits = CompactTarget::from_consensus(bits);
            assert_eq!(
                next_bits(&[previous, last], 5_050_000, Coin::Dogecoin)?.to_consensus(),
                expected
            );
        }
        let mut epoch = vec![genesis.header; 2016];
        epoch[2015].time = epoch[0].time + 604800;
        assert_eq!(
            next_bits(&epoch, 0, Coin::Bitcoin)?.to_consensus(),
            0x1c7fff80
        );
        let mut next = genesis.header;
        next.prev_blockhash = epoch[2015].block_hash();
        next.bits = CompactTarget::from_consensus(0x1d00ffff);
        assert!(
            append_header(
                &mut epoch,
                0,
                Coin::Bitcoin,
                next,
                u64::from(next.time) + 7200
            )
            .is_err()
        );
        Ok(())
    }
}
